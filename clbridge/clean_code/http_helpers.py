# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  HTTP helpers: retry, QIDO/WADO/STOW requests (BR-005).

Contains _http_retry (exponential backoff + jitter),
_qido_request, _wado_metadata_request, stow_single_instance, and
_http_status_to_dimse (BR-011).

Auth is handled by nginx reverse proxy  -  no Basic Auth in application code.
"""

import io
import json
import logging
import time
import uuid
from urllib.parse import urlsplit, urlunsplit

import httpx
from pydicom import dcmwrite
from pydicom.uid import ExplicitVRLittleEndian

from clean_code.metrics import (
    stow_requests,
    stow_retries,
)

# ---------------------------------------------------------------------------
# 2908: single source of the DICOMweb base URL (clarus_url refactor)
# ---------------------------------------------------------------------------
RETRY_AFTER_MIN_SEC = 5
RETRY_AFTER_MAX_SEC = 300
RETRY_AFTER_DEFAULT_SEC = 30


def dicomweb_base(clarus_url):
    """Return the normalized DICOMweb base URL (no trailing slash).

    Single source for EVERY DICOMweb URL the bridge builds. Config
    validation (clean_code.config.validate_clarus_url) already rejects
    bad schemes and strips the trailing slash; this re-strips for
    defense in depth.
    """
    return clarus_url.rstrip("/")


def dicomweb_origin(clarus_url):
    """Return scheme://netloc of clarus_url (health probe lives at root)."""
    parts = urlsplit(clarus_url)
    return urlunsplit((parts.scheme, parts.netloc, "", "", ""))


def service_base_url(clarus_url, qa_url, qa_view):
    """Pick the DICOMweb base URL for this request.

    `qa_view` is true for associations accepted by the QA AE. AIRA 57.2.1
    defines the expose mechanism as "a webservice endpoint specifically
    configured to provide access to hidden instances", i.e. a separate base
    URL the deployment chooses - NOT a query parameter. Selecting the base URL
    keeps the bridge usable against any DICOMweb server: a server that
    publishes a QA endpoint works, and one that would need a vendor-specific
    parameter is simply pointed at its own endpoint instead.

    Falls back to `clarus_url` when no QA endpoint is configured, so a bridge
    with `expose_port` set but no `qa_url` never silently queries an empty
    base - the caller has already refused to start the QA AE in that case.
    """
    if qa_view and qa_url:
        return dicomweb_base(qa_url)
    return dicomweb_base(clarus_url)


def retry_after_seconds(headers, default=RETRY_AFTER_DEFAULT_SEC):
    """Parse the Retry-After header (seconds or HTTP-date), clamp to [5, 300].

    Garbage / absent header -> fixed 30 s default. HTTP-date values are
    treated conservatively as the default (no clock math in the bridge).
    """
    try:
        value = int(str(headers.get("Retry-After", "")).strip())
    except (ValueError, TypeError, AttributeError):
        return default
    return max(RETRY_AFTER_MIN_SEC, min(RETRY_AFTER_MAX_SEC, value))


# ---------------------------------------------------------------------------
# Bridge: simple retry with fixed backoff (like Orthanc)
# ---------------------------------------------------------------------------
def _http_retry(request_func, max_retries=3, delay=2.0):
    """Execute HTTP request with fixed backoff  -  3 attempts, 2s delay.

    Retry on: 429, 5xx, timeouts, connection errors.
    No retry on: 4xx except 429 (client errors are permanent).

    Returns httpx.Response, including the final 5xx; raises the last
    transport exception (timeout / connection error).
    """
    last_exception = None

    for attempt in range(max_retries + 1):
        try:
            resp = request_func()
            if resp.status_code == 429:
                if attempt < max_retries:
                    stow_retries.inc()
                    time.sleep(delay)
                    continue
                return resp
            if resp.status_code == 507:
                # 507 Insufficient Storage: NO retry inside _http_retry.
                # Return immediately; the outbox worker applies Retry-After
                # backpressure (clamped 5..300 s) and keeps files pending.
                return resp
            if 400 <= resp.status_code < 500:
                return resp
            if resp.status_code < 500:
                return resp
            logging.warning(
                "[RETRY] HTTP %d on attempt %d/%d",
                resp.status_code,
                attempt + 1,
                max_retries + 1,
            )
            # Final 5xx: return the response so the caller's
            # raise_for_status() / _http_status_to_dimse maps it (5xx ->
            # 0xA700 transient), not a bare Exception that escapes C-FIND
            # generators and mis-maps to fatal.
            if attempt == max_retries:
                return resp
        except (httpx.TimeoutException, httpx.ConnectError) as e:
            logging.warning(
                "[RETRY] %s on attempt %d/%d: %s",
                type(e).__name__,
                attempt + 1,
                max_retries + 1,
                e,
            )
            last_exception = e
        except httpx.TransportError as e:
            # Every other transport failure - a reset mid-response, a read
            # timeout, a truncated body - is equally transient and equally
            # retryable. Letting those escape aborted the batch on the first
            # hiccup while the two named subclasses were retried.
            logging.warning(
                "[RETRY] transport %s on attempt %d/%d: %s",
                type(e).__name__,
                attempt + 1,
                max_retries + 1,
                e,
            )
            last_exception = e

        if attempt < max_retries:
            stow_retries.inc()
            time.sleep(delay)

    raise last_exception or Exception("Max retries exceeded")


