# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  C-FIND handler: dispatches to Patient/Study Root or MWL.

CRIT-002: extracted common _handle_qido_search() to eliminate ~60 lines of duplication.
"""

import logging
from urllib.parse import quote

import httpx

from clean_code.http_helpers import (
    QidoMalformedBodyError,
    _http_status_to_dimse,
    _qido_request,
    service_base_url,
)
from clean_code.metrics import find_responses
from clean_code.protocol import (
    MWL_UID,
    PATIENT_ROOT_UID,
    STUDY_ROOT_UID,
    _build_mwl_params,
    _build_qido_params,
    _extract_mwl_request_keywords,
    _extract_request_keywords,
    _json_first_value,
    _json_to_dataset,
    _json_to_dataset_mwl,
    _json_to_dataset_patient,
    _json_to_dataset_series,
    _split_qido_params,
)
from clean_code.utils import _shutdown_requested


# ---------------------------------------------------------------------------
# Bridge: C-FIND handler with response limit + timeout
# ---------------------------------------------------------------------------
def _stamp_cfind_response(event, row_ds):
    """Echo (0008,0052) QueryRetrieveLevel and add (0008,0054)
    RetrieveAETitle on every C-FIND response row (01.09 Orthanc-control
    finding: MicroDicom's series panel keys rows by the RESPONSE
    QueryRetrieveLevel; Orthanc puts both on every row and the panel
    renders there). Spec-legal: PS3.4 C.4.1 - the response identifier
    shall contain the attributes of the request identifier; the SCP's
    AE title is a standard C-FIND response attribute.
    """
    level = (
        str(getattr(event.identifier, "QueryRetrieveLevel", "") or "").strip().upper()
        or "STUDY"
    )
    if "QueryRetrieveLevel" not in row_ds:
        row_ds.QueryRetrieveLevel = level
    bridge_ae = (
        getattr(
            getattr(getattr(event, "assoc", None), "acceptor", None), "ae_title", ""
        )
        or ""
    )
    if bridge_ae and "RetrieveAETitle" not in row_ds:
        row_ds.RetrieveAETitle = bridge_ae


def _cfind_shape(ds):
    """Privacy-safe C-FIND query shape for diagnostics (31.08 field
    debugging): attribute NAMES with a '*' marker when the value contains
    a wildcard star, plus QueryRetrieveLevel and SpecificCharacterSet.
    NO attribute values are logged (PHI discipline, access-journal rule).
    """
    keys = []
    for elem in ds:
        if elem.VR == "SQ" or not elem.keyword:
            continue
        val = elem.value
        if val is None or str(val).strip() == "":
            continue
        keys.append(elem.keyword + ("*" if "*" in str(val) else ""))
    level = str(getattr(ds, "QueryRetrieveLevel", "") or "")
    charset = str(getattr(ds, "SpecificCharacterSet", "") or "")
    return (
        f"level={level or '-'} keys={','.join(keys) or '-'} "
        f"charset={charset or '-'}"
    )


def handle_c_find(
    event,
    clarus_url,
    verbose,
    max_responses,
    http_timeout=30,
    tls=None,
    expose_rejected=False,
    qa_url="",
):
    """Dispatch C-FIND to Patient Root, Study Root, or MWL models.

    `expose_rejected` is a property of the AE that accepted this association,
    not of the message: the QA AE sets it, every other AE leaves it False. It
    selects `qa_url` (the backend QA webservice endpoint) as the DICOMweb base
    instead of `clarus_url`; no query parameter is added, so the bridge works
    against any server that publishes such an endpoint.
    """
    # Bridge: reject new requests during shutdown
    if _shutdown_requested.is_set():
        yield 0xC000, None
        return

    model_uid = event.context.abstract_syntax
    calling_ae = event.assoc.requestor.ae_title
    logging.info("[DIMSE] C-FIND from %s -> model: %s", calling_ae, model_uid)
    logging.info("[C-FIND-DEBUG] AE=%s %s", calling_ae, _cfind_shape(event.identifier))

    if model_uid == PATIENT_ROOT_UID:
        yield from _handle_patient_root(
            event,
            clarus_url,
            verbose,
            max_responses,
            http_timeout,
            tls,
            expose_rejected=expose_rejected,
            qa_url=qa_url,
        )
    elif model_uid == STUDY_ROOT_UID:
        yield from _handle_study_root(
            event,
            clarus_url,
            verbose,
            max_responses,
            http_timeout,
            tls,
            expose_rejected=expose_rejected,
            qa_url=qa_url,
        )
    elif model_uid == MWL_UID:
        # MWL is a worklist: rejected instances are stored studies, never
        # scheduled procedures, so the QA AE has nothing to change here.
        yield from _handle_mwl(
            event, clarus_url, verbose, max_responses, http_timeout, tls
        )
    else:
        yield 0xA900, None


# ---------------------------------------------------------------------------
# CRIT-002: Common QIDO-RS search handler (eliminates ~60 lines of duplication)
# ---------------------------------------------------------------------------
# 31.08 honest error logging: HTTP status errors log the actual status and
# URL (with PHI values masked, like the access journal);
# "connection failed" is reserved for TRANSPORT errors. 4xx is never
# retried (_http_retry) - a 400 here means the bridge sent a bad query.
#
# PS3.18 Table 6.7.1-1: every query key here can carry patient data, not just
# PatientName - PatientID, the accession number and the description fields all
# identify a person. Masking one key left the rest in the journal.
_MASKED_LOG_KEYS = frozenset(
    key.lower()
    for key in (
        "PatientName",
        "PatientID",
        "OtherPatientIDs",
        "PatientBirthDate",
        "PatientSex",
        "PatientAddress",
        "PatientTelephoneNumbers",
        "AccessionNumber",
        "StudyDescription",
        "SeriesDescription",
        "InstitutionName",
        "ReferringPhysicianName",
        "PerformingPhysicianName",
        "NameOfPhysiciansReadingStudy",
        "RequestingPhysician",
        "AdmittingDiagnosesDescription",
        "StudyID",
    )
)
# The same keys by tag, for a caller that passed numeric tags. Normalised to
# lower case because `_masked_query` lower-cases the key before the lookup:
# a tag whose element component contains a hex letter (0008,103E, 0008,1060,
# 0008,1080) is stored with that letter in upper case by convention, and an
# exact comparison against the canonical spelling let the whole tag - and the
# PHI it carries - through unmasked.
_MASKED_LOG_TAGS = frozenset(
    tag.lower()
    for tag in (
        "00100010",
        "00100020",
        "00100022",
        "00100030",
        "00100040",
        "00101040",
        "00102154",
        "00080050",
        "00081030",
        "0008103E",
        "00080080",
        "00080090",
        "00081050",
        "00081060",
        "00081080",
        "00200010",
    )
)


def _masked_query(params):
    """Render QIDO params for logging with every PHI value masked as ***.

    The key is URL-decoded and lower-cased before the lookup: a query string
    may spell `patientname` or percent-encode the tag, and an exact-match
    comparison against the canonical spelling would let those through.
    """
    from urllib.parse import unquote

    parts = []
    # The error path calls this with whatever `params` holds, and a caller may
    # pass `default_param=None` (no default search key). Masking must never be
    # the reason a status is not sent, so a missing map logs as empty.
    for key in sorted(params or {}):
        bare = unquote(str(key)).strip().lower().replace(",", "")
        if bare in _MASKED_LOG_KEYS or bare in _MASKED_LOG_TAGS:
            parts.append(f"{key}=***")
        else:
            parts.append(f"{key}={params[key]}")
    return "&".join(parts)


def _handle_qido_search(
    event,
    clarus_url,
    verbose,
    max_responses,
    http_timeout,
    default_param,
    log_label,
    url_path="studies",
    json_to_ds=_json_to_dataset,
    tls=None,
    expose_rejected=False,
    qa_url="",
):
    """Common C-FIND handler: QIDO-RS search -> DIMSE Pending responses.

    Args:
        default_param: default search value if no params extracted
            (e.g. {"PatientName": "*"} for Patient Root,
             {"StudyDescription": "*"} for Study Root)
        log_label: label for find_responses metric ('study' or 'mwl')
        url_path: DICOMweb resource path ("studies" or "workitems")
        json_to_ds: DICOMweb JSON -> Dataset converter (QIDO-study for
            "studies", MWL/UPS for "workitems")
    """
    ds = event.identifier
    # MWL requests carry their SPS-level keys inside (0040,0100), so the MWL
    # path needs keywords from the sequence item too. QIDO keeps the flat
    # extraction: its identifier carries no SPS sequence, and recursing would
    # only add noise to its keyword set.
    if url_path == "workitems":
        request_keywords = _extract_mwl_request_keywords(ds)
    else:
        request_keywords = _extract_request_keywords(ds)
    # Diagnostics ( 23.08): the bridge used to drop unmapped keywords
    # silently, which hid the modality-filter field bug behind "show all".
    logging.info(
        "[C-FIND] %s keywords: %s",
        log_label,
        ", ".join(sorted(request_keywords)) if request_keywords else "(none)",
    )
    if url_path == "workitems":
        # MWL: params already formed by caller, skip _build_qido_params
        params = default_param
    else:
        params = _build_qido_params(ds)
        if not params:
            params = default_param
    base = service_base_url(clarus_url, qa_url, expose_rejected)
    url = f"{base}/{url_path}"
    # One row beyond the limit keeps the local truncation guard reachable: a
    # server told exactly max_responses cannot prove there was more
    # (PS3.4 Table C.4.1-1: B001 means further matches exist).
    qido_limit = max_responses + 1 if max_responses else None
    try:
        results = _qido_request(
            url,
            params,
            http_timeout,
            limit=qido_limit,
            tls=tls,
        )
        if not isinstance(results, list):
            results = []
    except httpx.HTTPStatusError as exc:
        # Status class decides the refusal: 4xx is the BRIDGE's own bad
        # query (e.g. series keys on the study level), 5xx is the server or
        # a proxy failing for a reason unrelated to this query. The response
        # body is not logged (may echo the query).
        #
        # R14: 429 is NOT a bad query - it is the upstream rate-limiting us,
        # and it is TRANSIENT. `_http_status_to_dimse` already classifies it
        # (and 5xx) as 0xA700; a bare `>= 500` check turned rate-limiting into
        # 0xC000, which told the SCU to give up on a query that would succeed
        # a moment later.
        logging.warning(
            "[C-FIND] QIDO HTTP %d: %s?%s",
            exc.response.status_code,
            url,
            _masked_query(params),
        )
        dimse, _code, _desc = _http_status_to_dimse(exc.response.status_code)
        yield dimse, None
        return
    except QidoMalformedBodyError as exc:
        # R16: a 2xx whose body is not a DICOM JSON array is an unreadable
        # ANSWER, not an empty result (batch-2 raised it instead of returning
        # []). It is a ValueError, so without this clause it escaped the
        # generator: no warning was logged, `find_responses` was never
        # incremented, and the SCU got whatever pynettyic picked. Treat it as
        # the transient upstream failure it is.
        logging.warning("[C-FIND] QIDO malformed response: %s", exc)
        yield 0xA700, None
        return
    except httpx.HTTPError:
        # Transport errors only: ReadError / ConnectError / Timeout. A broken
        # TCP link is TRANSIENT - answering 0xC000 told the SCU the query was
        # permanently refused, so it would never retry. 0xA700 (Refused: Out
        # of Resources) is the retryable refusal. HTTPStatusError above maps
        # by status class: 4xx -> 0xC000 (this bridge's bad query), 5xx ->
        # 0xA700 (transient upstream outage).
        logging.warning("[C-FIND] QIDO error: connection failed")
        yield 0xA700, None
        return

    count = 0
    truncated = False
    for result in results:
        # Bridge: C-FIND response limit guard
        if max_responses and count >= max_responses:
            truncated = True
            logging.warning(
                "[C-FIND] Response limit reached (%d). "
                "Results truncated. Total available: %d.",
                max_responses,
                len(results),
            )
            break
        # Bridge: all responses sent with Pending status
        row_ds = json_to_ds(result, request_keywords)
        if url_path != "workitems":
            _stamp_cfind_response(event, row_ds)
        yield 0xFF00, row_ds
        count += 1

    # Bridge: final status when truncated  -  0xB001 (Warning).
    # PS3.4 Table C.4.1-1: "Matching terminated due to number of matches
    # encountered. Subsequent request may return additional matches."
    # This is a STANDARD C-FIND status. Every DIMSE-compatible viewer
    # (Weasis, MicroDICOM, Orthanc) is required to handle it.
    # pynetdicom knows this status  -  no "Unknown status" warning.
    # Bridge: count C-FIND responses sent. The counter is incremented BEFORE
    # the final yield: the SCP stops iterating a generator as soon as it reads
    # a non-Pending status, so anything after that yield never runs and the
    # metric would always report zero.
    find_responses.labels(type=log_label).inc(count)
    if truncated:
        yield 0xB001, None
    else:
        yield 0x0000, None


def _handle_patient_level(
    event,
    clarus_url,
    verbose,
    max_responses,
    http_timeout=30,
    tls=None,
    expose_rejected=False,
    qa_url="",
):
    """C-FIND Patient Root at PATIENT level: study QIDO, folded per patient.

    PS3.4 C.6.1.1.2 Table C.6-1: PATIENT is the ROOT level of the model this
    SCP announces, and Patient's Name (0010,0010) is the one Type R attribute
    there. The bridge has no patient index of its own, so the level is served
    from the same study QIDO the STUDY level uses (PS3.18 10.6: a study
    result carries the patient attributes), folded onto one row per
    PatientID.

    Folding rules, in the order they matter:
      - Matching keys still go to the server: the study query is built by
        `_handle_qido_search`'s own parameter mapping, so a PatientName or
        PatientID filter narrows the study set exactly as at STUDY level.
      - A row keeps only the attributes Table C.6-1 defines at this level.
        PatientName is required; the optional ones are returned when the
        origin supplied a value (the shared converter already drops
        empty values, so an absent tag means the origin had nothing).
      - One row per PatientID, first-seen order: the study rows arrive
        ordered by the origin, and re-sorting would make the response
        unstable across servers.
      - A study with NO PatientID cannot be identified as belonging to any
        other, so each such study is its own row. Collapsing them would
        assert that every anonymous study is one patient (PS3.4 C.6.1.1.2
        note: Patient ID is the grouping key; without it there is no group).

    max_responses caps the PATIENT rows (not the study rows that produced
    them): the caller asked for patients, and 0xB001 must mean "more
    patients exist". A study set big enough to be ambiguous is therefore
    fetched whole - the patient count is only known after the fold, so a
    study-level limit could hide patients behind a silent early cut.
    """
    ds = event.identifier
    request_keywords = _extract_request_keywords(ds)
    logging.info(
        "[C-FIND] patient keywords: %s",
        ", ".join(sorted(request_keywords)) if request_keywords else "(none)",
    )
    params = _build_qido_params(ds) or {}
    base = service_base_url(clarus_url, qa_url, expose_rejected)
    try:
        study_rows = _qido_request(
            f"{base}/studies",
            params,
            http_timeout,
            limit=None,
            tls=tls,
        )
        if not isinstance(study_rows, list):
            study_rows = []
    except httpx.HTTPStatusError as exc:
        # Same classifier as the other handlers: 4xx is this query's fault,
        # 5xx and 429 are upstream conditions the SCU should retry.
        logging.warning(
            "[C-FIND] QIDO HTTP %d: %s?%s",
            exc.response.status_code,
            f"{base}/studies",
            _masked_query(params),
        )
        dimse, _code, _desc = _http_status_to_dimse(exc.response.status_code)
        yield dimse, None
        return
    except QidoMalformedBodyError as exc:
        logging.warning("[C-FIND] QIDO malformed response: %s", exc)
        yield 0xA700, None
        return
    except httpx.HTTPError:
        logging.warning("[C-FIND] QIDO error: connection failed")
        yield 0xA700, None
        return

    count = 0
    truncated = False
    seen = set()
    for study_row in study_rows:
        if getattr(event, "is_cancelled", False):
            find_responses.labels(type="patient").inc(count)
            yield 0xFE00, None
            return
        if _shutdown_requested.is_set():
            find_responses.labels(type="patient").inc(count)
            yield 0xC000, None
            return
        patient_id = _json_first_value(study_row, "00100020") or ""
        # An empty PatientID identifies nobody, so it never merges: give each
        # such study a key of its own instead of a shared "" bucket.
        group_key = patient_id if patient_id else object()
        if group_key in seen:
            continue
        seen.add(group_key)
        if max_responses and count >= max_responses:
            truncated = True
            logging.warning(
                "[C-FIND] Response limit reached (%d). Patients truncated.",
                max_responses,
            )
            break
        row_ds = _json_to_dataset_patient(study_row)
        _stamp_cfind_response(event, row_ds)
        yield 0xFF00, row_ds
        count += 1

    find_responses.labels(type="patient").inc(count)
    if truncated:
        yield 0xB001, None
    else:
        yield 0x0000, None


def _handle_patient_root(
    event,
    clarus_url,
    verbose,
    max_responses,
    http_timeout=30,
    tls=None,
    expose_rejected=False,
    qa_url="",
):
    """C-FIND Patient Root -> QIDO-RS.

    PATIENT level: one row per patient, folded from the study query
    (_handle_patient_level) - see PS3.4 C.6.1.1.2 Table C.6-1.
    STUDY level (or level absent): GET /studies - unchanged.
    SERIES level (31.08): two-step series search, the same routing Study
    Root already applies - a patient-root SERIES query used to fall through
    to the study path and relabel study rows as SERIES, so the SCU received
    rows with no SeriesInstanceUID.
    """
    level = (
        str(getattr(event.identifier, "QueryRetrieveLevel", "") or "").strip().upper()
    )
    if level == "SERIES":
        yield from _handle_series_search(
            event,
            clarus_url,
            verbose,
            max_responses,
            http_timeout,
            tls,
            expose_rejected=expose_rejected,
            qa_url=qa_url,
        )
        return
    if level == "PATIENT":
        yield from _handle_patient_level(
            event,
            clarus_url,
            verbose,
            max_responses,
            http_timeout,
            tls,
            expose_rejected=expose_rejected,
            qa_url=qa_url,
        )
        return
    if level not in ("", "STUDY"):
        # R5: IMAGE (and anything unrecognised) has no study-level equivalent.
        # Falling through relabelled study rows with QueryRetrieveLevel=IMAGE
        # and no SOPInstanceUID, which the SCU cannot use - and the server
        # drops a return key it cannot honour, so the rows looked plausible.
        # PS3.4 C.4.1.1.4 / PS3.7: the honest answer is Identifier Does Not
        # Match SOP Class. Same refusal the PATIENT branch above already makes.
        yield 0xA900, None
        return
    yield from _handle_qido_search(
        event,
        clarus_url,
        verbose,
        max_responses,
        http_timeout,
        {},
        "study",
        tls=tls,
        expose_rejected=expose_rejected,
        qa_url=qa_url,
    )


def _handle_study_root(
    event,
    clarus_url,
    verbose,
    max_responses,
    http_timeout=30,
    tls=None,
    expose_rejected=False,
    qa_url="",
):
    """C-FIND Study Root -> QIDO-RS.

    STUDY level (or level absent): GET /studies - unchanged.
    SERIES level (31.08, MicroDicom series search): two-step search
    (study -> per-study series), see _handle_series_search.
    IMAGE level is NOT implemented: such a query is refused with 0xA900.
    It used to fall through to the study-level path, which relabelled study
    rows as IMAGE with no SOPInstanceUID - rows the SCU cannot use.
    """
    level = (
        str(getattr(event.identifier, "QueryRetrieveLevel", "") or "").strip().upper()
    )
    if level == "SERIES":
        yield from _handle_series_search(
            event,
            clarus_url,
            verbose,
            max_responses,
            http_timeout,
            tls,
            expose_rejected=expose_rejected,
            qa_url=qa_url,
        )
        return
    if level not in ("", "STUDY"):
        # R5: see _handle_patient_root - IMAGE has no study-row equivalent.
        yield 0xA900, None
        return
    yield from _handle_qido_search(
        event,
        clarus_url,
        verbose,
        max_responses,
        http_timeout,
        {},
        "study",
        tls=tls,
        expose_rejected=expose_rejected,
        qa_url=qa_url,
    )


def _handle_series_search(
    event,
    clarus_url,
    verbose,
    max_responses,
    http_timeout=30,
    tls=None,
    expose_rejected=False,
    qa_url="",
):
    """Study Root C-FIND at SERIES level: two-step QIDO search (31.08).

    Step a: GET /studies with the study-level keys -> study rows (identity:
    StudyInstanceUID + PatientName/PatientID for enrichment).
    Step b: per study, GET /studies/{study}/series with the series-level
    keys (+ Modality) -> series rows.

    Rows from all studies merge into ONE 0xFF00 stream; max_responses
    applies to the TOTAL (0xB001 truncation preserved). An empty step (a)
    yields an empty result (0x0000), not an error.
    """
    ds = event.identifier
    request_keywords = _extract_request_keywords(ds)
    logging.info(
        "[C-FIND] series keywords: %s",
        ", ".join(sorted(request_keywords)) if request_keywords else "(none)",
    )
    study_params, series_params = _split_qido_params(ds)
    base = service_base_url(clarus_url, qa_url, expose_rejected)
    # One row beyond the limit keeps the local truncation guard reachable
    # (PS3.4 Table C.4.1-1: B001 means further matches exist).
    qido_limit = max_responses + 1 if max_responses else None

    # ---- step a: studies ----
    studies_url = f"{base}/studies"
    try:
        study_rows = _qido_request(
            studies_url,
            study_params,
            http_timeout,
            limit=qido_limit,
            tls=tls,
        )
        if not isinstance(study_rows, list):
            study_rows = []
    except httpx.HTTPStatusError as exc:
        # Status class decides the refusal: 4xx is this query's fault
        # (0xC000), 5xx is the server or a proxy failing (0xA700, retryable).
        # R14: 429 is upstream rate-limiting and is transient, so the shared
        # classifier decides rather than a bare `>= 500` threshold.
        logging.warning(
            "[C-FIND] QIDO HTTP %d: %s?%s",
            exc.response.status_code,
            studies_url,
            _masked_query(study_params),
        )
        dimse, _code, _desc = _http_status_to_dimse(exc.response.status_code)
        yield dimse, None
        return
    except QidoMalformedBodyError as exc:
        # R16: an unreadable 2xx body is a transient upstream failure, not an
        # empty result; see the single-study handler above.
        logging.warning("[C-FIND] QIDO malformed response: %s", exc)
        yield 0xA700, None
        return
    except httpx.HTTPError:
        # A broken transport is transient: 0xA700 tells the SCU to retry,
        # while 0xC000 would read as a permanent refusal and lose a query
        # the network happened to interrupt.
        logging.warning("[C-FIND] QIDO error: connection failed")
        yield 0xA700, None
        return

    study_by_uid = {}
    for row in study_rows:
        study_uid = _json_first_value(row, "0020000D")
        if study_uid:
            study_by_uid[study_uid] = row
    # The study step asking for one row more than the caller's limit is how a
    # cap is DETECTED: if that many studies came back, there are more studies
    # than this search can walk. Their series were never fetched, so the answer
    # is already incomplete before step (b) produces a single row - and a
    # transfer that reported 0x0000 ("complete, no matches") while matches sit
    # beyond the cap tells the SCU the study set is empty when it is not.
    study_cap_hit = qido_limit is not None and len(study_rows) >= qido_limit
    if not study_by_uid:
        # Empty step (a) -> empty result, per the prompt.
        yield 0x0000, None
        return

    # ---- step b: per-study series ----
    count = 0
    truncated = False
    for study_uid, study_row in study_by_uid.items():
        # A C-CANCEL from the SCU, or a bridge shutdown, must stop a broad
        # SERIES walk: each iteration can block for http_timeout and the walk
        # otherwise has no way out. Mirror the metric bookkeeping the terminal
        # paths do: count the rows already sent before the non-Pending yield
        # (nothing after a non-Pending status is ever executed).
        if getattr(event, "is_cancelled", False):
            find_responses.labels(type="series").inc(count)
            yield 0xFE00, None
            return
        if _shutdown_requested.is_set():
            find_responses.labels(type="series").inc(count)
            yield 0xC000, None
            return
        if max_responses and count >= max_responses:
            truncated = True
            logging.warning(
                "[C-FIND] Response limit reached (%d). Results truncated.",
                max_responses,
            )
            break
        series_url = f"{base}/studies/{quote(study_uid, safe='')}/series"
        try:
            series_rows = _qido_request(
                series_url,
                dict(series_params),
                http_timeout,
                limit=qido_limit,
                tls=tls,
            )
            if not isinstance(series_rows, list):
                series_rows = []
        except httpx.HTTPStatusError as exc:
            # Status class decides the refusal: 4xx is this query's fault
            # (0xC000), 5xx is the server or a proxy failing (0xA700).
            # R14: 429 is transient and the shared classifier already says so.
            logging.warning(
                "[C-FIND] QIDO HTTP %d: %s?%s",
                exc.response.status_code,
                series_url,
                _masked_query(series_params),
            )
            dimse, _code, _desc = _http_status_to_dimse(exc.response.status_code)
            yield dimse, None
            return
        except QidoMalformedBodyError as exc:
            # R16: same class as the single-study handler - an unreadable 2xx
            # body is a transient upstream failure, not an empty result.
            logging.warning("[C-FIND] QIDO malformed response: %s", exc)
            yield 0xA700, None
            return
        except httpx.HTTPError:
            # Same class as step (a): a transport break is retryable.
            logging.warning("[C-FIND] QIDO error: connection failed")
            yield 0xA700, None
            return
        for row in series_rows:
            if max_responses and count >= max_responses:
                truncated = True
                logging.warning(
                    "[C-FIND] Response limit reached (%d). " "Results truncated.",
                    max_responses,
                )
                break
            row_ds = _json_to_dataset_series(
                row,
                request_keywords,
                study_row,
            )
            _stamp_cfind_response(event, row_ds)
            yield 0xFF00, row_ds
            count += 1
        if truncated:
            break

    # Counted before the terminal yield for the same reason as the study
    # handler: nothing after a non-Pending status is ever executed.
    find_responses.labels(type="series").inc(count)
    if truncated or study_cap_hit:
        yield 0xB001, None
    else:
        yield 0x0000, None


def _handle_mwl(event, clarus_url, verbose, max_responses, http_timeout=30, tls=None):
    """C-FIND MWL -> UPS-RS workitems (Stage 3: real MWL key mapping).

    Query: maps the modality's MWL matching keys to the workitem query params
    the server filters on (station_aet, modality, patient_id, workitem_uid,
    workflow_state). Response: converts the workitem DICOMweb JSON back to MWL
    attributes (scheduled procedure step + patient) so the modality sees a
    complete worklist entry.
    """
    params = _build_mwl_params(event.identifier)
    yield from _handle_qido_search(
        event,
        clarus_url,
        verbose,
        max_responses,
        http_timeout,
        params,
        "mwl",
        url_path="workitems",
        json_to_ds=_json_to_dataset_mwl,
        tls=tls,
    )
