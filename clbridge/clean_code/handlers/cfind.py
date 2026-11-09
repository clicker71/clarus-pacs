# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  C-FIND handler: dispatches to Patient/Study Root or MWL.

CRIT-002: extracted common _handle_qido_search() to eliminate ~60 lines of duplication.
"""

import logging
from urllib.parse import quote

import httpx

from clean_code.http_helpers import _qido_request, service_base_url
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
        "RequestingPhysician",
        "AdmittingDiagnosesDescription",
        "StudyID",
    )
)
# The same keys by tag, for a caller that passed numeric tags.
_MASKED_LOG_TAGS = frozenset(
    tag
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
    for key in sorted(params):
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
        # 31.08 honest log: a 400 here is the BRIDGE's fault (bad query,
        # e.g. series keys on the study level) - never a "connection
        # failure". The response body is not logged (may echo the query).
        logging.warning(
            "[C-FIND] QIDO HTTP %d: %s?%s",
            exc.response.status_code,
            url,
            _masked_query(params),
        )
        yield 0xC000, None
        return
    except httpx.HTTPError:
        # Transport errors only: ReadError / ConnectError / Timeout.
        logging.warning("[C-FIND] QIDO error: connection failed")
        yield 0xC000, None
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
    """C-FIND Patient Root -> QIDO-RS studies."""
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
    IMAGE level is NOT implemented - out of scope, documented in the
    report; such a query falls through to the study-level path and the
    server answers 400 for the foreign SOPInstanceUID key (honest log).
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
        logging.warning(
            "[C-FIND] QIDO HTTP %d: %s?%s",
            exc.response.status_code,
            studies_url,
            _masked_query(study_params),
        )
        yield 0xC000, None
        return
    except httpx.HTTPError:
        logging.warning("[C-FIND] QIDO error: connection failed")
        yield 0xC000, None
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
            logging.warning(
                "[C-FIND] QIDO HTTP %d: %s?%s",
                exc.response.status_code,
                series_url,
                _masked_query(series_params),
            )
            yield 0xC000, None
            return
        except httpx.HTTPError:
            logging.warning("[C-FIND] QIDO error: connection failed")
            yield 0xC000, None
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