# ---------------------------------------------------------------------------
# Bridge: QIDO-RS with retry
# ---------------------------------------------------------------------------
class QidoMalformedBodyError(ValueError):
    """A 2xx QIDO-RS answer whose body is not a DICOM JSON array.

    Separate from an empty result: `[]` means the query ran and matched
    nothing, whereas this means the answer could not be read at all.
    """


def _qido_request(url, params, http_timeout=30, limit=None, offset=0, tls=None):
    """QIDO-RS GET with retry wrapper. Auth by nginx.

    Args:
        limit: if set, adds ?limit=N&offset=M to request (server-side pagination).
        offset: pagination offset, only used when limit is set.
        tls: optional (cert_path, key_path) tuple for mTLS. verify always on.

    The QA view is chosen by the CALLER through the base URL (see
    `qa_base_url`), not by a query parameter: the bridge is a universal
    DICOMweb client, so it must not depend on any one server's private
    parameter for this.
    """
    if limit is not None:
        params = dict(params)
        params["limit"] = str(limit)
        params["offset"] = str(offset)

    def _do_qido():
        client = httpx.Client(timeout=httpx.Timeout(http_timeout + 10.0), cert=tls)
        try:
            return client.get(
                url,
                params=params,
                headers={"Accept": "application/dicom+json"},
            )
        finally:
            client.close()

    resp = _http_retry(_do_qido)
    resp.raise_for_status()
    # PS3.18 10.6: a search with no matches may answer 204 No Content, which
    # carries no body at all. `json()` on it raises ValueError, and the C-FIND
    # callers only catch httpx errors - a no-match search would surface as a
    # tooling failure instead of an empty result set. The same applies to an
    # intermediary that answers 200 with an empty or non-JSON body.
    # PS3.18 10.6.1: 204 is the explicit "nothing to return" answer and stays
    # an empty result. A 200 with no body is NOT that: the server claimed to
    # answer a query and delivered nothing readable, which is the same failure
    # class as an HTML error page and must not read as "matched nothing".
    if resp.status_code == 204:
        return []
    if not resp.content:
        raise QidoMalformedBodyError(f"{url} answered 200 with an empty body")
    try:
        body = resp.json()
    except ValueError:
        # PS3.18 10.6.1: a successful QIDO-RS response is a JSON array. A 200
        # carrying HTML/plain text is a proxy or an error page, not an empty
        # answer - and returning `[]` made it indistinguishable from "the
        # study has no matches", so a caller that counted on a real result got
        # a silent, confidently wrong zero. Signal it as a failure instead.
        raise QidoMalformedBodyError(
            f"{url} answered 200 with a body that is not JSON ({len(resp.content)} bytes)"
        )
    if not isinstance(body, list):
        # A DICOM JSON object where the array belongs is the same class of
        # breakage: it states nothing about matches.
        raise QidoMalformedBodyError(
            f"{url} answered 200 with a JSON {type(body).__name__}, expected an array"
        )
    return body


# ---------------------------------------------------------------------------
# Bridge: WADO-RS metadata lookup with retry
# ---------------------------------------------------------------------------
def _wado_metadata_request(url, http_timeout=30, tls=None):
    """WADO-RS metadata GET with retry wrapper. Auth by nginx."""

    def _do_wado():
        client = httpx.Client(timeout=httpx.Timeout(http_timeout + 10.0), cert=tls)
        try:
            return client.get(
                url,
                headers={"Accept": "application/dicom+json"},
            )
        finally:
            client.close()

    return _http_retry(_do_wado)


# ---------------------------------------------------------------------------
# 2409: Storage Commitment (PS3.18 13.4/13.5) - POST commit, GET check
# ---------------------------------------------------------------------------
# DICOM JSON keys (Annex J.2), the same ones the server's commitment_pure
# module defines. Only the INSTANCE form of the Referenced SOP Sequence is
# used here: the bridge always has SOP Instance UIDs to commit.
_STC_REFERENCED_SOP_SEQUENCE = "00081199"
_STC_FAILED_SOP_SEQUENCE = "00081198"
_STC_FAILURE_REASON = "00081197"
_STC_REFERENCED_SOP_CLASS_UID = "00081150"
_STC_REFERENCED_SOP_INSTANCE_UID = "00081155"

# The only FailureReason values PS3.3 C.14.1.1 enumerates for the attribute.
# The server emits a subset of these; the table is total so a future server-
# side addition can never push a non-enumerated code onto the DIMSE wire.
_STC_NO_SUCH_SOP = 0x0112
_STC_PROCESSING_FAILURE = 0x0110
_STC_RESOURCE_LIMITATION = 0x0213

