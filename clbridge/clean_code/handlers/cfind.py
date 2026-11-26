# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  C-FIND handler: dispatches to Patient/Study Root or MWL.

CRIT-002: extracted common _handle_qido_search() to eliminate ~60 lines of duplication.
"""

import logging
from collections import Counter
from urllib.parse import quote

import httpx

from clean_code.http_helpers import (
    QidoMalformedBodyError,
    QidoPage,
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
    _json_to_dataset_instance,
    _json_to_dataset_mwl,
    _json_to_dataset_patient,
    _json_to_dataset_series,
    _mwl_datetime_in_bounds,
    _mwl_physician_matchers,
    _mwl_physician_matches,
    _mwl_start_datetime_bounds,
    _split_instance_qido_params,
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
    row_filter=None,
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
        if not isinstance(results, (list, QidoPage)):
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
    # PS3.18 10.6.1.4: the origin can answer fewer rows than the page size and
    # still report the search incomplete. 0x0000 states that no further
    # matches exist (PS3.4 Table C.4.1-1), so an answer the origin left
    # unfinished must be reported as 0xB001 instead. Only a `QidoPage` carries
    # that flag; a plain list (a caller that built its own pages) says nothing.
    incomplete = isinstance(results, QidoPage) and results.more
    for result in results:
        # PS3.7 9.3.2: a C-CANCEL can arrive at ANY time, and the SCU expects
        # the next response to be 0xFE00. Checking only at the top of a study
        # meant a multi-row answer kept sending Pending responses after the
        # SCU had already cancelled - each one a write the SCU no longer
        # reads. The rows already sent are retained in the metric.
        if getattr(event, "is_cancelled", False):
            find_responses.labels(type=log_label).inc(count)
            yield 0xFE00, None
            return
        if _shutdown_requested.is_set():
            find_responses.labels(type=log_label).inc(count)
            yield 0xC000, None
            return
        # A matching key the origin cannot filter on is applied here, BEFORE
        # the response limit, so a non-matching row never consumes a Pending
        # response (PS3.4 Table K.6-1 matching rule).
        if row_filter is not None and not row_filter(result):
            continue
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

    # Bridge: final status when truncated.
    #
    # 0xB001 ("matching terminated due to number of matches encountered") is
    # NOT defined for Query/Retrieve by PS3.4 Table C.4.1-1 nor for MWL by
    # Table K.4-1, so it is not a status this SCP may choose to send. A search
    # the bridge could not finish is a refusal, not a success and not a
    # warning: 0xA700 (Refused: Out of Resources) is the defined status for
    # "the SCP could not complete this request", and it also tells the SCU
    # that retrying - with a narrower query - is meaningful.
    #
    # PS3.7 9.3.2: a C-CANCEL received during the HTTP request is only
    # observable AFTER it, so the cancel check has to sit between the request
    # and this terminal status; otherwise the SCU is told "success" for a
    # search it already abandoned.
    find_responses.labels(type=log_label).inc(count)
    if getattr(event, "is_cancelled", False):
        yield 0xFE00, None
        return
    if truncated or incomplete:
        yield 0xA700, None
    else:
        yield 0x0000, None


def _patient_group_key(study_row):
    """Grouping key of a study row in the patient fold.

    A study with a PatientID groups by that ID. A study without one
    (anonymous) cannot be asserted to belong to any other (PS3.4 C.6.1.1.2
    note: Patient ID is the grouping key), so it is its own group. The
    ("", id(study_row)) key keeps anonymous rows apart while staying distinct
    from a real PatientID; the id is stable because the pre-fold pass and the
    fold walk the same row list.
    """
    patient_id = _json_first_value(study_row, "00100020") or ""
    return patient_id if patient_id else ("", id(study_row))


def _int_or_zero(value):
    """DICOMweb JSON integer-string -> int; 0 when absent or unparsable.

    NumberOfStudyRelatedSeries/Instances arrive as IS strings, and the server
    may omit the key entirely. 0 is the neutral term of the fold: a study
    with no count adds nothing to the patient total.
    """
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


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
        if not isinstance(study_rows, (list, QidoPage)):
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

    # PS3.4 C.6.1.1.2: NumberOfPatientRelated* are TOTALS over the patient's
    # studies, series and instances - not one study's own counts. The fold
    # emits a row while walking, and the first study of a patient cannot know
    # how many more follow, so the totals are computed in one pass FIRST.
    counts = Counter()
    series_sums = {}
    instance_sums = {}
    for study_row in study_rows:
        key = _patient_group_key(study_row)
        counts[key] += 1
        series_sums[key] = series_sums.get(key, 0) + _int_or_zero(
            _json_first_value(study_row, "00201206")
        )
        instance_sums[key] = instance_sums.get(key, 0) + _int_or_zero(
            _json_first_value(study_row, "00201208")
        )

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
        group_key = _patient_group_key(study_row)
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
        # The three Patient-level counters (0020,1200/1202/1204) are the
        # fold's own totals, not values read off a study row, so they are
        # added here as IS after the study row is projected to the level.
        row_ds.add_new((0x0020, 0x1200), "IS", counts[group_key])
        row_ds.add_new((0x0020, 0x1202), "IS", series_sums[group_key])
        row_ds.add_new((0x0020, 0x1204), "IS", instance_sums[group_key])
        _stamp_cfind_response(event, row_ds)
        yield 0xFF00, row_ds
        count += 1

    find_responses.labels(type="patient").inc(count)
    # PS3.7 9.3.2 and the same rule as the common handler: a cancel that
    # arrived during the HTTP request is only visible here, before the
    # terminal status. 0xB001 is not defined for Query/Retrieve either, so an
    # incomplete answer is the defined refusal 0xA700.
    if getattr(event, "is_cancelled", False):
        yield 0xFE00, None
        return
    if truncated:
        yield 0xA700, None
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
    IMAGE level: three-step series search (study -> series -> instances), the
    same routing Study Root applies - PS3.4 Table C.6-4 defines the Composite
    Object Instance level for both models.
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
    if level == "IMAGE":
        yield from _handle_image_search(
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
        # R5: an unrecognised level has no study-level equivalent. Falling
        # through relabelled study rows with that level and no SOPInstanceUID,
        # which the SCU cannot use - and the server drops a return key it
        # cannot honour, so the rows looked plausible. PS3.4 C.4.1.3.1 / PS3.7:
        # the honest answer is Error: Data Set does not match SOP Class.
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
    IMAGE level: three-step search (study -> series -> instances), see
    _handle_image_search. PS3.4 C.6.2.1.4 assigns the Study Root the same
    Composite Object Instance level as the Patient Root.
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
    if level == "IMAGE":
        yield from _handle_image_search(
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
        # R5: see _handle_patient_root - an unrecognised level has no
        # study-row equivalent.
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
    # (PS3.4 Table C.4.1-1: further matches exist).
    qido_limit = max_responses + 1 if max_responses else None

    # ---- step a: studies ----
    #
    # The response budget limits MATCHING DESCENDANTS, not parent rows. A
    # SERIES query with a limit of one used to ask for two studies and then
    # stop, so a match in the third study was never looked at - the limit was
    # silently applied to the level above the one it describes. Parents are
    # therefore paged until the walk finds an extra matching series or runs
    # out of studies.
    studies_url = f"{base}/studies"
    try:
        study_rows = _qido_request(
            studies_url,
            study_params,
            http_timeout,
            limit=None,
            tls=tls,
        )
        if not isinstance(study_rows, (list, QidoPage)):
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
    # An origin can leave the study set unfinished on its own (`more`), e.g.
    # when its configured page is smaller than this walk asked for. The rows
    # beyond that page are unknown, so the answer cannot be called complete.
    origin_more = isinstance(study_rows, QidoPage) and study_rows.more
    if not study_by_uid:
        # Empty step (a) -> empty result, per the prompt.
        yield 0x0000, None
        return

    # ---- step b: per-study series ----
    count = 0
    # PS3.4 Table C.4.1-1: 0xB001 states that FURTHER MATCHES EXIST. Reaching the
    # response cap alone does not prove that - the studies not yet walked may
    # contain no matching series at all. So the walk keeps going after the cap
    # until it either finds one extra matching descendant (then it really can
    # say "more matches exist") or runs out of parents.
    extra_match = False
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
            # Stop EMITTING; keep checking the remaining studies, because only
            # an extra matching descendant makes 0xB001 a true statement. The
            # series loop below sees the spent budget and stops yielding.
            logging.warning(
                "[C-FIND] Response limit reached (%d). Checking for further "
                "matches.",
                max_responses,
            )
        series_url = f"{base}/studies/{quote(study_uid, safe='')}/series"
        try:
            series_rows = _qido_request(
                series_url,
                dict(series_params),
                http_timeout,
                limit=qido_limit,
                tls=tls,
            )
            if not isinstance(series_rows, (list, QidoPage)):
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
            # Same rule as the common loop: a cancel that arrived during the
            # HTTP call must stop the OUTPUT, not wait for the next study.
            if getattr(event, "is_cancelled", False):
                find_responses.labels(type="series").inc(count)
                yield 0xFE00, None
                return
            if _shutdown_requested.is_set():
                find_responses.labels(type="series").inc(count)
                yield 0xC000, None
                return
            if max_responses and count >= max_responses:
                # Same rule as the parent loop: one more matching series is
                # enough to justify 0xB001, and nothing else is.
                extra_match = True
                break
            row_ds = _json_to_dataset_series(
                row,
                request_keywords,
                study_row,
            )
            _stamp_cfind_response(event, row_ds)
            yield 0xFF00, row_ds
            count += 1
        if extra_match:
            break

    # Counted before the terminal yield for the same reason as the study
    # handler: nothing after a non-Pending status is ever executed.
    find_responses.labels(type="series").inc(count)
    if getattr(event, "is_cancelled", False):
        yield 0xFE00, None
        return
    # 0xB001 is not a defined Query/Retrieve status (PS3.4 Table C.4.1-1), so
    # an answer the bridge could not complete is the defined refusal 0xA700.
    #
    # An origin-imposed page limit counts: the origin answered paged rows and
    # said `more`, so the studies beyond it were never walked even though the
    # bridge's own cap was never reached (PS3.18 10.6.1.4).
    if extra_match or study_cap_hit or origin_more:
        yield 0xA700, None
    else:
        yield 0x0000, None


def _image_qido_error(url, params, exc):
    """Log and classify one failed hop of an IMAGE-level walk.

    Shared by the three steps so each of them refuses for the same reason the
    study/series handlers do (PS3.18 10.6; the status class decides): 4xx is
    this query's fault (0xC000), 5xx and 429 are transient upstream conditions
    the SCU should retry (0xA700), and a broken transport is transient too.
    Returns the DIMSE status to yield.
    """
    if isinstance(exc, httpx.HTTPStatusError):
        # R14: 429 is upstream rate-limiting and is transient, so the shared
        # classifier decides rather than a bare `>= 500` threshold.
        logging.warning(
            "[C-FIND] QIDO HTTP %d: %s?%s",
            exc.response.status_code,
            url,
            _masked_query(params),
        )
        dimse, _code, _desc = _http_status_to_dimse(exc.response.status_code)
        return dimse
    if isinstance(exc, QidoMalformedBodyError):
        # R16: an unreadable 2xx body is a transient upstream failure, not an
        # empty result.
        logging.warning("[C-FIND] QIDO malformed response: %s", exc)
        return 0xA700
    # Transport errors only (ReadError / ConnectError / Timeout): transient,
    # so 0xA700 rather than a 0xC000 that reads as a permanent refusal.
    logging.warning("[C-FIND] QIDO error: connection failed")
    return 0xA700


def _handle_image_search(
    event,
    clarus_url,
    verbose,
    max_responses,
    http_timeout=30,
    tls=None,
    expose_rejected=False,
    qa_url="",
):
    """C-FIND at COMPOSITE OBJECT INSTANCE (IMAGE) level: three-step QIDO walk.

    PS3.4 C.4.1.3.1.1 (Hierarchical Search Method): start at the top level and
    match down until the level named by QueryRetrieveLevel is reached, so the
    walk is study -> series -> instances of the requested series, each hop
    carrying that level's own matching keys (`_split_instance_qido_params`).
    PS3.4 Table C.6-4 / C.6.2.1.4 (standard/ps3.4/image-level-C.6-4.json)
    defines the same instance level for both Query/Retrieve models.

    A row without SOPInstanceUID is dropped rather than yielded: that was the
    defect 0xA900 used to paper over - study rows relabelled as IMAGE look
    plausible to the SCU but cannot be retrieved. Every row that does go out
    carries QueryRetrieveLevel=IMAGE (C.4.1.1.3.2), the Unique Keys of the
    levels above and the instance's own SOP Instance UID.

    This is the baseline hierarchical search, not the relational extension
    (C.4.1.2.2.1): every study comes back from step (a) because a baseline
    request may omit the study Unique Key, and every series of a matched
    study is walked. A SeriesInstanceUID in the identifier narrows step (b) to
    that one series, so the common "instances of one series" query is three
    requests. Rows merge into ONE 0xFF00 stream and max_responses applies to
    the TOTAL, with 0xB001 when the cap cuts matches off.
    """
    ds = event.identifier
    request_keywords = _extract_request_keywords(ds)
    logging.info(
        "[C-FIND] image keywords: %s",
        ", ".join(sorted(request_keywords)) if request_keywords else "(none)",
    )
    study_params, series_params, instance_params = _split_instance_qido_params(ds)
    base = service_base_url(clarus_url, qa_url, expose_rejected)
    # One row beyond the limit keeps the local truncation guard reachable
    # (PS3.4 Table C.4.1-1: B001 means further matches exist).
    qido_limit = max_responses + 1 if max_responses else None

    # ---- step a: studies ----
    studies_url = f"{base}/studies"
    try:
        study_rows = _qido_request(
            studies_url, study_params, http_timeout, limit=None, tls=tls
        )
        if not isinstance(study_rows, (list, QidoPage)):
            study_rows = []
    except (httpx.HTTPError, QidoMalformedBodyError) as exc:
        yield _image_qido_error(studies_url, study_params, exc), None
        return

    study_by_uid = {}
    for row in study_rows:
        study_uid = _json_first_value(row, "0020000D")
        if study_uid:
            study_by_uid[study_uid] = row
    # Same cap detection as the series walk: one row more than the caller's
    # limit means studies were never walked, so the answer is already
    # incomplete and 0x0000 would claim otherwise.
    study_cap_hit = qido_limit is not None and len(study_rows) >= qido_limit
    # Same rule as the series walk: an origin-imposed page limit on the study
    # hop leaves studies unvisited, so the answer is incomplete even though
    # this walk never reached its own cap.
    origin_more = isinstance(study_rows, QidoPage) and study_rows.more
    if not study_by_uid:
        yield 0x0000, None
        return

    count = 0
    truncated = False
    # Same rule as the SERIES walk (PS3.4 Table C.4.1-1): the cap alone does not
    # prove another match exists, so the walk keeps checking parents until it
    # finds one. `extra_match` is what justifies reporting an incomplete answer.
    extra_match = False
    # Set when an ORIGIN page limit (not this bridge's cap) left rows unseen.
    origin_more = False
    for study_uid, study_row in study_by_uid.items():
        # A C-CANCEL or a shutdown must stop the walk: each iteration can
        # block for http_timeout. Mirror the series walk's metric bookkeeping
        # (nothing after a non-Pending yield is ever executed).
        if getattr(event, "is_cancelled", False):
            find_responses.labels(type="image").inc(count)
            yield 0xFE00, None
            return
        if _shutdown_requested.is_set():
            find_responses.labels(type="image").inc(count)
            yield 0xC000, None
            return
        if max_responses and count >= max_responses and not truncated:
            # Emitting stops; the check for a further match does not.
            truncated = True
            logging.warning(
                "[C-FIND] Response limit reached (%d). Checking for further "
                "matches.",
                max_responses,
            )

        # ---- step b: series of this study ----
        series_url = f"{base}/studies/{quote(study_uid, safe='')}/series"
        try:
            series_rows = _qido_request(
                series_url, dict(series_params), http_timeout, limit=qido_limit, tls=tls
            )
            if not isinstance(series_rows, (list, QidoPage)):
                series_rows = []
        except (httpx.HTTPError, QidoMalformedBodyError) as exc:
            yield _image_qido_error(series_url, series_params, exc), None
            return

        for series_row in series_rows:
            series_uid = _json_first_value(series_row, "0020000E")
            # An unnamed series cannot be walked: the instance route is keyed
            # by it, and a row without it carries no Unique Key of its level.
            if not series_uid:
                continue
            # A cancel can arrive while the previous series was being read.
            if getattr(event, "is_cancelled", False):
                find_responses.labels(type="image").inc(count)
                yield 0xFE00, None
                return
            if max_responses and count >= max_responses:
                # A series not yet walked is a candidate for a further match,
                # but it is only a MATCH once an instance inside it matches.
                # Fetch it and let the instance loop decide.
                truncated = True
            # ---- step c: instances of this series ----
            #
            # A shutdown that began during the series request must stop the
            # walk HERE, before another instance request goes out. Only the
            # outermost study loop used to check the flag, so a shutdown could
            # still emit rows and issue instance requests for the same study.
            if _shutdown_requested.is_set():
                find_responses.labels(type="image").inc(count)
                yield 0xC000, None
                return
            instances_url = (
                f"{base}/studies/{quote(study_uid, safe='')}"
                f"/series/{quote(series_uid, safe='')}/instances"
            )
            try:
                instance_rows = _qido_request(
                    instances_url,
                    dict(instance_params),
                    http_timeout,
                    limit=qido_limit,
                    tls=tls,
                )
                if not isinstance(instance_rows, (list, QidoPage)):
                    instance_rows = []
            except (httpx.HTTPError, QidoMalformedBodyError) as exc:
                yield _image_qido_error(instances_url, instance_params, exc), None
                return
            # Shutdown or cancel that arrived DURING the instance request: the
            # rows it returned must not be emitted, and no further request may
            # go out for this study.
            if _shutdown_requested.is_set():
                find_responses.labels(type="image").inc(count)
                yield 0xC000, None
                return
            if isinstance(instance_rows, QidoPage) and instance_rows.more:
                # The origin answered only its own page of instances and said
                # so. Treating that as a complete answer tells the SCU the
                # search found everything, when rows beyond the page were
                # never even offered.
                origin_more = True
            for row in instance_rows:
                if not _json_first_value(row, "00080018"):
                    # PS3.4 C.6.1.1.5: SOP Instance UID is the level's Unique
                    # key. Without it the row is not an instance match at all.
                    logging.warning(
                        "[C-FIND] instance row without SOPInstanceUID dropped"
                    )
                    continue
                # A cancel during the instance read must stop the output.
                if getattr(event, "is_cancelled", False):
                    find_responses.labels(type="image").inc(count)
                    yield 0xFE00, None
                    return
                if max_responses and count >= max_responses:
                    # THIS is a matching descendant the bridge will not send: it
                    # is the fact 0xB001 asserts.
                    extra_match = True
                    break
                row_ds = _json_to_dataset_instance(
                    row,
                    request_keywords,
                    study_uid,
                    series_uid,
                    study_row,
                    series_row,
                )
                _stamp_cfind_response(event, row_ds)
                yield 0xFF00, row_ds
                count += 1
            if extra_match:
                break
        if extra_match:
            break

    # Counted before the terminal yield for the same reason as the study
    # handler: nothing after a non-Pending status is ever executed.
    find_responses.labels(type="image").inc(count)
    if getattr(event, "is_cancelled", False):
        yield 0xFE00, None
        return
    # Same rule as the series walk: 0xB001 is undefined for Query/Retrieve, so
    # an incomplete answer is 0xA700, and an origin-imposed page limit on any
    # hop means rows beyond it were never walked.
    if extra_match or study_cap_hit or origin_more:
        yield 0xA700, None
    else:
        yield 0x0000, None


def _mwl_row_in_time_window(row, bounds):
    """Whether a workitem row's start instant lies inside the requested window.

    Reads the workitem's Combined Scheduled Procedure Step Start DateTime
    (0040,4005) and compares the DATE AND TIME together: PS3.4 Table K.6-1
    defines (0040,0002) and (0040,0003) as one interval, so a July 6 workitem
    at 22:00 belongs to "July 5-7 at 10:00-18:00" (see
    `_mwl_start_datetime_bounds`). A row without the attribute is kept:
    absence is not a proven mismatch, and dropping it would hide a workitem
    the modality may still want.
    """
    stamp = _json_first_value(row, "00404005")
    if not stamp:
        return True
    return _mwl_datetime_in_bounds(stamp, bounds)


def _handle_mwl(event, clarus_url, verbose, max_responses, http_timeout=30, tls=None):
    """C-FIND MWL -> UPS-RS workitems (Stage 3: real MWL key mapping).

    Query: maps the modality's MWL matching keys to the workitem query params
    the server filters on (station_aet, modality, patient_id, workitem_uid,
    workflow_state). Response: converts the workitem DICOMweb JSON back to MWL
    attributes (scheduled procedure step + patient) so the modality sees a
    complete worklist entry.
    """
    params = _build_mwl_params(event.identifier)
    # PS3.4 Table K.6-1 makes Scheduled Procedure Step Start Date (0040,0002)
    # and Start Time (0040,0003) a SINGLE interval: July 5-7 at 10:00-18:00
    # includes a July 6 workitem at 22:00. UPS-RS has no parameter for a time
    # of day - the origin filters the combined workitem start instant - so the
    # window is applied HERE, before the response limit, with the date and
    # time bounds combined rather than as two independent filters.
    time_bounds = _mwl_start_datetime_bounds(event.identifier)
    # PS3.4 Table K.6-1 also makes Scheduled Performing Physician's Name
    # (0040,0006) a matching key with "Single Value or Wildcard", and the
    # UPS-RS workitem search has no parameter for it - so it is applied
    # locally over the candidate rows, exactly like the time window.
    physician_matchers = _mwl_physician_matchers(event.identifier)
    yield from _mwl_search(
        event,
        clarus_url,
        verbose,
        max_responses,
        http_timeout,
        params,
        time_bounds,
        tls=tls,
        physician_matchers=physician_matchers,
    )


# Candidate page size for the MWL walk, in units of `max_responses`. Two
# pages' worth is the smallest window that can prove "no more matches": the
# walk stops as soon as it has one row beyond the cap, and a page that the
# local filter rejects in full has to be followed by another.
_MWL_PAGE_FACTOR = 4


def _mwl_search(
    event,
    clarus_url,
    verbose,
    max_responses,
    http_timeout,
    params,
    time_bounds,
    tls=None,
    physician_matchers=(),
):
    """MWL search whose response limit counts MATCHING rows, not candidates.

    PS3.4 Table K.6-1 puts the matching rule on the worklist row, but the
    date/time window can only be applied locally (UPS-RS has no time-of-day
    parameter), so the origin's rows are CANDIDATES. Asking for exactly
    `max_responses + 1` candidates and filtering afterwards cannot recover a
    matching row the bridge never fetched: with a limit of one, two in-window
    candidates that fall outside the window hide a third that matches.

    So the walk pages the origin while it has room for another match, and
    stops only when it finds one extra matching row (proving more exist) or
    the origin runs out. That is the same rule the SERIES and IMAGE walks use
    for their parent hops.
    """
    base = service_base_url(clarus_url, "", False)
    url = f"{base}/workitems"
    want = max_responses or 0
    page = max(1, want * _MWL_PAGE_FACTOR) if want else None
    offset = 0
    count = 0
    # One matching row beyond the cap is the evidence that further matches
    # exist; without it the answer is complete and reports Success.
    extra_match = False
    seen_offsets = set()

    while True:
        if getattr(event, "is_cancelled", False):
            find_responses.labels(type="mwl").inc(count)
            yield 0xFE00, None
            return
        if _shutdown_requested.is_set():
            find_responses.labels(type="mwl").inc(count)
            yield 0xC000, None
            return
        page_params = dict(params)
        try:
            results = _qido_request(
                url,
                page_params,
                http_timeout,
                limit=page,
                offset=offset,
                tls=tls,
            )
            if not isinstance(results, (list, QidoPage)):
                results = []
        except (httpx.HTTPError, QidoMalformedBodyError) as exc:
            yield _image_qido_error(url, params, exc), None
            return

        for row in results:
            # A cancel can arrive while the origin is answering.
            if getattr(event, "is_cancelled", False):
                find_responses.labels(type="mwl").inc(count)
                yield 0xFE00, None
                return
            if time_bounds is not None and not _mwl_row_in_time_window(
                row, time_bounds
            ):
                continue
            if not _mwl_physician_matches(
                _json_first_value(row, "00400006"), physician_matchers
            ):
                continue
            if want and count >= want:
                # A MATCHING row the bridge will not send: this is the fact
                # that makes the answer incomplete.
                extra_match = True
                break
            row_ds = _json_to_dataset_mwl(
                row, _extract_mwl_request_keywords(event.identifier)
            )
            yield 0xFF00, row_ds
            count += 1
        if extra_match:
            break

        if page is None:
            # No cap: the single unpaged answer was the whole result.
            break
        if len(results) < page:
            # The origin had nothing left to give: the answer is complete.
            break
        origin_more = isinstance(results, QidoPage) and results.more
        offset += page
        if offset in seen_offsets:
            # A page that does not advance would loop forever.
            break
        seen_offsets.add(offset)
        if origin_more and offset == page:
            # The origin flagged the first page as unfinished but still
            # returned a full page: keep walking rather than trust the flag
            # alone. Any later page that is short ends the loop above.
            continue

    find_responses.labels(type="mwl").inc(count)
    if getattr(event, "is_cancelled", False):
        yield 0xFE00, None
        return
    # 0xA700 is the defined refusal for an answer the bridge could not
    # complete; 0xB001 is not defined for MWL (PS3.4 Table K.4-1).
    if extra_match:
        yield 0xA700, None
    else:
        yield 0x0000, None