_STC_REASON_MAP = {
    "0110": _STC_PROCESSING_FAILURE,
    "0112": _STC_NO_SUCH_SOP,
    "0213": _STC_RESOURCE_LIMITATION,
}

# Sentinel on the return value for "the origin has no Commit resource".
# The caller logs the misconfiguration WARN; there is deliberately NO
# fallback to the local path (a silent fallback would mask the wrong knob
# forever).
STC_NO_RESOURCE = "no_commit_resource"


def _stc_commit_request(clarus_url, tx_uid, sop_list, http_timeout=30, tls=None):
    """POST one Storage Commitment request and map the verdict.

    Returns ``(committed, failed, sentinel)``:

      committed: [(sop_class, sop_uid)]            - the origin committed these
      failed:    [(sop_class, sop_uid, reason_int)] - enumerated C.14.1.1 codes
      sentinel:  None, or STC_NO_RESOURCE when the origin answered 404/405

    409 (duplicate Transaction UID) replays the stored verdict through ONE
    GET of the same path. Every other non-200 outcome maps to "all failed
    0x0110" - the bridge reports what it knows, it never guesses.

    ``sop_list`` is [(sop_class_uid, sop_instance_uid), ...]; the body is the
    INSTANCE form of the Referenced SOP Sequence (commitment_pure.py).
    """
    base = dicomweb_base(clarus_url)
    url = f"{base}/commitment-requests/{tx_uid}"

    items = []
    for sop_class, sop_uid in sop_list:
        items.append(
            {
                _STC_REFERENCED_SOP_CLASS_UID: {"vr": "UI", "Value": [sop_class]},
                _STC_REFERENCED_SOP_INSTANCE_UID: {"vr": "UI", "Value": [sop_uid]},
            }
        )
    body = {_STC_REFERENCED_SOP_SEQUENCE: {"vr": "SQ", "Value": items}}

    def _do_commit():
        client = httpx.Client(timeout=httpx.Timeout(http_timeout + 10.0), cert=tls)
        try:
            return client.post(
                url,
                json=body,
                headers={
                    "Accept": "application/dicom+json",
                    "Content-Type": "application/dicom+json",
                },
            )
        finally:
            client.close()

    try:
        resp = _http_retry(_do_commit)
    except Exception as e:
        # Retry budget spent on a transport failure or a 5xx.
        logging.warning("[STC] commit tx=%s failed: %s", tx_uid, e)
        return [], _all_failed(sop_list), None

    status = resp.status_code

    if status in (404, 405):
        logging.warning(
            "[STC] origin answers %d on commitment-requests; set "
            "storage_commitment_presence=false for this origin",
            status,
        )
        return [], _all_failed(sop_list), STC_NO_RESOURCE

    if status == 409:
        # The Transaction UID was already committed: the verdict is stored,
        # not re-derived. ONE GET, and an idempotent answer is reported.
        return _stc_check_request(clarus_url, tx_uid, sop_list, http_timeout, tls=tls)

    if status != 200:
        logging.warning(
            "[STC] commit tx=%s: HTTP %d -> all failed 0x0110 (%s)",
            tx_uid,
            status,
            resp.text[:200],
        )
        return [], _all_failed(sop_list), None

    try:
        payload = resp.json()
    except Exception as e:
        logging.warning("[STC] commit tx=%s: unparsable body (%s)", tx_uid, e)
        return [], _all_failed(sop_list), None

    return _stc_map_verdict(payload, sop_list, tx_uid)


def _stc_check_request(clarus_url, tx_uid, sop_list, http_timeout=30, tls=None):
    """GET the stored verdict for a Transaction UID (PS3.18 13.5).

    Used for the 409 replay path: the origin already has a verdict for this
    Transaction UID and re-committing must not change it. A 404 (the result
    was swept by retention) is every SOP failed 0x0110.
    """
    base = dicomweb_base(clarus_url)
    url = f"{base}/commitment-requests/{tx_uid}"

    def _do_check():
        client = httpx.Client(timeout=httpx.Timeout(http_timeout + 10.0), cert=tls)
        try:
            return client.get(url, headers={"Accept": "application/dicom+json"})
        finally:
            client.close()

    try:
        resp = _http_retry(_do_check)
    except Exception as e:
        logging.warning("[STC] check tx=%s failed: %s", tx_uid, e)
        return [], _all_failed(sop_list), None

    if resp.status_code in (404, 405):
        logging.warning(
            "[STC] check tx=%s: HTTP %d (result not stored) -> all failed 0x0110",
            tx_uid,
            resp.status_code,
        )
        return [], _all_failed(sop_list), None

    if resp.status_code != 200:
        logging.warning(
            "[STC] check tx=%s: HTTP %d -> all failed 0x0110",
            tx_uid,
            resp.status_code,
        )
        return [], _all_failed(sop_list), None

    try:
        payload = resp.json()
    except Exception as e:
        logging.warning("[STC] check tx=%s: unparsable body (%s)", tx_uid, e)
        return [], _all_failed(sop_list), None

    return _stc_map_verdict(payload, sop_list, tx_uid)


def _all_failed(sop_list):
    """Every SOP failed with the general processing failure code."""
    return [
        (sop_class, sop_uid, _STC_PROCESSING_FAILURE) for sop_class, sop_uid in sop_list
    ]


def _stc_map_verdict(payload, sop_list, tx_uid):
    """Map a Commit/Check 200 body onto (committed, failed, None).

    Committed SOPs come from ReferencedSOPSequence (0008,1199); failed ones
    from FailedSOPSequence (0008,1198) with FailureReason (0008,1197) read as
    a STRING (DICOM JSON renders numeric VRs as strings). A SOP absent from
    BOTH sequences is a protocol defect of the origin and is reported as
    failed 0x0110 - the bridge reports, it does not guess.
    """
    committed = []
    failed = []
    seen = set()

    for item in _stc_sequence_items(payload, _STC_REFERENCED_SOP_SEQUENCE):
        pair = _stc_pair(item)
        if pair is None:
            continue
        seen.add(_stc_key(pair))
        committed.append(pair)

    for item in _stc_sequence_items(payload, _STC_FAILED_SOP_SEQUENCE):
        pair = _stc_pair(item)
        if pair is None:
            continue
        reason = _stc_reason(item)
        seen.add(_stc_key(pair))
        failed.append((pair[0], pair[1], reason))

    for sop_class, sop_uid in sop_list:
        if (str(sop_class), str(sop_uid)) not in seen:
            logging.warning(
                "[STC] tx=%s: SOP %s is in neither response sequence -> 0x0110",
                tx_uid,
                sop_uid,
            )
            failed.append((sop_class, sop_uid, _STC_PROCESSING_FAILURE))

    return committed, failed, None


def _stc_sequence_items(payload, tag):
    """The Value items of one sequence tag, or [] when absent/malformed."""
    if not isinstance(payload, dict):
        return []
    seq = payload.get(tag)
    if not isinstance(seq, dict):
        return []
    items = seq.get("Value")
    if not isinstance(items, list):
        return []
    return [item for item in items if isinstance(item, dict)]


def _stc_pair(item):
    """(sop_class, sop_uid) from one sequence item, or None when incomplete."""
    sop_class = _stc_single(item, _STC_REFERENCED_SOP_CLASS_UID)
    sop_uid = _stc_single(item, _STC_REFERENCED_SOP_INSTANCE_UID)
    if sop_class is None or sop_uid is None:
        return None
    return (sop_class, sop_uid)


def _stc_single(item, tag):
    """One string out of a DICOM JSON element's Value list."""
    element = item.get(tag)
    if not isinstance(element, dict):
        return None
    values = element.get("Value")
    if not isinstance(values, list) or not values:
        return None
    return str(values[0])


def _stc_key(pair):
    return (str(pair[0]), str(pair[1]))


def _stc_reason(item):
    """FailureReason as a DIMSE code; anything unenumerated is 0x0110."""
    raw = _stc_single(item, _STC_FAILURE_REASON)
    if raw is None:
        return _STC_PROCESSING_FAILURE
    return _STC_REASON_MAP.get(str(raw).strip(), _STC_PROCESSING_FAILURE)


# ---------------------------------------------------------------------------
# Bridge: HTTP status -> DIMSE status mapping
# ---------------------------------------------------------------------------
def _http_status_to_dimse(http_status):
    """Map HTTP status code to DIMSE (status, description) tuple (BR-011).

    Transient errors (0xA700)  -  bridge should retry with backoff.
    Fatal errors (0xC000)  -  bridge should NOT retry.
    Success (0x0000)  -  delivered.
    """
    # -- Transient errors (retryable) --
    TRANSIENT = {
        429: "Too Many Requests - rate limited, retry after delay",
        500: "Internal Server Error - server malfunction",
        502: "Bad Gateway - storage backend error",
        503: "Service Unavailable - server busy or down",
        504: "Gateway Timeout - database operation timed out",
        507: "Insufficient Storage - disk full on server",
    }
    if http_status in TRANSIENT:
        return (0xA700, http_status, TRANSIENT[http_status])

    # -- Fatal errors (do NOT retry) --
    FATAL = {
        400: "Bad Request - invalid DICOM data",
        401: "Unauthorized - check Clarus credentials",
        403: "Forbidden - insufficient permissions",
        404: "Not Found - study/series/instance missing",
        405: "Method Not Allowed - wrong HTTP method",
        406: "Not Acceptable - unsupported Transfer Syntax",
        409: "Conflict - SOP Instance UID conflict",  # Note: 409 succeeds for idempotent delivery
        413: "Payload Too Large - dataset exceeds server limit",
        415: "Unsupported Media Type - wrong Content-Type",
        416: "Range Not Satisfiable - invalid byte range",
        422: "Unprocessable Content - DICOM validation failed",
    }
    if http_status in FATAL:
        return (0xC000, http_status, FATAL[http_status])

    # -- Success codes --
    if 200 <= http_status < 300:
        return (0x0000, http_status, "OK")

    # -- Unknown  -  conservative: treat as transient for safety --
    if 500 <= http_status < 600:
        return (0xA700, http_status, f"Unknown server error HTTP {http_status}")
    return (0xC000, http_status, f"Unexpected HTTP {http_status}")


# ---------------------------------------------------------------------------
# Bridge: STOW-RS response parser  -  detect per-SOP failures
# ---------------------------------------------------------------------------
class StowBodyNotJsonObject(ValueError):
    """A STOW-RS response body parsed to something that is not a JSON object.

    PS3.18 10.5.2 makes the STOW response a DICOM JSON OBJECT (an array or a
    bare scalar is not one). Rejecting it here turns what used to be an
    AttributeError escaping the handler into a structured 4xx the caller can
    act on.
    """


def _stow_body_dict(response_text):
    """Parse a STOW-RS body and return the JSON object it must contain.

    Returns None when the body is empty or not JSON at all (the caller treats
    that as "no failure report"). Raises `StowBodyNotJsonObject` when the body
    IS JSON but is not an object - a list or a scalar - because silently
    returning [] there reported a malformed answer as a clean one.
    """
    try:
        body = json.loads(response_text)
    except (json.JSONDecodeError, TypeError):
        return None
    if not isinstance(body, dict):
        raise StowBodyNotJsonObject(
            f"STOW-RS body is {type(body).__name__}, not a JSON object"
        )
    return body


def _stow_seq_value(tag, seq_item):
    """The `Value` list of one DICOM JSON tag element, type-checked (PS3.18 J.2).

    A tag element is an object with an optional `Value` array. Anything else -
    a bare string, a list where the element should be, a `Value` that is not a
    list - used to reach `.get`/`[0]` and raise AttributeError, which the wide
    `except Exception` in the callers mapped to 0xC000: a PERMANENT refusal
    for a body that is merely malformed. Corruption is transient, so it raises
    `StowBodyNotJsonObject` and the caller answers 0xA700 (retryable) instead.
    """
    if not isinstance(seq_item, dict):
        raise StowBodyNotJsonObject(
            f"failed-SOP item({tag}) is {type(seq_item).__name__}, not an object"
        )
    values = seq_item.get("Value", [])
    if values is None:
        values = []
    if not isinstance(values, list):
        raise StowBodyNotJsonObject(
            f"failed-SOP item({tag}) Value is {type(values).__name__}, not a list"
        )
    return values


def _parse_stow_failures_all(response_text):
    """Parse STOW-RS JSON for ALL failed SOPs (0008,1198 Failed SOP Sequence).

    Server returns HTTP 202 even when individual SOPs in a batch fail.
    Returns a list of {"sop_uid": str, "reason": str}; empty when none failed.
    Raises `StowBodyNotJsonObject` when the body is JSON but not an object, or
    when a failed-SOP item does not follow the DICOM JSON shape.
    """
    body = _stow_body_dict(response_text)
    if body is None:
        return []

    failed_seq = body.get("00081198")
    if not isinstance(failed_seq, dict):
        return []

    items = failed_seq.get("Value", [])
    if items is None:
        return []
    if not isinstance(items, list):
        raise StowBodyNotJsonObject(
            f"00081198 Value is {type(items).__name__}, not a list"
        )
    if not items:
        return []

    failures = []
    seq_items = []
    for item in items:
        if not isinstance(item, dict):
            raise StowBodyNotJsonObject(
                f"00081198 item is {type(item).__name__}, not an object"
            )
        seq_items.append(item)

    for item in seq_items:
        # 00081197 = Failure Reason (US, numeric code)
        codes = _stow_seq_value("00081197", item.get("00081197", {}))
        failure_code = codes[0] if codes else "?"

        # 00000902 = Error Comment (LO, human-readable)
        comments = _stow_seq_value("00000902", item.get("00000902", {}))
        error_comment = comments[0] if comments else "Unknown reason"

        # 00081155 = Referenced SOP Instance UID (PS3.18 sec.10.5.2.1)
        ref_sops = _stow_seq_value("00081155", item.get("00081155", {}))
        ref_sop = ref_sops[0] if ref_sops else "?"

        failures.append(
            {
                "sop_uid": ref_sop,
                "reason": f"[code={failure_code}] {error_comment}",
            }
        )
    return failures


def _parse_stow_failures(response_text, sop_instance_uid):
    """Parse STOW-RS JSON response for the FIRST Failed SOP (batch-compatible).

    Server returns HTTP 202 even when individual SOPs fail.
    Returns dict {"sop_uid", "reason"} if rejected, None if accepted.
    """
    failures = _parse_stow_failures_all(response_text)
    if not failures:
        return None
    return failures[0]


def _ensure_stow_ts(dataset):
    """Ensure dataset has file_meta.TransferSyntaxUID for dcmwrite.

    pynetdicom may strip file_meta from network-received datasets (BR-020).
    Only needed when re-serializing (modified instances). Raw pass-through
    (raw_bytes != None) keeps the original bytes and never reaches here.
    Note: pydicom Dataset.__getattr__ raises AttributeError, not returns None.
    """
    try:
        _ = dataset.file_meta.TransferSyntaxUID
    except AttributeError:
        from pydicom.dataset import FileMetaDataset

        dataset.file_meta = FileMetaDataset()
        dataset.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian


def stow_single_instance(
    dataset, clarus_url, http_timeout=30, raw_bytes=None, client=None, tls=None
):
    """STOW-RS single instance with retry (BR-002). Auth by nginx.

    Args:
        clarus_url: DICOMweb base URL (single knob, 2908). Trailing slash
            is stripped by dicomweb_base().
        raw_bytes: if provided, use these EXACT bytes as DICOM payload.
            Bypasses dcmwrite  -  preserves byte-identical content for CAS dedup.
            When None, dataset is re-serialized via dcmwrite (modified instance).
        client: optional pre-created httpx.Client for connection reuse.
            When None, creates and closes a new Client per call.
            P30: use a persistent Client for Keep-Alive (x2 throughput).
        tls: optional (cert_path, key_path) tuple for mTLS. verify is
            ALWAYS on (httpx default)  -  no knob to disable it.

    Returns: (dimse_status, http_status, msg).
    """
    # ensure file_meta TransferSyntaxUID before any dcmwrite re-serialization.
    if raw_bytes is None:
        _ensure_stow_ts(dataset)

    # P8: raw pass-through  -  use exact bytes when no modifications were applied.
    # This preserves byte-identical content for CAS dedup (avoids dcmwrite
    # re-serialization which changes File Meta Information, tag ordering, padding).
    if raw_bytes is not None:
        dicom_bytes = raw_bytes
    else:
        # R4: an instance that cannot be re-serialized (an invalid value for
        # its VR) is a verdict about THIS instance, not a crash. Letting
        # dcmwrite raise here escaped before the function returned its
        # promised three-tuple, and the caller's broad handler then answered
        # for the whole request.
        try:
            buf = io.BytesIO()
            dcmwrite(buf, dataset, enforce_file_format=True)
            dicom_bytes = buf.getvalue()
        except Exception as e:
            logging.error("[STOW-RS] could not serialize the instance: %s", e)
            return (0xC000, None, f"Serialize error: {e}")

    boundary = f"BOUNDARY_{uuid.uuid4().hex[:16]}"
    body = (
        (f"--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").encode("ascii")
        + dicom_bytes
        + (f"\r\n--{boundary}--\r\n").encode("ascii")
    )

    # P8: httpx with total timeout  -  prevents indefinite hangs on TCP accept.
    # requests(timeout=30) only covers connect+read, not total elapsed time.
    # httpx.Timeout(N) covers the entire request lifecycle.
    total_timeout = http_timeout + 10
    base = dicomweb_base(clarus_url)
    stow_url = f"{base}/studies"

    # P30: Keep-Alive  -  reuse client for connection persistence (x2 throughput).
    # Without Keep-Alive: TCP-handshake (~30-100ms) on every slice.
    # With Keep-Alive: one handshake for the entire queue drain.
    def _do_stow():
        if client is not None:
            # Persistent client  -  connection reuse via Keep-Alive
            return client.post(
                stow_url,
                content=body,
                headers={
                    "Content-Type": (
                        f'multipart/related; type="application/dicom"; '
                        f"boundary={boundary}"
                    ),
                    "Accept": "application/dicom+json",
                },
            )
        # Fallback: create+close client per request (backward compat)
        _c = httpx.Client(timeout=httpx.Timeout(total_timeout), cert=tls)
        try:
            return _c.post(
                stow_url,
                content=body,
                headers={
                    "Content-Type": (
                        f'multipart/related; type="application/dicom"; '
                        f"boundary={boundary}"
                    ),
                    "Accept": "application/dicom+json",
                },
            )
        finally:
            _c.close()

    try:
        resp = _http_retry(_do_stow)
        # Bridge: STOW requests counter by HTTP status
        stow_requests.labels(status=str(resp.status_code)).inc()
        sop_uid = str(getattr(dataset, "SOPInstanceUID", "?"))

        if resp.status_code in (200, 201, 202):
            # Bridge: parse STOW-RS JSON for per-SOP failures. A body that is
            # not a DICOM JSON object is the server answering the wrong thing:
            # report it as a rejected instance rather than letting the parse
            # exception escape the C-STORE handler.
            try:
                rejection = _parse_stow_failures(resp.text, sop_uid)
            except StowBodyNotJsonObject as e:
                logging.error("[STOW-RS] malformed response body: %s", e)
                return (0xA700, resp.status_code, str(e))
            if rejection:
                logging.error(
                    "[STOW-RS] Server rejected SOP %s: %s - check clarus-server logs",
                    rejection["sop_uid"],
                    rejection["reason"],
                )
                return (0xA700, resp.status_code, rejection["reason"])

            logging.debug("-> STOW-RS: %d OK (SOP: %s)", resp.status_code, sop_uid)
            return (0x0000, resp.status_code, "OK")

        # Disk full
        if resp.status_code == 507:
            backoff = retry_after_seconds(resp.headers)
            logging.error(
                "-> STOW-RS: 507 DISK FULL (retry after %ss)",
                backoff,
            )
            return (0xA700, 507, f"Disk full - retry after {backoff}s")

        # use _http_status_to_dimse for all non-2xx responses
        return _http_status_to_dimse(resp.status_code)
    except httpx.ConnectError:
        logging.warning("-> STOW-RS: CONNECTION REFUSED")
        stow_requests.labels(status="error").inc()
        return (0xA700, None, "Connection refused")
    except httpx.TimeoutException:
        logging.warning("-> STOW-RS: TOTAL TIMEOUT after %.0fs", total_timeout)
        stow_requests.labels(status="error").inc()
        return (0xA700, None, "Total timeout")
    except httpx.TransportError as e:
        # ReadError / WriteError / RemoteProtocolError: the TCP link broke
        # mid-request. This is TRANSIENT (retryable), not a permanent verdict:
        # answering 0xC000 told the modality the instance was refused and must
        # never be re-sent, so a single blip lost data. 0xA700 (Out of
        # Resources) is the retryable refusal.
        logging.warning("-> STOW-RS: TRANSPORT ERROR (%s)", e)
        stow_requests.labels(status="error").inc()
        return (0xA700, None, str(e))
    except Exception as e:
        logging.warning("-> STOW-RS: ERROR (%s)", e)
        logging.debug("-> STOW-RS: Full traceback follows:", exc_info=True)
        stow_requests.labels(status="error").inc()
        return (0xC000, None, str(e))


def stow_batch_instances(payloads, clarus_url, http_timeout=30, client=None, tls=None):
    """STOW-RS batch  -  N instances in ONE multipart/related request.

    One POST per study-batch (fieldtest: 1063 POSTs -> ~20). P8 raw
    pass-through per part: byte-identical content for CAS dedup, no
    dcmwrite re-serialization unless the instance was modified.

    Args:
        payloads: list of (sop_uid, ds, raw_bytes).
            raw_bytes: exact bytes (None -> re-serialize modified ds).
        clarus_url: DICOMweb base URL (single knob, 2908).
        client: optional pre-created httpx.Client (Keep-Alive, P30).
        tls: optional (cert_path, key_path) tuple for mTLS.

    Returns: (results, retry_after) tuple.
        results: per-SOP list, in payload order:
            [(sop_uid, dimse_status, http_status, msg), ...]
            dimse_status mirrors stow_single_instance (0x0000 / 0xA700 / 0xC000).
        retry_after: seconds to pause on HTTP 507 (clamped 5..300), or None.
    """
    if not payloads:
        return [], None

    boundary = f"BOUNDARY_{uuid.uuid4().hex[:16]}"
    total_timeout = http_timeout + 10
    base = dicomweb_base(clarus_url)
    stow_url = f"{base}/studies"

    body = bytearray()
    pre_failed = []
    for sop_uid, ds, raw_bytes in payloads:
        if raw_bytes is None:
            # R4: re-serialization is per-SOP work and can raise on ONE bad
            # instance (an invalid value for its VR). Outside this guard that
            # exception escaped before the function returned the `(results,
            # retry_after)` tuple it promises, so the whole batch had no
            # outcome and the retry re-sent every other SOP with it. A failed
            # part is reported as 0xC000 (this instance is unwritable) and the
            # rest of the batch still goes.
            try:
                _ensure_stow_ts(ds)
                buf = io.BytesIO()
                dcmwrite(buf, ds, enforce_file_format=True)
                dicom_bytes = buf.getvalue()
            except Exception as e:
                logging.error(
                    "[STOW-RS] could not serialize SOP %s for the batch: %s",
                    sop_uid,
                    e,
                )
                pre_failed.append((sop_uid, 0xC000, None, f"Serialize error: {e}"))
                continue
        else:
            dicom_bytes = raw_bytes
        body += b"--" + boundary.encode("ascii") + b"\r\n"
        body += b"Content-Type: application/dicom\r\n\r\n"
        body += dicom_bytes
        body += b"\r\n"
    body += b"--" + boundary.encode("ascii") + b"--\r\n"

    failed_uids = {entry[0] for entry in pre_failed}
    live_payloads = [
        (uid, ds, raw) for uid, ds, raw in payloads if uid not in failed_uids
    ]
    if not live_payloads:
        # Every part was unwritable: nothing to POST, and the caller still gets
        # the promised tuple with the per-SOP verdicts.
        return pre_failed, None

    headers = {
        "Content-Type": (
            f'multipart/related; type="application/dicom"; ' f"boundary={boundary}"
        ),
        "Accept": "application/dicom+json",
    }

    def _do_stow():
        if client is not None:
            return client.post(
                stow_url,
                content=bytes(body),
                headers=headers,
            )
        _c = httpx.Client(timeout=httpx.Timeout(total_timeout), cert=tls)
        try:
            return _c.post(
                stow_url,
                content=bytes(body),
                headers=headers,
            )
        finally:
            _c.close()

    try:
        resp = _http_retry(_do_stow)
        stow_requests.labels(status=str(resp.status_code)).inc()

        if resp.status_code in (200, 201, 202):
            # Per-SOP outcome: server may reject a subset (0008,1198) in a 2xx.
            try:
                failed = _parse_stow_failures_all(resp.text)
            except StowBodyNotJsonObject as e:
                # PS3.18 10.5.2: the response must be a DICOM JSON object. A
                # list or scalar body states nothing about the batch, so it is
                # a server-side reject, not a clean answer - and it must be
                # returned in the SAME `(results, retryafter)` shape as every
                # other outcome, or the caller unpacks two values from a
                # three-tuple and dies with ValueError.
                logging.error("[STOW-RS] malformed batch response body: %s", e)
                return pre_failed + [
                    (uid, 0xA700, resp.status_code, f"Malformed response: {e}")
                    for uid, _ds, _raw in live_payloads
                ], None
            failed_by_uid = {f["sop_uid"]: f["reason"] for f in failed}
            sent = {uid for uid, _ds, _raw in live_payloads}
            # PS3.18 10.5: a rejection the server reported for a SOP the client
            # did not send means the two sides disagree about the batch. Marking
            # every sent SOP as delivered would claim a success nobody stated,
            # so the whole batch is retried.
            unmatched = [f for f in failed if f["sop_uid"] not in sent]
            if unmatched:
                logging.error(
                    "[STOW-RS] Server rejected %d SOP(s) not in this batch - "
                    "whole batch retried",
                    len(unmatched),
                )
                return pre_failed + [
                    (uid, 0xA700, resp.status_code, "Unmatched failure response")
                    for uid, _ds, _raw in live_payloads
                ], None
            results = list(pre_failed)
            for sop_uid, ds, raw_bytes in live_payloads:
                if sop_uid in failed_by_uid:
                    results.append(
                        (sop_uid, 0xA700, resp.status_code, failed_by_uid[sop_uid])
                    )
                else:
                    results.append((sop_uid, 0x0000, resp.status_code, "OK"))
            if failed:
                logging.error(
                    "[STOW-RS] Batch: %d/%d SOPs rejected by server - "
                    "will re-send individually on retry",
                    len(failed),
                    len(live_payloads),
                )
            return results, None

        # / Disk full  -  whole batch retryable.
        # 2908: _http_retry does NOT retry 507; backoff comes from the
        # Retry-After header (clamped 5..300 s, default 30 s). The outbox
        # worker pauses and keeps the files pending.
        if resp.status_code == 507:
            backoff = retry_after_seconds(resp.headers)
            logging.error(
                "-> STOW-RS: 507 DISK FULL (retry after %ss)",
                backoff,
            )
            msg = f"Disk full - retry after {backoff}s"
            return pre_failed + [
                (uid, 0xA700, 507, msg) for uid, _, _ in live_payloads
            ], backoff

        # non-2xx  -  whole batch shares one outcome.
        dimse_status, _, msg = _http_status_to_dimse(resp.status_code)
        return (
            pre_failed
            + [
                (uid, dimse_status, resp.status_code, msg)
                for uid, _, _ in live_payloads
            ],
            None,
        )
    except httpx.ConnectError:
        logging.warning("-> STOW-RS: CONNECTION REFUSED")
        stow_requests.labels(status="error").inc()
        return pre_failed + [
            (uid, 0xA700, None, "Connection refused") for uid, _, _ in live_payloads
        ], None
    except httpx.TimeoutException:
        logging.warning("-> STOW-RS: TOTAL TIMEOUT after %.0fs", total_timeout)
        stow_requests.labels(status="error").inc()
        return pre_failed + [
            (uid, 0xA700, None, "Total timeout") for uid, _, _ in live_payloads
        ], None
    except httpx.TransportError as e:
        # Transient mid-request transport break (Read/Write/RemoteProtocol):
        # retryable, so 0xA700 per SOP, not the permanent 0xC000.
        logging.warning("-> STOW-RS: TRANSPORT ERROR (%s)", e)
        stow_requests.labels(status="error").inc()
        return pre_failed + [
            (uid, 0xA700, None, str(e)) for uid, _, _ in live_payloads
        ], None
    except Exception as e:
        logging.warning("-> STOW-RS: ERROR (%s)", e)
        logging.debug("-> STOW-RS: Full traceback follows:", exc_info=True)
        stow_requests.labels(status="error").inc()
        return pre_failed + [
            (uid, 0xC000, None, str(e)) for uid, _, _ in live_payloads
        ], None
