# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  C-MOVE handler: WADO-RS streaming + C-STORE forwarding.

handle_c_move: accepts C-MOVE-RQ, creates job in queue, returns Pending.
_execute_cmove_job: streams study from Clarus, sends instances to target SCP.
_send_storage_commitment: pushes Storage Commitment N-ACTION after successful C-MOVE.

MAJ-002: _execute_cmove_job split into _associate_for_move() and _stream_and_send_study().
"""

import io
import json
import logging
import re
import shutil
import struct
import threading
import time
from urllib.parse import quote as _quote

# HTTP client + URL helpers for the R1 pre-flight (QIDO series lookup uses
# httpx via _qido_request; the per-series WADO metadata GET uses plain
# requests).
import httpx as _httpx
import requests
from pydicom import Dataset, dcmread
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    generate_uid,
)
from pynetdicom import AE
from pynetdicom.sop_class import (
    StorageCommitmentPushModel,
    Verification,
)

from clean_code.http_helpers import (
    _qido_request,
    dicomweb_base,
    service_base_url,
)
from clean_code.metrics import (
    ae_label,
    associations_total,
    bytes_total,
    cstore_outbound_duration,
    move_jobs_counter,
)
from clean_code.prefetch import PartPrefetcher
from clean_code.protocol import (
    JPEGLS_LOSSLESS_TS,
    _dimse_status_name,
    _json_first_value,
    canonical_ts_sort_key,
)
from clean_code.registry import _BUILTIN_SOP_CLASSES, _get_sop_registry_uids
from clean_code.streaming import (
    SPILL_THRESHOLD,
    SpilledPart,
    _stream_multipart_instances,
)
from clean_code.utils import _shutdown_requested, mask_patient_id

# TS-order constants (IMAGE_TS EVRLE-first etc.) live in clean_code.protocol
# (single definition - the bundle warns on duplicate module-level names).
# The main C-MOVE outbound association proposes each SOP class with its
# uncompressed family (EVRLE, IVRLE) first and then any stored/compressed
# transfer syntaxes from the pre-flight; negotiation is acceptor-driven, so a
# 4.80-capable viewer still negotiates 4.80 and a stored 4.80 sends as-is on
# the accepted 4.80 context.

# ---------------------------------------------------------------------------
# No probe/second association: the bridge NEVER opens one to a C-MOVE
# destination. The main association is the ONLY delivery association; a stored
# TS the viewer did not accept is either refetched on the MAIN association (if
# the server can produce an accepted TS) or fails honestly. There is no
# fallback/probe/reject-cache machinery. Retrying the main association is
# governed by the single outbound_assoc_attempts knob (PS3.8 9.1.4 Note 2
# leaves retry policy to the implementation).
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# simple bool to suppress pynetdicom noise during streaming/SC
# ---------------------------------------------------------------------------
_suppress_pynetdicom = False


class _SuppressPynetdicomFilter(logging.Filter):
    """Simple filter: suppress pynetdicom logs when _suppress_pynetdicom is True."""

    def filter(self, record):
        return not _suppress_pynetdicom


_pynetdicom_suppress = _SuppressPynetdicomFilter()
logging.getLogger("pynetdicom").addFilter(_pynetdicom_suppress)

# Enrich pynetdicom timeout messages with context: which timer,
# inbound/outbound, AE title, timeout value.
import pynetdicom.association as _assoc_module

_original_assoc_error = _assoc_module.LOGGER.error


def _patched_assoc_error(msg, *args, **kwargs):
    if msg == "Network timeout reached":
        msg = (
            "Network timeout reached (idle socket > network_timeout - "
            "check inbound_network_timeout / outbound_network_timeout in config)"
        )
    elif msg and "DIMSE timeout reached" in str(msg):
        msg = (
            "DIMSE timeout reached (no response to command within dimse_timeout - "
            "check inbound_dimse_timeout / outbound_dimse_timeout in config)"
        )
    _original_assoc_error(msg, *args, **kwargs)


_assoc_module.LOGGER.error = _patched_assoc_error


# ---------------------------------------------------------------------------
# Bridge: C-MOVE  -  extract MoveDestination from C-MOVE-RQ
# ---------------------------------------------------------------------------
def _parse_c_move_rq(event):
    """Extract StudyInstanceUID(s) and MoveDestination from C-MOVE-RQ.

    Returns (list_of_study_uids, move_dest_ae).
    Raises ValueError if StudyInstanceUID or MoveDestination is missing.

    MoveDestination (0000,0600) is a DIMSE command field, not an identifier
    element  -  accessed via event.move_destination in pynetdicom.

    A sender may put MULTIPLE StudyInstanceUIDs in a single C-MOVE-RQ.
    Discarding all but the first would silently drop studies, so every
    requested UID is processed.
    """
    move_dest = event.move_destination
    if not move_dest or str(move_dest).strip() == "":
        raise ValueError("C-MOVE-RQ: missing MoveDestination (0000,0600)")

    ds = event.identifier
    study_uid_raw = getattr(ds, "StudyInstanceUID", None)
    if not study_uid_raw or str(study_uid_raw).strip() == "":
        raise ValueError("C-MOVE-RQ: missing StudyInstanceUID (0020,000D)")

    # pynetdicom may return multi-valued StudyInstanceUID as list, tuple,
    # or pydicom MultiString (all iterable, none are str).
    # process ALL StudyInstanceUIDs in the request.
    if isinstance(study_uid_raw, str):
        study_uids = [str(study_uid_raw)]
    else:
        try:
            study_uids = [str(uid) for uid in list(study_uid_raw)]
        except (TypeError, IndexError):
            raise ValueError("C-MOVE-RQ: invalid StudyInstanceUID (0020,000D)")

    if not study_uids:
        raise ValueError("C-MOVE-RQ: empty StudyInstanceUID list")

    return study_uids, str(move_dest)


def _json_loads(payload):
    """json.loads over bytes/str - no new dependency, no external stream."""
    if isinstance(payload, (bytes, bytearray)):
        return json.loads(bytes(payload).decode("utf-8", "replace"))
    return json.loads(payload)


def split_multipart_parts(raw, boundary):
    """Split a multipart body into (headers_bytes, body_bytes) parts.

    Minimal, dependency-free (no python-multipart) parser for the PS3.18
    10.4.4 metadata response. `raw` is the bytes body; `boundary` is the
    Content-Type boundary parameter (already unquoted by the caller). A
    body of two consecutive boundaries (Preamble / Epilogue around the
    parts) is handled by dropping empty segments and lone boundary
    markers. Each returned part is (raw_header_bytes, raw_body_bytes) with
    the CRLFCRLF boundary removed. Returns [] on any structural failure.
    """
    try:
        if not raw or not boundary:
            return []
        if isinstance(raw, str):
            raw = raw.encode("utf-8")
        # Find every boundary line ("--boundary" as a whole line, i.e. on its
        # own line: preceded by CRLF or start-of-string; optionally followed
        # by "--" for the closing "--boundary--").
        delim = re.compile(
            rb"(?:\r\n|\r|\n|^)--" + re.escape(boundary.encode("utf-8")),
            re.MULTILINE,
        )
        # Content between two consecutive boundary lines is one part (headers
        # then a blank line then the body). The preamble before the first
        # boundary (often empty) is skipped.
        matches = list(delim.finditer(raw))
        parts = []
        for k in range(1, len(matches)):
            seg_start = matches[k - 1].end()
            seg_end = matches[k].start()
            # Drop an optional trailing CRLF that ends the previous segment
            # (it belongs to the boundary line break, not the part body).
            if raw[seg_start : seg_start + 2] == b"\r\n":
                seg_start += 2
            elif seg_start < len(raw) and raw[seg_start : seg_start + 1] == b"\n":
                seg_start += 1
            content = raw[seg_start:seg_end].rstrip(b"\r\n")
            # Skip empty segments (double boundaries / stray separators).
            if not content:
                continue
            sep = content.find(b"\r\n\r\n")
            if sep == -1:
                sep2 = content.find(b"\n\n")
                if sep2 == -1:
                    continue  # not a well-formed part (no header/body split)
                header_bytes = content[:sep2]
                body_bytes = content[sep2 + 2 :]
            else:
                header_bytes = content[:sep]
                body_bytes = content[sep + 4 :]
            parts.append((header_bytes, body_bytes))
        return parts
    except Exception:
        return []


# ---------------------------------------------------------------------------
# R1 pre-flight (0609): learn each class's stored TS BEFORE the association.
# Source: the standard per-instance metadata resource (PS3.18 10.4.4)
# /studies/{study}/series/{series}/metadata, whose elements ALWAYS carry
# (0002,0010) TransferSyntaxUID (and (0008,0016) SOPClassUID). Series are
# enumerated via QIDO-RS /studies/{study}/series.
# ---------------------------------------------------------------------------
# TTL cache of successful pre-flights (0609 field): a repeated C-MOVE of
# the same study skips the PS3.18 10.4.4 metadata walk (field: 2.3 s for a
# 432-instance series + parse, plus the association waits on it). Failures
# are NOT cached - the degrade path must retry. A new stored TS appearing
# inside the TTL window stays deliverable: such an instance misses its
# context and takes the legacy fallback path (spec R4 rule 3).
_PREFLIGHT_TTL_S = 120.0
_preflight_cache = {}  # study_uid -> (expires_monotonic, result dict)

# Entries expire on their own deadline, but nothing removed them, so a bridge
# serving many studies grew this dict without bound (each value also holds a
# set per SOP class). The cap is generous against the TTL: at the observed
# rate a full 120 s window holds a few hundred entries, and 1024 is well past
# that while still bounding memory.
_CACHE_MAX_ENTRIES = 1024


def _cache_evict(cache, now):
    """Drop expired entries, then the oldest until the cache fits the cap."""
    for key, value in list(cache.items()):
        if now >= value[0]:
            del cache[key]
    if len(cache) <= _CACHE_MAX_ENTRIES:
        return
    # Oldest-expiring first: those are the entries closest to being useless.
    for key, _value in sorted(cache.items(), key=lambda kv: kv[1][0])[
        : len(cache) - _CACHE_MAX_ENTRIES
    ]:
        del cache[key]


def _preflight_study_ts_by_class(
    study_uid,
    clarus_url,
    http_timeout=2.0,
    tls=None,
    budget_s=4.0,
    expose_rejected=False,
    qa_url="",
):
    """Return dict[sop_class -> set[stored transfer syntax UID]] for a
    C-MOVE study, or None when pre-flight cannot be completed.

    Used to shape the outbound association's per-TS contexts so the stored
    transfer syntax of every instance is PROPOSED and its acceptance is
    explicit at negotiation time (spec R1/R2). Failure is NEVER fatal: any
    error/timeout/404/over-budget returns None and the caller degrades to
    the fixed [EVRLE, IVRLE, 4.80, 4.70] proposal (today's behavior).

    1. QIDO series lookup /studies/{study}/series -> SeriesInstanceUIDs.
    2. Per series, WADO metadata /studies/{study}/series/{series}/metadata
       (Accept application/dicom+json) -> one JSON array; each element
       contributes (SOPClassUID from 00080016, TS from 00020010).

    The metadata resource is answered with the mandatory media type
    multipart/related; type="application/dicom+json" (PS3.18 10.4.4) - the
    single part is parsed; a plain application/dicom+json body is handled as
    before. Successful non-empty results are cached for _PREFLIGHT_TTL_S
    seconds.
    """
    _now = time.monotonic()
    # The cache key carries the expose mode: the two AEs see DIFFERENT instance
    # sets for the same study, so a shared entry would let a normal-AE job
    # propose contexts for instances it must not retrieve.
    _cache_key = (study_uid, bool(expose_rejected))
    _hit = _preflight_cache.get(_cache_key)
    if _hit is not None and _now < _hit[0]:
        logging.info(
            "[C-MOVE] pre-flight %s: cache hit (TTL %.0fs), %d class(es)",
            str(study_uid)[:50],
            _PREFLIGHT_TTL_S,
            len(_hit[1]),
        )
        return {k: set(v) for k, v in _hit[1].items()}
    _preflight_cache.pop(_cache_key, None)
    base = service_base_url(clarus_url, qa_url, expose_rejected)
    start = time.monotonic()

    def _over_budget():
        return (time.monotonic() - start) >= budget_s

    def _normalize_rows(value):
        """Coerce a parsed metadata JSON value into a list of instance dicts.

        Some deployments answer WADO metadata as a single JSON array of
        instance objects; others (PS3.18 Annex F allows either a single
        array or one object per multipart part) answer as one JSON
        object per instance per multipart part. Accept a list (extend) or a
        bare dict (one instance). Anything else yields nothing.
        """
        if isinstance(value, list):
            return [v for v in value if isinstance(v, dict)]
        if isinstance(value, dict):
            return [value]
        return []

    def _parse_metadata_body(content_type, raw):
        """Parse the metadata response body into a list of JSON dicts.

        PS3.18 8.9.3 + 10.4.4: metadata is conveyed as the mandatory
        multipart/related media type. Two conformant shapes occur in the
        field:
          * one part whose payload is a JSON array of instance objects, or
          * one JSON object per instance, one instance per multipart part.
        Both are parsed here: when content_type starts with multipart/related
        the raw body is split on the boundary and EVERY part's payload is
        parsed (each as an array -> extended, or a bare object -> appended);
        otherwise the whole body is parsed directly as JSON. Returns [] on
        any parse failure.
        """
        ct = content_type or ""
        if not ct.lower().startswith("multipart/related"):
            try:
                rows = raw.json() if hasattr(raw, "json") else _json_loads(raw)
                return _normalize_rows(rows)
            except Exception:
                return []
        # Extract the boundary parameter, e.g.
        #   boundary="18d3aa...".
        boundary = ""
        for piece in ct.split(";"):
            piece = piece.strip()
            if piece.lower().startswith("boundary="):
                boundary = piece[len("boundary=") :].strip().strip('"')
                break
        if not boundary:
            return []
        try:
            body_bytes = raw.content if hasattr(raw, "content") else raw
            parts = split_multipart_parts(body_bytes, boundary)
            if not parts:
                return []
            out = []
            for _headers, body in parts:
                try:
                    out.extend(_normalize_rows(_json_loads(body)))
                except Exception:
                    # A malformed part must not sink a good response: skip it.
                    continue
            return out
        except Exception:
            return []

    # 1. enumerate series (QIDO-RS, same shape the series C-FIND step-b uses).
    # `_qido_request` returns the parsed JSON list (or []); a 4xx raises an
    # httpx.HTTPStatusError which is a subclass of httpx.HTTPError.
    try:
        series_url = f"{base}/studies/{_quote(study_uid, safe='')}/series"
        series_rows = _qido_request(series_url, {}, http_timeout, limit=None, tls=tls)
        if not isinstance(series_rows, list):
            raise ValueError("series QIDO did not return a JSON list")
    except (_httpx.HTTPError, ValueError, KeyError, TypeError):
        logging.debug(
            "[C-MOVE] pre-flight: series QIDO for %s failed - no stored-TS "
            "contexts proposed",
            str(study_uid)[:50],
        )
        return None
    if _over_budget():
        return None

    series_uids = []
    for row in series_rows:
        if not isinstance(row, dict):
            continue
        suid = _json_first_value(row, "0020000E")
        if suid:
            series_uids.append(suid)
    if not series_uids:
        # Empty study (no series) - nothing to pre-liquidate; caller uses the
        # default proposal and the study stream will simply return 0 parts.
        return {}

    result = {}
    for suid in series_uids:
        if _over_budget():
            return None  # give up cleanly, degrade to default proposal
        meta_url = (
            f"{base}/studies/{_quote(study_uid, safe='')}"
            f"/series/{_quote(suid, safe='')}/metadata"
        )
        try:
            mresp = requests.get(
                meta_url,
                headers={"Accept": "application/dicom+json"},
                timeout=http_timeout,
                cert=tls,
            )
            mresp.raise_for_status()
            _mh = getattr(mresp, "headers", {}) or {}
            rows = _parse_metadata_body(
                _mh.get("Content-Type", ""),
                mresp,
            )
            if not rows:
                # add ONE debug line with the reason so a failing field
                # pre-flight is not silent.
                logging.debug(
                    "[C-MOVE] pre-flight: metadata for series %s: %d bytes, "
                    "content-type=%s - no JSON rows parsed",
                    str(suid)[:50],
                    len(mresp.content) if hasattr(mresp, "content") else -1,
                    _mh.get("Content-Type", "?"),
                )
                continue
        except Exception as _pref_exc:
            # Any series metadata failure -> abandon the whole pre-flight
            # (partial data could shape a wrong proposal).
            logging.debug(
                "[C-MOVE] pre-flight: metadata for series %s failed (%s) - "
                "no stored-TS contexts proposed",
                str(suid)[:50],
                _pref_exc,
            )
            return None
        for inst in rows:
            if not isinstance(inst, dict):
                continue
            sop_class = _json_first_value(inst, "00080016")
            ts_uid = _json_first_value(inst, "00020010")
            if sop_class and ts_uid:
                result.setdefault(sop_class, set()).add(ts_uid)
    # drop empty classes
    result = {k: v for k, v in result.items() if v}
    n_classes = len(result)
    n_ts = sum(len(v) for v in result.values())
    logging.info(
        "[C-MOVE] pre-flight %s: %d class(es), %d stored TS total "
        "(PS3.18 10.4.4 metadata)",
        str(study_uid)[:50],
        n_classes,
        n_ts,
    )
    if result:
        _preflight_cache[(study_uid, bool(expose_rejected))] = (
            time.monotonic() + _PREFLIGHT_TTL_S,
            {k: set(v) for k, v in result.items()},
        )
        _cache_evict(_preflight_cache, time.monotonic())
    return result


# ---------------------------------------------------------------------------
# _study_sop_uids - ordered, de-duplicated SOP instance UID list of a study
# (PS3.18 10.4.4 metadata): the authoritative UNDELIVERED tail for
# the (0008,0058) Failed SOP Instance UID List on a GIVE_UP / partial final is
# computed as (this list) minus (the UIDs actually delivered across all
# resume passes). Kept separate from `_preflight_study_ts_by_class` so the
# negotiation map and its tests are untouched. Returns the ordered list of
# SOP UIDs, or None when the study metadata cannot be enumerated (the worker
# then degrades to the per-attempt failed list).
# ---------------------------------------------------------------------------
_study_sop_uids_cache = {}  # study_uid -> (expires_monotonic, list[uid])


def _study_sop_uids(
    study_uid,
    clarus_url,
    http_timeout=2.0,
    tls=None,
    budget_s=4.0,
    expose_rejected=False,
    qa_url="",
):
    """Return ordered, de-duplicated SOP instance UID list of a study from
    the standard per-instance metadata resources (PS3.18 10.4.4), or None if
    the study cannot be enumerated (best effort; the caller degrades)."""

    def _normalize(value):
        if isinstance(value, list):
            return [v for v in value if isinstance(v, dict)]
        if isinstance(value, dict):
            return [value]
        return []

    def _parse_body(ct, raw):
        """Metadata body -> list of instance dicts (array part or object-per-
        part; mirrored from `_preflight_study_ts_by_class`)."""
        ctype = ct or ""
        try:
            if not ctype.lower().startswith("multipart/related"):
                rows = raw.json() if hasattr(raw, "json") else _json_loads(raw)
                return _normalize(rows)
            boundary = ""
            for piece in ctype.split(";"):
                piece = piece.strip()
                if piece.lower().startswith("boundary="):
                    boundary = piece[len("boundary=") :].strip().strip('"')
                    break
            if not boundary:
                return []
            body_bytes = raw.content if hasattr(raw, "content") else raw
            parts = split_multipart_parts(body_bytes, boundary)
            out = []
            for _h, body in parts:
                out.extend(_normalize(_json_loads(body)))
            return out
        except Exception:
            return []

    now = time.monotonic()
    hit = _study_sop_uids_cache.get((study_uid, bool(expose_rejected)))
    if hit is not None and now < hit[0]:
        return list(hit[1])
    _study_sop_uids_cache.pop((study_uid, bool(expose_rejected)), None)
    base = service_base_url(clarus_url, qa_url, expose_rejected)
    start = time.monotonic()

    def _over_budget():
        return (time.monotonic() - start) >= budget_s

    try:
        series_url = f"{base}/studies/{_quote(study_uid, safe='')}/series"
        series_rows = _qido_request(series_url, {}, http_timeout, limit=None, tls=tls)
    except Exception:
        return None
    series_uids = []
    if isinstance(series_rows, list):
        for row in series_rows:
            if isinstance(row, dict):
                s = _json_first_value(row, "0020000E")
                if s:
                    series_uids.append(s)
    if not series_uids:
        return []
    uids = []
    seen = set()
    for suid in series_uids:
        if _over_budget():
            return None
        meta_url = (
            f"{base}/studies/{_quote(study_uid, safe='')}"
            f"/series/{_quote(suid, safe='')}/metadata"
        )
        try:
            mresp = requests.get(
                meta_url,
                headers={"Accept": "application/dicom+json"},
                timeout=http_timeout,
                cert=tls,
            )
            mresp.raise_for_status()
            _mh = getattr(mresp, "headers", {}) or {}
            rows = _parse_body(_mh.get("Content-Type", ""), mresp)
        except Exception:
            return None
        for inst in rows:
            sop = _json_first_value(inst, "00080018")
            if sop and sop not in seen:
                seen.add(sop)
                uids.append(sop)
    if uids:
        _study_sop_uids_cache[(study_uid, bool(expose_rejected))] = (
            time.monotonic() + _PREFLIGHT_TTL_S,
            list(uids),
        )
        _cache_evict(_study_sop_uids_cache, time.monotonic())
    return uids


# ---------------------------------------------------------------------------
# (2608): self-delivery guard  -  is the target the bridge's own listener?
# ---------------------------------------------------------------------------
def _is_self_delivery_target(
    target_host, target_port, bridge_dimse_host, bridge_dimse_port
):
    """Return True if (target_host, target_port) is the bridge's own DIMSE listener.

    Loopback aliases (127.0.0.1, localhost, ::1) and bind-any addresses
    (0.0.0.0, ::) are treated as one endpoint: a peers.conf entry pointing
    at any alias with the bridge port would loop C-MOVE back into the
    bridge's own C-STORE SCP.
    """
    if bridge_dimse_port is None or target_port != bridge_dimse_port:
        return False
    if target_host is None:
        return False

    def _normalize(host):
        h = str(host).strip().lower()
        if h in ("0.0.0.0", "::", "[::]"):
            return "127.0.0.1"
        return h

    aliases = {"127.0.0.1", "localhost", "::1"}
    if bridge_dimse_host:
        aliases.add(_normalize(bridge_dimse_host))
    return _normalize(target_host) in aliases


def _failure_dataset(uids, error_comment=None):
    """Response-Identifier Data Set for a non-success C-MOVE-RSP final status.

    - (0008,0058) Failed SOP Instance UID List (UI, VM 1-n): the SOP instance
      UIDs that were attempted but not delivered - PS3.4 Table C.4-2 requires
      Cancel, Failure and Warning to contain it (PS3.4 C.4.2.1.4.2).
    - (0000,0902) Error Comment (LO): honest reason, carried on a Failure
      status when nothing conclusive was listed.

    Both elements use add_new (keyword-independent) so the command/data set
    round-trips regardless of pynetdicom property mapping.
    """
    ds = Dataset()
    if error_comment:
        try:
            ds.add_new((0x0000, 0x0902), "LO", str(error_comment)[:64])
        except Exception:
            pass
    if uids:
        try:
            ds.add_new((0x0008, 0x0058), "UI", [str(u) for u in uids])
        except Exception:
            pass
    return ds


# Bridge: C-MOVE handler  -  synchronous transfer with per-instance progress
def handle_c_move(
    event,
    move_jobs,
    peers_file,
    clarus_url,
    bridge_ae_title,
    http_timeout=30,
    sop_registry=None,
    storage_commit_after_move=True,
    sop_index=None,
    outbound_network_timeout=60,
    outbound_dimse_timeout=60,
    outbound_acse_timeout=15,
    bridge_dimse_host=None,
    bridge_dimse_port=None,
    move_poll_timeout=600,
    tls=None,
    expose_rejected=False,
    qa_url="",
):
    """Handle C-MOVE-RQ: fire-and-forget dispatch + held reactor for the final.

    DICOM PS3.4 sec.C.4.2.2: the SCP shall have prior knowledge of the
    Move Destination AE Title and Presentation Address.

    Combo model (fire-and-forget + DB-polling for the final response):
    - jobs are created in SQLite; _cmove_worker pool streams in background
      (worker threads associate and send C-STORE, independently of reactor)
    - yield (addr, port) -> pynetdicom primes store_assoc
    - reactor is DELIBERATELY held: the generator polls job progress from
      the DB (0.5 s interval, bounded by move_poll_timeout) until every job
      is terminal, and only then emits the pending-count int + final status
    - success -> yield 0 (zero sub-operations): pynetdicom sends the final
      0000 immediately and opens no store_assoc
    - failure / cancel -> yield 1, then the honest final status (0xB000 or
      0xC000 / 0xFE00) with its (0008,0058) Identifier

    Pending C-MOVE-RSPs are optional (PS3.4 C.1.4) and the bridge does not
    send them: the final response is delayed until the transfer actually
    finishes (so its timing and statuses stay honest), and the final
    sub-operation counters reflect the sub-operations the DIMSE framework
    performed synchronously  -  which in this fire-and-forget model is zero.

    The reactor is freed only after the final response  -  NOT after dispatch:
    this protects SCUs that treat the final C-MOVE-RSP as "all stores done".

    Actual transfer runs in _cmove_worker thread pool.
    Real progress tracked via Prometheus metrics + job status.

    Self-delivery guard (2608): if the resolved Move Destination is the
    bridge's own DIMSE listener (host/port match incl. loopback aliases),
    the request is rejected with 0xA801 before any job is created.

    Poll timeout (2608): the progress poll is bounded by move_poll_timeout;
    on expiry unfinished jobs are finished as failed and 0xB000 is returned.
    """
    calling_ae = event.assoc.requestor.ae_title
    model_uid = str(event.context.abstract_syntax) if event.context else "?"

    # DEBUG: log full C-MOVE-RQ identifier to diagnose Weasis multi-study import
    ds = event.identifier
    patient_id = (
        str(getattr(ds, "PatientID", "")).strip() if hasattr(ds, "PatientID") else ""
    )
    study_uid = (
        str(getattr(ds, "StudyInstanceUID", "")).strip()
        if hasattr(ds, "StudyInstanceUID")
        else ""
    )
    logging.info(
        "[DIMSE] C-MOVE from %s model=%s PatientID=%s StudyUID=%s",
        calling_ae,
        model_uid,
        mask_patient_id(patient_id) if patient_id else "(none)",
        study_uid[:50] if study_uid else "(none)",
    )

    try:
        study_uids, move_dest_ae = _parse_c_move_rq(event)
    except ValueError as e:
        logging.warning("[C-MOVE] Parse error: %s", e)
        yield 0xC000, None
        return

    if peers_file:
        move_jobs.reload_peers_from_file(peers_file)

    calling_ip = event.assoc.requestor.address
    target = move_jobs.lookup_scp(move_dest_ae, client_ip=calling_ip)
    if target is None:
        logging.warning(
            "[C-MOVE] Unknown Move Destination: %s. Add '%s  <IP>:<port>' to peers.conf.",
            move_dest_ae,
            move_dest_ae,
        )
        yield 0xA801, None
        return

    target_host, target_port = target

    # P2 (2608): CIDR peers entries carry an informational host  -  the real
    # destination is the calling client itself (subnet match, PS3.4 sec.C.4.2.2).
    # Without substitution _associate_for_move would get the CIDR string and
    # silently fail to associate.
    if "/" in target_host:
        logging.info(
            "[C-MOVE] Move Destination %s: CIDR host %s is informational - "
            "routing via calling client IP %s.",
            move_dest_ae,
            target_host,
            calling_ip,
        )
        target_host = calling_ip

    # (2608): self-delivery guard  -  never C-STORE back into the bridge's
    # own DIMSE listener. Field incident: misconfigured peers.conf pointed
    # the Move Destination at the bridge itself; the bridge answered its own
    # C-STORE with success and the viewer received nothing.
    if _is_self_delivery_target(
        target_host, target_port, bridge_dimse_host, bridge_dimse_port
    ):
        logging.warning(
            "[C-MOVE] Move Destination %s resolves to the bridge itself "
            "(%s:%d) - rejecting with 0xA801 (self-delivery loop).",
            move_dest_ae,
            target_host,
            target_port,
        )
        yield 0xA801, None
        return

    if target_host != calling_ip:
        logging.warning(
            "[C-MOVE] IP MISMATCH: %s registered at %s but C-MOVE from %s.",
            move_dest_ae,
            target_host,
            calling_ip,
        )
        yield 0xA801, None
        return

    # create a job for EACH StudyInstanceUID in the request.
    # Weasis sends multiple StudyUIDs in one C-MOVE-RQ  -  process them all.
    job_ids = []
    for study_uid in study_uids:
        job_id = move_jobs.create_job(
            study_uid,
            move_dest_ae,
            target_host,
            target_port,
            expose_rejected=expose_rejected,
        )
        job_ids.append(job_id)
        logging.info(
            "[C-MOVE] Job #%d created for Study %s -> %s",
            job_id,
            study_uid,
            move_dest_ae,
        )

    logging.info(
        "[C-MOVE] Move Destination: %s -> %s:%d, Studies: %d (%s)",
        move_dest_ae,
        target_host,
        target_port,
        len(study_uids),
        ", ".join(study_uids),
    )

    # Set cancel callback on ALL jobs.
    #
    # `hasattr(event, "is_cancelled")` is NOT a harmless probe: on a
    # pynetdicom service event the name is a property that READS the
    # association's cancel state, and that read consumes a C-CANCEL already
    # received. Probing with it therefore discarded a cancel that arrived
    # before this point, and every later poll saw False. Presence is
    # decided on the TYPE, which never invokes the getter.
    #
    # The state is also LATCHED: one property read per poll writes True into a
    # holder both the poller and the worker read, so whichever of them asks
    # first cannot swallow the cancel out from under the other.
    class _CancelState:
        __slots__ = ("cancelled",)

        def __init__(self):
            self.cancelled = False

    _cancel_state = _CancelState()
    _has_cancel = getattr(type(event), "is_cancelled", None) is not None or (
        "is_cancelled" in getattr(event, "__dict__", {})
    )

    def _is_cancelled():
        # Read at most once per poll and remember a True: the property is
        # consumed by reading it, so a second reader in the same window would
        # otherwise see the state already drained.
        if _cancel_state.cancelled:
            return True
        if not _has_cancel:
            return False
        state = event.is_cancelled
        if state() if callable(state) else state:
            _cancel_state.cancelled = True
            return True
        return False

    for job_id in job_ids:
        move_jobs.set_job_cancel_callback(job_id, _is_cancelled)

    def _clear_callbacks():
        # The callback closes over `event` (and through it the association).
        # Leaving it in the registry keeps the whole request object alive for
        # the life of the process, so every cancel path drops it.
        for job_id in job_ids:
            move_jobs.clear_job_callback(job_id)

    # yield 1: (addr, port)  -  pynetdicom primes store_assoc
    yield (target_host, target_port)

    def _agg_failed_uids():
        """(0008,0058) material across all finished jobs (PS3.4 C.4.2.1.4.2)."""
        out = []
        for job_id in job_ids:
            getter = getattr(move_jobs, "get_job_failed_sops", None)
            if getter is None:
                continue
            try:
                for u in getter(job_id):
                    if u and u not in out:
                        out.append(u)
            except Exception:
                continue
        return out

    def _finish_jobs_cancelled():
        for job_id in job_ids:
            move_jobs.finish_job(
                job_id, success=False, error_message="Cancelled by SCU"
            )
            move_jobs_counter.labels(status="failed").inc()
        _clear_callbacks()

    # yield 2+: hold the DIMSE dialog (0.5 s DB poll, bounded by
    # move_poll_timeout) until every job is terminal  -  the final response is
    # not emitted until the transfer actually finishes. Pending C-MOVE-RSPs
    # are optional (PS3.4 C.1.4) and the bridge does not send them, so no
    # per-instance progress yield occurs; the pending-count int is emitted
    # only once the poll concludes.
    logging.info(
        "[C-MOVE] %d job(s) dispatched to worker (polling DB for progress)",
        len(job_ids),
    )

    # P3 (2608): bound the poll with an overall timeout. If a worker dies
    # between picking a job and finishing it, the viewer must get 0xB000
    # instead of a hung association.
    poll_started = time.monotonic()

    while True:
        if _is_cancelled():
            _finish_jobs_cancelled()
            # Non-empty sub-operation count so pynetdicom keeps the dialog
            # open for the Cancel status yield below.
            yield 1
            # PS3.4 Table C.4-2 FE00 Cancel; C.4.2.1.4.2: the Identifier
            # carries (0008,0058) Failed SOP Instance UID List (may be empty).
            yield 0xFE00, _failure_dataset(_agg_failed_uids())
            return

        # Aggregate terminal state across all jobs
        total_completed = 0
        all_finished = True
        any_failed = False
        for job_id in job_ids:
            completed, _total, status = move_jobs.get_job_progress(job_id)
            total_completed += completed
            if status not in ("success", "failed"):
                all_finished = False
            if status == "failed":
                any_failed = True

        if all_finished:
            if any_failed:
                # A terminal status carries NO sub-operations: the count is
                # what tells pynetdicom whether to open the move-destination
                # association before it reads the failure, and a non-zero
                # count there makes it associate once more - a failure of that
                # association then replaces the honest 0xB000/0xC000 with
                # 0xA801, and the (0008,0058) list never reaches the viewer.
                # `0` is the truthful count: nothing is still in flight.
                failed_uids = _agg_failed_uids()
                # Two yields, not one tuple: the reactor protocol is a bare
                # sub-operation count followed by (status, identifier), and a
                # bare 0 is the truthful count here.
                yield 0
                if total_completed == 0:
                    # Zero sub-operations completed: B000 ("Sub-operations
                    # Complete") would be a lie - say Failure (PS3.4 Table
                    # C.4-2, "Failed: Unable to process") with an honest
                    # Error Comment (PS3.4 C.4.2.1.4.2 Identifier).
                    comment = "Unable to process: no SOP instances delivered"
                    yield 0xC000, _failure_dataset(
                        failed_uids,
                        error_comment=comment,
                    )
                else:
                    # Partial: some sub-operations failed -> Warning (Table
                    # C.4-2) + (0008,0058) Failed SOP Instance UID List.
                    yield 0xB000, _failure_dataset(failed_uids)
            else:
                # All jobs succeeded: 0 sub-operations -> pynetdicom sends
                # the final 0000 immediately and opens NO store_assoc
                # (service_class.py:2136-2143 returns before the associate).
                yield 0
            _clear_callbacks()
            return

        # P3 (2608): poll timeout  -  finish unfinished jobs as failed, then
        # report the honest failure/warning. The reactor releases the
        # association after the final yields.
        if (
            move_poll_timeout is not None
            and time.monotonic() - poll_started >= move_poll_timeout
        ):
            timed_out_jobs = []
            for job_id in job_ids:
                _, _, job_status = move_jobs.get_job_progress(job_id)
                if job_status not in ("success", "failed"):
                    move_jobs.finish_job(
                        job_id, success=False, error_message="Move poll timeout"
                    )
                    move_jobs_counter.labels(status="failed").inc()
                    timed_out_jobs.append(job_id)
            # The worker streams instances and only finishes the job when it
            # runs out of them, so marking the row failed is not enough: it
            # would keep sending and then rewrite the row to success. The
            # cancel state is what the worker already polls per instance.
            _cancel_state.cancelled = True
            logging.warning(
                "[C-MOVE] Move poll timeout (%ds) expired: %d job(s) still "
                "unfinished (%s) - finished as failed, final status %s.",
                move_poll_timeout,
                len(timed_out_jobs),
                (
                    ", ".join(str(j) for j in timed_out_jobs)
                    if timed_out_jobs
                    else "(none)"
                ),
                "0xC000" if total_completed == 0 else "0xB000",
            )
            yield 0
            failed_uids = _agg_failed_uids()
            if total_completed == 0:
                # Nothing delivered before the poll timeout -> Failure, not a
                # "sub-operations complete" Warning (PS3.4 Table C.4-2).
                yield 0xC000, _failure_dataset(
                    failed_uids,
                    error_comment="Unable to process: no SOP instances delivered (move poll timeout)",
                )
            else:
                yield 0xB000, _failure_dataset(failed_uids)
            _clear_callbacks()
            return

        time.sleep(0.5)


# ---------------------------------------------------------------------------
# Bridge: Storage Commitment push after C-MOVE (CMP-008)
# ---------------------------------------------------------------------------
def _send_storage_commitment(
    move_dest_ae, target_host, target_port, sop_list, bridge_ae_title
):
    """Send N-ACTION Storage Commitment Request to move destination AE.

    Args:
        move_dest_ae: AE Title of the move destination (SCU)
        target_host: IP/hostname of the move destination
        target_port: port of the move destination
        sop_list: list of (sop_class_uid, sop_instance_uid) tuples
        bridge_ae_title: Bridge's own AE Title

    Returns:
        True if commitment request was sent successfully.
    """
    logging.info(
        "[C-MOVE-COMMIT] Sending Storage Commitment to %s@%s:%d for %d SOPs",
        move_dest_ae,
        target_host,
        target_port,
        len(sop_list),
    )

    ae = AE(ae_title=bridge_ae_title)
    # Storage Commitment is optional (PS3.4 sec.C.4.2.5). Fast-fail ACSE:
    # if viewer doesn't respond in 3s, it doesn't support SC. Don't
    # waste 10s on a feature that most viewers don't implement.
    ae.network_timeout = 5
    ae.acse_timeout = 3
    ae.dimse_timeout = 30
    ae.add_requested_context(StorageCommitmentPushModel)
    ae.add_requested_context(
        Verification
    )  # Fallback: allows association even if no SC support

    # suppress pynetdicom's "No accepted presentation contexts" ERROR  -
    # it's normal for SCPs that don't support Storage Commitment.
    global _suppress_pynetdicom
    _suppress_pynetdicom = True
    try:
        assoc = ae.associate(target_host, target_port, ae_title=move_dest_ae)
    finally:
        _suppress_pynetdicom = False

    if not assoc.is_established:
        logging.info(
            "[C-MOVE-COMMIT] SCP %s:%d unavailable - skipping Storage Commitment",
            target_host,
            target_port,
        )
        return False

    sc_accepted = any(
        str(ctx.abstract_syntax) == StorageCommitmentPushModel
        for ctx in assoc.accepted_contexts
    )
    if not sc_accepted:
        logging.info(
            "[C-MOVE-COMMIT] SCP %s does not support Storage Commitment - skipping",
            move_dest_ae,
        )
        _release_or_abort(assoc, timeout=2.0)
        return False

    try:
        # Build N-ACTION request dataset
        ds = Dataset()
        ds.TransactionUID = generate_uid()

        ref_seq = []
        for sop_class, sop_uid in sop_list:
            item = Dataset()
            item.ReferencedSOPClassUID = sop_class
            item.ReferencedSOPInstanceUID = sop_uid
            ref_seq.append(item)
        ds.ReferencedSOPSequence = ref_seq

        # Send N-ACTION with action type 1 (Storage Commitment Request)
        # pynetdicom 3.x signature: send_n_action(dataset, action_type, class_uid,
        #   meta_uid=None, instance_uid=None)
        # action_type=1 (PS3.4 sec.J.3.1), class_uid=StorageCommitmentPushModel.
        status, response = assoc.send_n_action(
            ds,
            1,  # action_type: Storage Commitment Request (PS3.4 sec.J.3.1)
            StorageCommitmentPushModel,
            instance_uid=ds.TransactionUID,
        )

        if status:
            status_code = status.Status if hasattr(status, "Status") else status
            if status_code == 0x0000:
                logging.info(
                    "[C-MOVE-COMMIT] %s: Storage Commitment %s.",
                    move_dest_ae,
                    _dimse_status_name(status_code),
                )
            else:
                # Any SC failure is non-fatal: the move already succeeded and
                # Storage Commitment is optional (PS3.4 C.4.2.5).
                logging.info(
                    "[C-MOVE-COMMIT] %s: Storage Commitment %s. "
                    "Non-fatal (SC optional, PS3.4 C.4.2.5).",
                    move_dest_ae,
                    _dimse_status_name(status_code),
                )
            return True
        else:
            logging.warning(
                "[C-MOVE-COMMIT] N-ACTION no status returned for %s",
                move_dest_ae,
            )
            return False
    except Exception as e:
        logging.warning(
            "[C-MOVE-COMMIT] N-ACTION failed for %s: %s",
            move_dest_ae,
            e,
        )
        return False
    finally:
        _release_or_abort(assoc, timeout=2.0)


# ---------------------------------------------------------------------------
# Lazy-load Clarus capabilities (GET /dicomweb) at first C-MOVE.
# Don't query at startup  -  server may not be running yet.
# Query on first use, cache result, fall back to defaults if unreachable.
# ---------------------------------------------------------------------------
_TS_NAMES = {
    "1.2.840.10008.1.2": "Implicit VR Little Endian",
    "1.2.840.10008.1.2.1": "Explicit VR Little Endian",
    "1.2.840.10008.1.2.1.99": "Deflated Explicit VR Little Endian",
    "1.2.840.10008.1.2.2": "Explicit VR Big Endian",
    "1.2.840.10008.1.2.4.50": "JPEG Baseline",
    "1.2.840.10008.1.2.4.51": "JPEG Extended",
    "1.2.840.10008.1.2.4.57": "JPEG Lossless SV1",
    "1.2.840.10008.1.2.4.70": "JPEG Lossless SV1 (first-order)",
    "1.2.840.10008.1.2.4.80": "JPEG-LS Lossless",
    "1.2.840.10008.1.2.4.81": "JPEG-LS Near-Lossless",
    "1.2.840.10008.1.2.4.90": "JPEG 2000 Lossless",
    "1.2.840.10008.1.2.4.91": "JPEG 2000",
    "1.2.840.10008.1.2.4.92": "JPEG 2000 Part 2",
    "1.2.840.10008.1.2.5": "RLE Lossless",
}


def _ts_name(uid):
    """Human-readable transfer syntax name for CAPS logging.

    Was a NameError before: the helper was called but never defined, so the
    whole caps fetch died and the bridge always fell back to passthrough.
    """
    return _TS_NAMES.get(uid, uid)


def _get_clarus_transfer_syntaxes(clarus_url, timeout=5.0, tls=None):
    """Query Clarus /dicomweb for transfer syntax capabilities.

    Result is cached in the function's closure after first success.

    IMPORTANT: CAPS announces only encode-capable
    transcode-target codecs. An empty CAPS means "no transcoding available,
    request without ?transferSyntax=, receive stored bytes as-is."
    Bridge MUST NOT invent capabilities the server doesn't claim.

    NB: "MUST NOT invent" applies to *compressed*
    transcode targets - we never refetch to a compressed TS the server did
    not announce. It does NOT exclude EVRLE: a transcode-capable server is
    expected to be able to return the standard default EVRLE representation
    (PS3.18 8.7.3.1), so `_pick_refetch_ts` adds EVRLE on its own when
    `server_ts_list` is non-empty. On a no-transcode server (this list empty)
    nothing - including EVRLE - is invented or requested.

    Args:
        clarus_url: Clarus DICOMweb base URL (single knob, 2908).
        timeout: HTTP request timeout in seconds.
        tls: optional (cert_path, key_path) tuple for mTLS.

    Returns:
        List of transfer syntax UID strings (empty if server can't transcode).
    """
    cache_key = clarus_url
    if not hasattr(_get_clarus_transfer_syntaxes, "cache"):
        _get_clarus_transfer_syntaxes.cache = {}
    cache = _get_clarus_transfer_syntaxes.cache
    cached_entry = cache.get(cache_key)
    if cached_entry:
        ts_list, cached_at = cached_entry
        if time.time() - cached_at < 60:
            return ts_list

    ts_list = []
    base = dicomweb_base(clarus_url)
    try:
        resp = requests.get(
            base,
            timeout=timeout,
            cert=tls,
        )
        resp.raise_for_status()
        caps = resp.json()
        tc_list = caps.get("transferCapabilities")
        if tc_list:
            for tc in tc_list:
                ts_list.extend(tc.get("transferSyntaxUid", []))
            ts_list = sorted(set(ts_list))
            logging.info(
                "[CAPS] %s -> %d transfer syntax(es):",
                base,
                len(ts_list),
            )
            for ts in ts_list:
                logging.info("[CAPS]   %s", _ts_name(ts))
        elif tc_list is not None:
            # Empty array []  -  server honestly reports: no transcoding.
            logging.info(
                "[CAPS] Server %s: empty transferCapabilities - "
                "passthrough mode (no transcoding, bridge will not request ?transferSyntax=)",
                clarus_url,
            )
        else:
            # null/absent  -  treat as empty (no transcode).
            logging.info(
                "[CAPS] Server %s: no transferCapabilities field - " "passthrough mode",
                clarus_url,
            )
    except Exception as e:
        logging.warning(
            "[CAPS] Clarus %s unreachable: %s - assuming passthrough mode",
            clarus_url,
            e,
        )
        ts_list = []

    _get_clarus_transfer_syntaxes.cache[cache_key] = (ts_list, time.time())
    return ts_list


# ---------------------------------------------------------------------------
# _associate_for_move  -  establish C-STORE association with target SCP
# ---------------------------------------------------------------------------
def _is_image_sop(sop_uid, sop_registry=None):
    """Check sop_registry for the non_image flag, fall back to the prefix
    heuristic. Non-image SOPs (SR/PR/RTSTRUCT) are always sent as-is."""
    if sop_registry and sop_uid in sop_registry:
        entry = sop_registry[sop_uid]
        if isinstance(entry, dict) and entry.get("non_image"):
            return False
    # Fallback prefix heuristic
    _PFX = (
        "1.2.840.10008.5.1.4.1.1.88.",
        "1.2.840.10008.5.1.4.1.1.11.",
        "1.2.840.10008.5.1.4.1.1.104.",
        "1.2.840.10008.5.1.4.1.1.66.",
    )
    _EXACT = {
        "1.2.840.10008.5.1.4.1.1.481.3",
        "1.2.840.10008.5.1.4.1.1.481.4",
        "1.2.840.10008.5.1.4.1.1.481.5",
        "1.2.840.10008.5.1.4.1.1.481.6",
        "1.2.840.10008.5.1.4.1.1.481.7",
        "1.2.840.10008.5.1.4.1.1.481.8",
        "1.2.840.10008.5.1.4.1.1.481.9",
    }
    if sop_uid in _EXACT:
        return False
    return not sop_uid.startswith(_PFX)


def _proposed_ts_per_class(sop_registry):
    """Yield the storage SOP classes (as UIDs) this bridge will propose on a
    C-MOVE outbound association.

    PS3.8 7.1.1.13: a presentation context = one abstract syntax + a list of
    transfer syntaxes. Proposing the SAME abstract syntax several times, once
    per transfer syntax (the multi-context shape), is conformance-legal and is
    exactly the arrangement PS3.7 C.2.2.1.1 wants: the C-STORE dataset is
    encoded in the negotiated transfer syntax of the presentation context that
    is used, so per-TS contexts let the bridge pick the context whose transfer
    syntax equals the instance's stored transfer syntax.
    """
    if sop_registry is not None:
        return list(_get_sop_registry_uids(sop_registry))
    return list(_BUILTIN_SOP_CLASSES())


# Context-ID budget for one A-ASSOCIATE (PS3.8 7.1.1.13: Presentation Context
# IDs are the odd bytes 1..255, so at most 128 contexts). 1 is reserved for
# the Storage Commitment context proposed on the main association.
_MAX_CONTEXTS = 128


def _trim_optional_ts(num_classes, has_sc, optional_tokens):
    """Return the subset of optional tokens that fit the 128-context budget.

    optional_tokens: list of (sop_class_uid, ts_uid, kind) in canonical
    ascending order, where kind is 'stored' (a class's stored TS from
    pre-flight) or 'fixed' (the 4.80/4.70 extras of the pre-flight-less path).
    Each class always keeps its 2 baselines (EVRLE, IVRLE), which are never
    dropped (they are the refetch targets).

    The class is part of the token because the budget is shared: dropping a
    token identified only by its transfer syntax would remove that syntax from
    EVERY class, so one overfull class would cost another class a syntax it
    actually needed.

    Real studies propose far fewer than 128 contexts; this runs only as a
    defensive trim. Drop order (spec 12 / R2): stored extras first in
    reverse canonical priority (lowest-priority stored TS first), and the
    4.80/4.70 'fixed' extras only after every stored extra is gone.
    """
    fixed = 1 if has_sc else 0
    budget = _MAX_CONTEXTS - fixed - (2 * num_classes)
    if budget >= len(optional_tokens):
        return optional_tokens
    # Victim order: stored extras dropped from the LOWEST canonical priority
    # (largest canonical rank) upward first; fixed extras only after all
    # stored extras are gone. `stored` group sorts before `fixed` so the
    # fixed 4.80/4.70 extras survive longer.
    _group = {"stored": 0, "fixed": 1}

    def _victim_key(item):
        _sop_uid, ts_uid, kind = item
        # within stored: drop higher canonical index first (lower priority)
        rank = canonical_ts_sort_key(ts_uid)
        return (_group.get(kind, 1), -rank)

    victims = sorted(optional_tokens, key=_victim_key)
    drop_n = len(optional_tokens) - budget
    dropped = {tok for tok in victims[: max(0, drop_n)]}
    return [tok for tok in optional_tokens if tok not in dropped]


# ---------------------------------------------------------------------------
# Per-peer job gate
# ---------------------------------------------------------------------------
# Two jobs toward the SAME peer are serialized (busy exclusion): the second
# waits only while the first is actively running and then starts
# immediately. There is no post-job cooldown; the gate opens as soon
# as the peer is free.
# No client-specific logic involved - the gate applies equally to any peer.
_ASSOC_GATE_LOCK = threading.Lock()
_ASSOC_BUSY = set()  # peers with a job currently between gate-enter/exit
# Peer -> monotonic time of the LAST "busy - waiting" DEBUG line (throttled
# to at most one per _ASSOC_GATE_LOG_INTERVAL seconds, C3).
_ASSOC_GATE_LOG_LAST = {}
_ASSOC_GATE_LOG_INTERVAL = 10.0


def _job_gate_enter(host, port, ae_title):
    """Serialise concurrent jobs toward the same peer (busy exclusion).

    Waits until the peer has no actively running job, then marks it busy and
    returns; there is no post-job cooldown (immediate start once free).

    Logging: the per-tick "busy - waiting" DEBUG line is throttled to
    no more than one per _ASSOC_GATE_LOG_INTERVAL (default 10 s) seconds per
    peer, and exactly ONE line is emitted on entry carrying the total time
    the caller waited for the peer.
    """
    key = (host, port, ae_title)
    started = time.monotonic()
    waited_any = False
    while True:
        with _ASSOC_GATE_LOCK:
            if key not in _ASSOC_BUSY:
                _ASSOC_BUSY.add(key)
                if waited_any:
                    logging.info(
                        "[ASSOC-GATE] %s@%s:%s waited %.1fs for the peer",
                        ae_title,
                        host,
                        port,
                        time.monotonic() - started,
                    )
                return
            waited_any = True
        # Sleep OUTSIDE the lock so other workers can gate.
        now = time.monotonic()
        with _ASSOC_GATE_LOCK:
            last = _ASSOC_GATE_LOG_LAST.get(key, 0.0)
            if now - last >= _ASSOC_GATE_LOG_INTERVAL:
                _ASSOC_GATE_LOG_LAST[key] = now
                logging.debug(
                    "[ASSOC-GATE] %s@%s:%s busy - waiting for it to free",
                    ae_title,
                    host,
                    port,
                )
        time.sleep(0.05)


def _job_gate_exit(host, port, ae_title):
    """Release the peer so the next queued job to it can start."""
    key = (host, port, ae_title)
    with _ASSOC_GATE_LOCK:
        _ASSOC_BUSY.discard(key)
        # Drop the throttle timestamp so the next wait starts clean.
        _ASSOC_GATE_LOG_LAST.pop(key, None)


def _associate_for_move(
    move_dest_ae,
    bridge_ae_title,
    sop_registry,
    target_host,
    target_port,
    network_timeout=60,
    dimse_timeout=60,
    acse_timeout=15,
    outbound_assoc_attempts=3,
    study_ts_by_class=None,
):
    """Establish C-STORE association with target SCP (multi-context).

    Standard-shaped negotiation (PS3.8 7.1.1.13 +
    9.3.2.2, PS3.7 C.2.2.1.1): each SOP class is proposed as ONE presentation
    context per transfer syntax, in canonical order (uncompressed family
    first, then compressed). This lets the acceptor accept exactly the
    transfer syntaxes it supports and the SCU pick the context whose TS
    matches each instance's stored TS.

    Proposal per class:
    - WITH pre-flight (`study_ts_by_class` set): [EVRLE, IVRLE] + the class's
      stored TS set from the pre-flight (single-TS contexts, dedup).
    - WITHOUT pre-flight: [EVRLE, IVRLE, 4.80, 4.70] (today's IMAGE_TS as
      separate contexts - preserves the negotiated outcomes of 4.80-capable
      receivers; see test_bridge_ts_order.py).
    Non-image classes (SR/PR/RTSTRUCT) keep the uncompressed family plus any
    stored extras seen.

    Budget: total contexts + Storage Commitment <= 128 (PS3.8, PCID 1..255
    odd). Overflow trims stored extras in reverse canonical priority, then the
    4.80/4.70 extras; EVRLE/IVRLE are never trimmed (refetch targets).

    Args:
        move_dest_ae: AE Title of the move destination.
        bridge_ae_title: Bridge's own AE Title.
        sop_registry: SOP class registry dict.
        target_host: IP/hostname of the move destination.
        target_port: port of the move destination.
        network_timeout: socket-level (outbound, viewer may be slow).
        dimse_timeout: per C-STORE response (outbound).
        acse_timeout: A-ASSOCIATE negotiation.
        outbound_assoc_attempts: total A-ASSOCIATE attempts (main), immediate.
        study_ts_by_class: dict[sop_class -> set[ts]] from pre-flight, or None.

    Returns:
        (assoc, accepted_contexts_map) tuple.
        accepted_contexts_map: dict[sop_class_uid -> SET[ts]].

    Raises:
        RuntimeError if association fails after all attempts.
    """
    ae = AE(ae_title=bridge_ae_title)
    ae.network_timeout = network_timeout
    ae.acse_timeout = acse_timeout
    ae.dimse_timeout = dimse_timeout

    # Proposal is shaped from the canonical order: uncompressed family first,
    # then compressed in the taxonomy order (RLE, JPEG lossless, JPEG-LS,
    # JPEG lossy, JPEG 2000). Deterministic proposer order; a
    # conforming acceptor (PS3.8 9.3.2.2) accepts per-context by its own
    # support set, so proposer order is not a correctness input for standard
    # peers - only deterministic output for the bridge.
    # Field-observed: the registry-wide proposal
    # overflows the 128-context budget. With pre-flight knowledge the
    # proposal covers ONLY the classes the study actually contains
    # (PS3.18 10.4.4 metadata is per-instance, so its class map is the
    # study's real class set); the pre-flight-less legacy path keeps the
    # registry-wide proposal, subject to the whole-class budget drop
    # below.
    has_preflight = bool(study_ts_by_class)
    if has_preflight:
        class_uids = sorted(study_ts_by_class.keys())
    else:
        class_uids = _proposed_ts_per_class(sop_registry)
    # Per-class optional (non-baseline) TS, in canonical ascending order, each
    # tagged 'stored' (from pre-flight) or 'fixed' (4.80/4.70, no pre-flight).
    per_class_optional = {}
    for sop_uid in class_uids:
        is_image = _is_image_sop(sop_uid, sop_registry)
        if has_preflight:
            stored = set(study_ts_by_class.get(sop_uid, set()) or set())
            stored = {
                t
                for t in stored
                if t not in (ExplicitVRLittleEndian, ImplicitVRLittleEndian)
            }
            per_class_optional[sop_uid] = [
                (sop_uid, t, "stored")
                for t in sorted(stored, key=canonical_ts_sort_key)
            ]
        else:
            extra = [JPEGLS_LOSSLESS_TS, "1.2.840.10008.1.2.4.70"] if is_image else []
            per_class_optional[sop_uid] = [
                (sop_uid, t, "fixed")
                for t in extra
                if t not in (ExplicitVRLittleEndian, ImplicitVRLittleEndian)
            ]
    flat = [tok for cls in per_class_optional.values() for tok in cls]
    kept_tokens = set(_trim_optional_ts(len(class_uids), True, flat))
    proposals = {}
    for sop_uid in class_uids:
        ts_list = [ExplicitVRLittleEndian, ImplicitVRLittleEndian]
        for tok in per_class_optional[sop_uid]:
            if tok in kept_tokens and tok[1] not in ts_list:
                ts_list.append(tok[1])
        proposals[sop_uid] = ts_list

    # PS3.8 7.1.1.13: PCIDs are the odd bytes 1..255 -> at most 128
    # contexts, one reserved for Storage Commitment. Baselines alone can
    # overflow on the registry-wide (pre-flight-less) proposal: drop WHOLE
    # classes from the END of the list (reverse registry priority) until
    # the budget fits. Never truncate a class's own baseline pair -
    # EVRLE/IVRLE are the refetch targets (spec R2).
    budget = _MAX_CONTEXTS - 1  # Storage Commitment slot
    while sum(len(v) for v in proposals.values()) > budget:
        if not proposals:
            raise RuntimeError("presentation-context budget cannot fit")
        dropped_cls, dropped_ts = proposals.popitem()
        logging.info(
            "[C-MOVE] 128-context budget (PS3.8 7.1.1.13): dropped class "
            "%s (%d TS contexts) from the proposal",
            dropped_cls,
            len(dropped_ts),
        )

    # ----- add requested contexts -----
    contexts_req = 0
    for sop_uid in proposals:
        for ts_uid in proposals[sop_uid]:
            ae.add_requested_context(sop_uid, ts_uid)
            contexts_req += 1
    contexts_dropped = len(flat) - len(kept_tokens)

    # Storage Commitment on the main association, as today (a separate SC
    # association burned ACSE timeouts; see _send_storage_commitment).
    ae.add_requested_context(StorageCommitmentPushModel)

    # ----- main association with retries (single config knob) + outcome -----
    # Classify the associate() outcome. pynetdicom 3.0.4 returns a
    # non-established Association for TCP connect-refused/RST/timeout AND for
    # ACSE silence (the latter two are indistinguishable at the API boundary,
    # both `is_established == False`); it RAISES only for terminal local/
    # lookup/transport errors (e.g. socket.gaierror on DNS). Therefore:
    #   * an exception from associate()  -> TERMINAL transport -> NO retry
    #     (immediate failure; retrying a dead/unresolvable peer is only noise -
    #      PS3.8 9.1.4 Note 2: retry policy is implementation freedom);
    #   * a non-established return        -> ACSE-retry per outbound_assoc_attempts
    #     (a live peer whose ACSE is momentarily silent may come back, existing
    #      behaviour). Refused and silent are indistinguishable at this API
    #      boundary, so both take the retry path.
    attempts = max(1, int(outbound_assoc_attempts or 3))
    assoc = None
    outcome = None  # 'established' | 'acse-retry' | 'terminal' (last attempt)
    for attempt in range(1, attempts + 1):
        try:
            assoc = ae.associate(target_host, target_port, ae_title=move_dest_ae)
        except (OSError, RuntimeError) as _tcp_err:
            # Terminal transport: connection refused/RST/DNS/lookup surfaced as
            # an exception - no point retrying a peer/protocol that is down.
            outcome = "terminal"
            logging.error(
                "[C-MOVE] %s@%s:%s: terminal transport (attempt %d/%d) - "
                "not retrying: %s: %s",
                move_dest_ae,
                target_host,
                target_port,
                attempt,
                attempts,
                type(_tcp_err).__name__,
                _tcp_err,
            )
            raise RuntimeError(
                f"Cannot associate with {move_dest_ae} at "
                f"{target_host}:{target_port} - terminal transport "
                f"({type(_tcp_err).__name__}: {_tcp_err})"
            ) from _tcp_err
        if assoc.is_established:
            try:
                associations_total.labels(role="to", ae=ae_label(move_dest_ae)).inc()
            except Exception:
                pass
            outcome = "established"
            break
        # ACSE silence / refused-RST swallowed to a non-established return:
        # retry per the single knob (immediate), PS3.8 9.1.4 Note 2.
        logging.info(
            "[C-MOVE] %s@%s:%s main association not established "
            "(attempt %d/%d) - acse-retry immediately (PS3.8 9.1.4 Note 2: "
            "retry policy is implementation freedom)",
            move_dest_ae,
            target_host,
            target_port,
            attempt,
            attempts,
        )
        outcome = "acse-retry"
    if not assoc or not assoc.is_established:
        raise RuntimeError(
            f"Cannot associate with {move_dest_ae} at {target_host}:{target_port}"
        )

    # Build set-valued map: dict[sop_class_uid -> SET[ts]] - a rejected
    # context for one TS must not erase another accepted TS of the same class.
    accepted_contexts_map = {}
    for ctx in assoc.accepted_contexts:
        sop_uid = str(ctx.abstract_syntax)
        ts_uid = str(ctx.transfer_syntax[0]) if ctx.transfer_syntax else None
        if ts_uid:
            accepted_contexts_map.setdefault(sop_uid, set()).add(ts_uid)
    contexts_acc = sum(len(v) for v in accepted_contexts_map.values())
    logging.info(
        "[C-MOVE] Association with %s: contexts_req=%d contexts_acc=%d "
        "contexts_dropped=%d assocs=%d attempts[1/%d] outcome=%s",
        move_dest_ae,
        contexts_req,
        contexts_acc,
        contexts_dropped,
        1,
        attempts,
        outcome or "established",
    )
    return assoc, accepted_contexts_map


# ---------------------------------------------------------------------------
# _ensure_file_meta_bytes - synthesize/rewrite group 0002 (canonical trio)
# ---------------------------------------------------------------------------
_LONG_VRS = {
    b"OB",
    b"OD",
    b"OF",
    b"OL",
    b"OV",
    b"OW",
    b"SQ",
    b"UC",
    b"UN",
    b"UR",
    b"UT",
}


def _meta_data_start(dicom_bytes):
    """Preamble-aware start of the meta group: 132 (full preamble), 4
    (bare DICM) or 0 (no magic at all)."""
    if len(dicom_bytes) >= 132 and dicom_bytes[128:132] == b"DICM":
        return 132
    if len(dicom_bytes) >= 4 and dicom_bytes[:4] == b"DICM":
        return 4
    return 0


def _walk_meta(dicom_bytes):
    """Walk group 0002 (explicit VR LE); return (data_start, elements,
    meta_end). Reader-side tolerance: the value skip is vlen + (vlen % 2),
    so old externally-padded files keep parsing."""
    data_start = _meta_data_start(dicom_bytes)
    pos = data_start
    elems = []
    while pos + 8 <= len(dicom_bytes):
        group = struct.unpack_from("<H", dicom_bytes, pos)[0]
        if group != 0x0002:
            break
        elem = struct.unpack_from("<H", dicom_bytes, pos + 2)[0]
        vr = dicom_bytes[pos + 4 : pos + 6]
        if vr in _LONG_VRS:
            if pos + 12 > len(dicom_bytes):
                break
            vlen = struct.unpack_from("<I", dicom_bytes, pos + 8)[0]
            vo = pos + 12
        else:
            vlen = struct.unpack_from("<H", dicom_bytes, pos + 6)[0]
            vo = pos + 8
        if vo + vlen > len(dicom_bytes):
            break
        elems.append((elem, dicom_bytes[vo : vo + vlen]))
        pos = vo + vlen + (vlen % 2)
    return data_start, elems, pos


def _ui_elem(elem, value):
    """One canonical UI meta element (3008): even length with the trailing
    NULL INSIDE the length (PS3.5 sec 6.2), no external pad byte."""
    v = value.encode("ascii") if isinstance(value, str) else bytes(value)
    if len(v) % 2:
        v += b"\x00"
    return struct.pack("<HH2sH", 0x0002, elem, b"UI", len(v)) + v


def _extract_meta_ui(dicom_bytes, elem):
    """Byte-level read of the source meta group for (0002,elem), explicit
    VR LE, preamble-aware. Returns the UID string or None."""
    _, elems, _ = _walk_meta(dicom_bytes)
    for e, value in elems:
        if e == elem:
            raw = value.rstrip(b"\x00 ").decode("ascii", "replace")
            return raw if raw else None
    return None


def _skip_undefined_element(dicom_bytes, value_off, implicit_vr=False):
    """Skip an undefined-length body (SQ/UN element or item content):
    walks items AND raw dataset elements until the enclosing sequence
    delimitation item (FFFE,E0DD). Undefined-length items and elements
    recurse; an item delimitation item (FFFE,E00D) ends only the inner
    body, a sequence delimitation ends ALL enclosing bodies (the
    ZONARE 0109 shape omits E00D). implicit_vr: elements carry NO VR
    field (tag + 4-byte LE length); items and delimitation codes are
    identical in both encodings. Returns (offset, is_final): offset
    AFTER the terminator (None on malformed bytes), is_final=True when
    the terminator was E0DD."""
    p = value_off
    n = len(dicom_bytes)
    while p + 8 <= n:
        g = struct.unpack_from("<H", dicom_bytes, p)[0]
        e = struct.unpack_from("<H", dicom_bytes, p + 2)[0]
        if (g, e) == (0xFFFE, 0xE0DD):  # sequence delimitation
            return p + 8, True
        if (g, e) == (0xFFFE, 0xE00D):  # item delimitation
            return p + 8, False
        if (g, e) == (0xFFFE, 0xE000):  # item
            it_len = struct.unpack_from("<I", dicom_bytes, p + 4)[0]
            if it_len == 0xFFFFFFFF:
                p, final = _skip_undefined_element(
                    dicom_bytes,
                    p + 8,
                    implicit_vr,
                )
                if p is None:
                    return None, False
                if final:
                    return p, True  # E0DD closed this body too
                continue
            p = p + 8 + it_len + (it_len % 2)
            continue
        # Regular dataset element (explicit or implicit VR LE).
        if implicit_vr:
            if p + 8 > n:
                return None, False
            vlen = struct.unpack_from("<I", dicom_bytes, p + 4)[0]
            vo = p + 8
        else:
            vr = dicom_bytes[p + 4 : p + 6]
            if vr in _LONG_VRS:
                if p + 12 > n:
                    return None, False
                vlen = struct.unpack_from("<I", dicom_bytes, p + 8)[0]
                vo = p + 12
            else:
                vlen = struct.unpack_from("<H", dicom_bytes, p + 6)[0]
                vo = p + 8
        if vlen == 0xFFFFFFFF:
            p, final = _skip_undefined_element(dicom_bytes, vo, implicit_vr)
            if p is None:
                return None, False
            if final:
                return p, True
        else:
            if vo + vlen > n:
                return None, False
            p = vo + vlen + (vlen % 2)
    return None, False


def _extract_dataset_ui(dicom_bytes, elem, group=0x0008, implicit_vr=False):
    """Byte-level scan of the top-level dataset for (group,elem) - SOP
    Class UID (0x0008,0x0016) / SOP Instance UID (0x0008,0x0018) /
    Series UID (0x0020,0x000E). Explicit VR LE by default; implicit_vr
    reads elements WITHOUT the VR field (tag + 4-byte LE length - the
    stored IVRLE shape). Undefined-length elements
    (SQ/UN) are skipped over their item lists (0109 field: ZONARE
    (0008,2112) SQ stopped the scan before (0020,000E) and killed the
    C-MOVE refetch rescue)."""
    pos = _meta_data_start(dicom_bytes)
    # Skip the meta group (always explicit VR LE).
    while pos + 8 <= len(dicom_bytes):
        g = struct.unpack_from("<H", dicom_bytes, pos)[0]
        if g != 0x0002:
            break
        vr = dicom_bytes[pos + 4 : pos + 6]
        if vr in _LONG_VRS:
            if pos + 12 > len(dicom_bytes):
                return None
            vlen = struct.unpack_from("<I", dicom_bytes, pos + 8)[0]
            vo = pos + 12
        else:
            vlen = struct.unpack_from("<H", dicom_bytes, pos + 6)[0]
            vo = pos + 8
        pos = vo + vlen + (vlen % 2)
    # Walk the dataset (explicit or implicit VR LE).
    while pos + 8 <= len(dicom_bytes):
        g = struct.unpack_from("<H", dicom_bytes, pos)[0]
        e = struct.unpack_from("<H", dicom_bytes, pos + 2)[0]
        if implicit_vr:
            vlen = struct.unpack_from("<I", dicom_bytes, pos + 4)[0]
            vo = pos + 8
        else:
            vr = dicom_bytes[pos + 4 : pos + 6]
            if vr in _LONG_VRS:
                if pos + 12 > len(dicom_bytes):
                    break
                vlen = struct.unpack_from("<I", dicom_bytes, pos + 8)[0]
                vo = pos + 12
            else:
                vlen = struct.unpack_from("<H", dicom_bytes, pos + 6)[0]
                vo = pos + 8
        if vlen == 0xFFFFFFFF:
            # Undefined-length element (SQ/UN): skip the item list to
            # the delimitation item - UIDs never live inside items.
            after, _final = _skip_undefined_element(
                dicom_bytes,
                vo,
                implicit_vr,
            )
            if after is None:
                break
            pos = after
            continue
        if vo + vlen > len(dicom_bytes):
            break
        if (g, e) == (group, elem):
            raw = dicom_bytes[vo : vo + vlen].rstrip(b"\x00 ")
            s = raw.decode("ascii", "replace")
            return s if s else None
        pos = vo + vlen + (vlen % 2)
    return None


def _ensure_file_meta_bytes(dicom_bytes, sop_class_uid, sop_uid, ts_uid):
    """Return bytes whose meta group is the canonical ascending trio -
    (0002,0002) MediaStorageSOPClassUID, (0002,0003)
    MediaStorageSOPInstanceUID, (0002,0010) TransferSyntaxUID LAST - each
    Explicit VR LE UI with an even value length and the trailing NULL
    INSIDE the length (PS3.5 sec 6.2).

    cltranscode-stored files have NO group 0002 at
    all, and the server's transcoded WADO response copies that gap.
    pynetdicom `send_c_store` then refuses the file and the whole C-MOVE
    dies with 0xB000.

    ONE canonical writer. UID resolution order: (a) byte-level meta
    group, (b) byte-level dataset scan for (0008,0016)/(0008,0018),
    (c) the pydicom-parsed value. A parser returning '' never suppresses
    (0002,0003). If the existing meta group is already byte-exactly the
    canonical trio, the input is returned unchanged; otherwise the WHOLE
    group is replaced (never spliced after non-ascending or
    externally-padded elements).
    """
    data_start = _meta_data_start(dicom_bytes)
    if len(dicom_bytes) >= 132 and dicom_bytes[128:132] == b"DICM":
        prefix = dicom_bytes[:132]
    elif len(dicom_bytes) >= 4 and dicom_bytes[:4] == b"DICM":
        prefix = dicom_bytes[:4]
    else:
        # No DICM magic at all: build a full preamble (pynetdicom reads
        # files, DICM expected).
        prefix = b"\x00" * 128 + b"DICM"

    _, existing, meta_end = _walk_meta(dicom_bytes)

    # 02.09 field Job #38: ts_uid==IVRLE means the dataset bytes are
    # implicit VR (no VR field). The meta group is ALWAYS explicit, so
    # only the dataset fallback scans switch encoding.
    _implicit = ts_uid == ImplicitVRLittleEndian
    resolved_sc = (
        _extract_meta_ui(dicom_bytes, 0x0002)
        or _extract_dataset_ui(dicom_bytes, 0x0016, implicit_vr=_implicit)
        or (sop_class_uid or None)
    )
    resolved_si = (
        _extract_meta_ui(dicom_bytes, 0x0003)
        or _extract_dataset_ui(dicom_bytes, 0x0018, implicit_vr=_implicit)
        or (sop_uid or None)
    )
    ts = ts_uid or "1.2.840.10008.1.2.1"

    trio = b""
    if resolved_sc:
        trio += _ui_elem(0x0002, resolved_sc)
    if resolved_si:
        trio += _ui_elem(0x0003, resolved_si)
    trio += _ui_elem(0x0010, ts)

    if dicom_bytes[data_start:meta_end] == trio:
        return dicom_bytes
    return prefix + trio + dicom_bytes[meta_end:]


def _pick_refetch_ts(accepted_ts_for_class, server_ts_list):
    """Choose the refetch transfer syntax for a stored-not-accepted instance.

    The bridge NEVER opens a second/probe association; a stored TS the viewer
    did not accept is delivered only if the server can produce an accepted TS.
    The refetch target is drawn from the intersection of the viewer's accepted
    set for the class and the server's producible set (caps). Priority within
    that intersection: EVRLE, then IVRLE, then any remaining accepted TS
    (deterministic order).

    The caps endpoint declares only the compressed transcode-target codecs and
    does NOT declare EVRLE/IVRLE, yet a server that serves `?transferSyntax=`
    (transcode-capable) requests is expected to be able to return an
    uncompressed form. PS3.18 8.7.3.1 (Table 8.7.3-2) makes Explicit VR Little
    Endian the Default (D) media-type transfer syntax for Image/Video/Text/Other
    instances and returns it "unless the origin server has only access to the
    pixel data in lossy compressed form or ... such length that it cannot be
    encoded in the Explicit VR Little Endian Transfer Syntax" - i.e. EVRLE is
    the standard default representation, not an optional transcode target, so a
    transcode-capable origin server can be expected to produce it.
    Therefore, when the server reports transcode capabilities
    (`server_ts_list` non-empty) EVRLE is treated as producible and added to
    the candidate set before the intersection - a transcode build has an EVRLE
    encoder. This is not inventing a capability. If the server in fact cannot
    produce EVRLE the refetch itself returns 406 and the instance fails
    honestly (the same outcome as before, so this cannot regress). When
    `server_ts_list` is EMPTY (a no-transcode server) EVRLE is NOT added -
    nothing can be transcoded, a refetch call would be wasted, and the
    instance fails honestly as today.

    Returns the chosen TS UID, or '' when no deliverable form exists (the
    caller must fail the instance honestly).
    """
    if isinstance(accepted_ts_for_class, str):
        acc = {accepted_ts_for_class} if accepted_ts_for_class else set()
    elif isinstance(accepted_ts_for_class, (list, tuple)):
        acc = set(accepted_ts_for_class)
    else:
        acc = set(accepted_ts_for_class or set())
    prod = set(server_ts_list or [])
    if prod:
        # A transcode-capable origin server is expected to be able to produce
        # EVRLE (PS3.18 8.7.3.1: EVRLE is the Default D transfer syntax for
        # image instances). EVRLE is never invented on a no-transcode server
        # (prod empty below).
        prod.add(ExplicitVRLittleEndian)
    cand = acc & prod
    if not cand:
        return ""
    for ts in (ExplicitVRLittleEndian, ImplicitVRLittleEndian):
        if ts in cand:
            return ts
    return sorted(cand)[0]


def _decide_send_path(accepted_ts_for_class, actual_ts, is_image):
    """Pure per-instance send-path decision (no probe is ever opened).

    Returns 'send_as_is' (C-STORE the stored bytes on the main association
    - the stored TS is accepted by the viewer, or it is a non-image SOP, or
    the stored TS is unknown) or 'needs_deliver' (a stored TS the viewer did
    NOT accept on the main association - the caller then chooses a
    deliverable refetch TS via `_pick_refetch_ts`, or fails the instance
    honestly when the server cannot produce one).

    obligation: the bridge NEVER opens a probe / second association to a
    C-MOVE destination. The fallback path is gone; the only outcomes per
    stored-not-accepted image are refetch on the MAIN association (if the
    server can produce an accepted TS) or an honest per-instance failure with
    reason (no silent drops).
    """
    if not is_image:
        return "send_as_is"
    if not actual_ts:
        return "send_as_is"
    accepted = accepted_ts_for_class
    if isinstance(accepted, str):
        accepted = {accepted} if accepted else set()
    elif isinstance(accepted, (list, tuple)):
        accepted = set(accepted)
    else:
        accepted = accepted or set()
    if accepted and actual_ts in accepted:
        return "send_as_is"
    return "needs_deliver"


def _pydicom_file_meta_ts(dicom_bytes):
    """Last-resort transfer-syntax read (pydicom). Used ONLY when neither
    the part Content-Type parameter nor the byte-level (0002,0010) read
    produced a TS. Returns '' on any failure."""
    try:
        ds = dcmread(io.BytesIO(dicom_bytes), stop_before_pixels=True)
        return str(
            getattr(getattr(ds, "file_meta", None), "TransferSyntaxUID", "") or ""
        )
    except Exception:
        return ""


# ---------------------------------------------------------------------------
# _stream_and_send_study  -  stream study from Clarus, send via C-STORE
# ---------------------------------------------------------------------------
def _stream_and_send_study(
    assoc,
    study_uid,
    clarus_url,
    http_timeout,
    move_jobs,
    job_id,
    accepted_contexts_map=None,
    server_ts_list=None,
    sop_registry=None,
    tls=None,
    spill_threshold=SPILL_THRESHOLD,
    peer_ae="",
    _skip_ids=None,
    _resumed=False,
    _report_lost=False,
    expose_rejected=False,
    qa_url="",
):
    """Stream study from Clarus via WADO-RS, send via C-STORE.

    stored-first: ONE study-level streaming multipart GET WITHOUT
    ?transferSyntax= (stored bytes, zero server CPU). Per instance the
    stored transfer syntax is decided byte-level.

    The bridge NEVER opens a probe / second association to a C-MOVE
    destination. Delivery per instance follows the negotiation result on the
    MAIN association:
    - stored TS accepted -> send_as_is (passthrough);
    - stored TS not accepted AND the server can produce an accepted TS (the
      intersection of the viewer's accepted set and the server caps) ->
      refetch the instance on the MAIN association with ?transferSyntax=
      (priority EVRLE -> IVRLE -> any);
    - otherwise -> an honest per-instance failure (the undelivered SOP UID
      lands in the (0008,0058) list of the final response).

    Resume: on an association loss the driver re-associates ONCE and calls
    this function a SECOND time with *_skip_ids* = the SOP UIDs already
    delivered on the first pass (they are refused a re-send, so every instance
    is delivered exactly once), *_resumed*=True (adds resumed=1 to the final
    INFO), and *_report_lost=True so this pass can report a further association
    loss via a trailing boolean. A resumed pass issues a FRESH study-level GET
    (a conformant continuation - PS3.7 9.1.4.2 (e) sub-operations may occur on
    a different Association than the C-MOVE; PS3.8 9.1.4 Note 2 + 9.1.5
    re-association freedom). When *_report_lost* is False the returned tuple
    has the historical 7 elements; when True an 8th element `lost` (0/1) is
    appended so the driver can tell "association lost again -> GIVE_UP".

    sop_registry: SOP class registry for image/non-image classification.
    spill_threshold: multipart parser spill threshold (W3); tests inject a
    small value, production uses the 16 MB default.

    Returns (completed, failed, total, sent_sop_uids, failed_sop_uids,
             cancelled, no_common_ts[, lost]). failed_sop_uids = the SOP
    instance UIDs that were seen and not delivered in THIS pass (PS3.4
    C.4.2.1.4.2 material); the driver reconciles the (0008,0058) tail across
    passes from the study-level list.
    """
    completed = 0
    failed = 0
    total = 0
    total_bytes = 0
    passthrough = 0
    transcoded = 0
    refetched = 0
    sent_sop_uids = []
    # 0709: every SOP instance UID this job attempted, so the failed set
    # (seen minus delivered) feeds the (0008,0058) Failed SOP Instance UID
    # List on the final C-MOVE-RSP (PS3.4 C.4.2.1.4.2).
    seen_sop_uids = set()

    def _out(cancelled=False, no_common=False, lost=False):
        """Assemble the return tuple; a `lost` element is appended ONLY when
        the driver requested it via `_report_lost` (test_spill unpacks the
        historical 7 elements and does not set `_report_lost`). The
        (0008,0058) tail is computed here from the current seen/delivered
        sets so the helper has no forward dependency."""
        _d = {u for _c, u in sent_sop_uids if u}
        _f = sorted(u for u in seen_sop_uids if u not in _d)
        base = (completed, failed, total, sent_sop_uids, _f, cancelled, no_common)
        if _report_lost:
            return base + (lost,)
        return base

    # Negotiate transfer syntax between viewer and server
    server_ts = server_ts_list if server_ts_list else []
    if accepted_contexts_map is not None and not accepted_contexts_map:
        # No accepted presentation context and therefore no refetch target.
        # The job finishes failed with zero slices delivered and the C-MOVE
        # generator reports a Failure (0xC000, PS3.4 Table C.4-2 "Failed:
        # Unable to process") - the wire status and this log agree. 0xC001
        # stayed a log-only claim in the past; it is NOT a response status.
        logging.warning(
            "[C-MOVE] Viewer accepted no presentation contexts - "
            "no common transfer syntax; job fails with 0 slices, "
            "C-MOVE-RSP is a Failure (0xC000)",
        )
        return _out(no_common=True)
    if server_ts:
        # Stored-first: the study-level GET NEVER carries
        # ?transferSyntax=. The server can transcode; per-instance
        # refetch picks a target from the accepted set.
        logging.info(
            "[C-MOVE] Server transcode available (%d TS); streams stored "
            "bytes; not-accepted instances refetch on the main association",
            len(server_ts),
        )
    else:
        # Passthrough mode: server has no transcode capabilities. Don't
        # request ?transferSyntax=. Server returns stored bytes as-is. The
        # bridge forwards whatever it receives; a stored TS the viewer did
        # not accept cannot be produced -> honest per-instance failure.
        logging.info(
            "[C-MOVE] Passthrough mode - server has no transcode; stored "
            "bytes only, not-accepted instances fail honestly",
        )

    # ONE study-level streaming GET WITHOUT ?transferSyntax=.
    accept_value = "multipart/related; type=application/dicom"
    # The study UID goes into the path through `_quote`: it is client input, and
    # `1.2?a=b` or `../x` in one would steer the GET at a different resource.
    url = (
        f"{service_base_url(clarus_url, qa_url, expose_rejected)}"
        f"/studies/{_quote(study_uid, safe='')}"
    )
    headers = {"Accept": accept_value}

    logging.info("[C-MOVE] Streaming study %s from DICOMweb server...", study_uid)
    resp = requests.get(
        url, headers=headers, timeout=http_timeout, stream=True, cert=tls
    )
    resp.raise_for_status()

    # B1 (2508): a reader thread prefetches the streamed multipart parts into a
    # bounded FIFO while this sender loop C-STORE-s them - the serial pipeline
    # left the Clarus server leg idle during each network C-STORE. Structural
    # backpressure is the bounded queue (4 parts ~ 20 MB, FIFO). The sender
    # loop below is unchanged; every early return must stop the reader thread.
    prefetch = PartPrefetcher(
        _stream_multipart_instances(resp, spill_threshold=spill_threshold)
    ).start()

    import os as _os
    import tempfile

    def _refetch_and_send(
        idx, sop_uid, sop_class_uid, actual_ts, refetch_ts, series_uid, peer_ae=""
    ):
        """Per-instance refetch + C-STORE on the MAIN association.

        Called when the stored TS is not accepted but a deliverable TS exists
        (the caller passes `refetch_ts` chosen from the accepted x server
        intersection via `_pick_refetch_ts` - never a probe/second
        association). A series UID is required to build the instance URL.
        Updates the refetched/failed counters and returns the C-STORE status
        (None = failure already counted).
        """
        nonlocal refetched, failed
        if not (refetch_ts and series_uid):
            reason = (
                "no series UID for the instance URL"
                if not series_uid
                else "no accepted refetch TS for this class"
            )
            logging.warning(
                "[C-MOVE] Instance %d (%s): stored TS %s not accepted "
                "and refetch impossible (%s) - counted failed",
                idx,
                sop_uid,
                actual_ts,
                reason,
            )
            failed += 1
            return None
        refetched += 1
        # Every UID goes into the path through `_quote`: a UID is not trusted
        # input just because it arrived over DIMSE, and `1.2?a=b` or `../x` in
        # one would steer the GET at a different resource than the instance the
        # peer asked for.
        inst_url = (
            f"{service_base_url(clarus_url, qa_url, expose_rejected)}"
            f"/studies/{_quote(study_uid, safe='')}"
            f"/series/{_quote(series_uid, safe='')}"
            f"/instances/{_quote(sop_uid, safe='')}"
            f"?transferSyntax={_quote(refetch_ts, safe='')}"
        )
        logging.info(
            "[C-MOVE] Instance %d (%s): stored TS %s not accepted; "
            "refetching on the MAIN association with transfer-syntax=%s",
            idx,
            sop_uid,
            actual_ts,
            refetch_ts,
        )
        try:
            r2 = requests.get(
                inst_url,
                headers={"Accept": f"application/dicom; transfer-syntax={refetch_ts}"},
                timeout=http_timeout,
                cert=tls,
            )
            r2.raise_for_status()
        except requests.RequestException as rf_err:
            logging.warning(
                "[C-MOVE] Instance %d (%s): refetch failed: %s",
                idx,
                sop_uid,
                rf_err,
            )
            failed += 1
            return None
        new_bytes = r2.content
        with tempfile.NamedTemporaryFile(suffix=".dcm", delete=False) as _f2:
            _f2.write(
                _ensure_file_meta_bytes(
                    new_bytes,
                    sop_class_uid,
                    sop_uid,
                    refetch_ts,
                )
            )
            _rtmp = _f2.name
        try:
            _t0 = time.perf_counter()
            status = assoc.send_c_store(_rtmp)
        except Exception as rs_err:
            logging.warning(
                "[C-MOVE] Instance %d (%s): C-STORE of refetched " "bytes failed: %s",
                idx,
                sop_uid,
                rs_err,
            )
            status = None
            _rtmp_size = None
        else:
            try:
                _rtmp_size = _os.path.getsize(_rtmp)
            except OSError:
                _rtmp_size = None
        finally:
            try:
                _os.unlink(_rtmp)
            except OSError:
                pass
        try:
            cstore_outbound_duration.labels(peer=ae_label(peer_ae)).observe(
                time.perf_counter() - _t0
            )
            if _rtmp_size is not None:
                bytes_total.labels(direction="tx", peer=ae_label(peer_ae)).inc(
                    _rtmp_size
                )
        except Exception:
            pass
        if status is None:
            failed += 1
        return status

    def _refetch_after_rejection(
        inst_idx, sop_uid, sop_class_uid, actual_ts, dicom_bytes, peer_ae=""
    ):
        """Refetch one not-accepted instance on the MAIN association.

        Refetch target = a TS the viewer accepts AND the server can produce,
        chosen by `_pick_refetch_ts` (EVRLE -> IVRLE -> any). No probe /
        second association is ever used. Returns the C-STORE status
        (None = failure already counted by _refetch_and_send; the caller then
        lets the undelivered SOP UID land in the (0008,0058) failed list).
        """
        _acc = (
            accepted_contexts_map.get(sop_class_uid) if accepted_contexts_map else None
        )
        refetch_ts = _pick_refetch_ts(_acc, server_ts)
        if not refetch_ts:
            # No deliverable form - honest per-instance failure. The SOP
            # UID stays in seen_sop_uids so the final (0008,0058) lists it.
            logging.warning(
                "[C-MOVE] Instance %d (%s): stored TS %s not accepted and "
                "no accepted TS the server can produce - counted failed",
                inst_idx,
                sop_uid,
                actual_ts,
            )
            return None
        # Field-observed: stored IVRLE bytes carry no VR field -
        # scan in implicit mode so the series UID (and the refetch URL)
        # survives the refetch rescue.
        series_uid = (
            _extract_dataset_ui(
                dicom_bytes,
                0x000E,
                group=0x0020,
                implicit_vr=(actual_ts == ImplicitVRLittleEndian),
            )
            or ""
        )
        return _refetch_and_send(
            inst_idx,
            sop_uid,
            sop_class_uid,
            actual_ts,
            refetch_ts,
            series_uid,
            peer_ae,
        )

    def _unlink_quiet(path):
        """Best-effort temp-file cleanup - cleanup must never fail the job."""
        try:
            _os.unlink(path)
        except OSError:
            pass

    def _failed_uids():
        """Sorted (0008,0058) material: seen-but-not-delivered SOP instance
        UIDs (PS3.4 C.4.2.1.4.2). Deterministic order for the wire list."""
        _delivered = {u for _c, u in sent_sop_uids if u}
        return sorted(u for u in seen_sop_uids if u not in _delivered)

    # The streamed response stays open for the whole loop; every exit
    # path must release it, early returns included, or the pooled
    # connection stays checked out for the life of the process.
    try:
        for part, idx, part_ts in prefetch:
            # A spilled part arrives as (head bytes, temp-file path).
            # The head carries the file meta group read below; the rest is
            # streamed from disk, never read whole into memory.
            spill_path = None
            if isinstance(part, SpilledPart):
                dicom_bytes = part.head
                spill_path = part.path
            else:
                dicom_bytes = part

            # Bridge: check C-MOVE-CANCEL before each C-STORE
            if move_jobs.is_job_cancelled(job_id):
                logging.warning(
                    "[C-MOVE] Job #%d cancelled by SCU after %d/%d instances",
                    job_id,
                    completed,
                    total if total else idx,
                )
                if spill_path is not None:
                    _unlink_quiet(spill_path)
                prefetch.stop()
                return _out(cancelled=True)

            # Bridge: check shutdown flag during long transfers
            if _shutdown_requested.is_set():
                logging.warning("[C-MOVE] Shutdown requested, aborting job #%d", job_id)
                if spill_path is not None:
                    _unlink_quiet(spill_path)
                prefetch.stop()
                return _out(cancelled=True)

            part_size = len(dicom_bytes)
            if spill_path is not None:
                part_size += _os.path.getsize(spill_path)
            total_bytes += part_size
            total = idx + 1  # Update total as we stream
            # DEBUG: log only every 50th part + first/last to avoid I/O spam
            if idx == 0 or idx % 50 == 0:
                logging.debug("[C-MOVE] Streamed part %d: %d bytes", idx + 1, part_size)

            # Stored-first hot loop: byte-level UIDs/TS, NO dcmread in the
            # send path (ap101 discipline - pydicom only as the TS last resort).
            sent_via_refetch = False
            try:
                # The transfer syntax decides how the dataset below must be
                # scanned, so it is resolved FIRST: an Implicit VR instance has no
                # VR field in any element, and reading it as explicit VR makes
                # every tag walk stop at the first element - the SOP UIDs then come
                # back empty and a delivered instance is counted failed.
                # Chain, PER PART:
                # 1) transfer-syntax= of the part Content-Type header;
                # 2) byte-level read of (0002,0010);
                # 3) pydicom - last resort only.
                actual_ts = (
                    part_ts
                    or _extract_meta_ui(dicom_bytes, 0x0010)
                    or _pydicom_file_meta_ts(dicom_bytes)
                )
                implicit_vr = actual_ts == ImplicitVRLittleEndian
                sop_class_uid = (
                    _extract_meta_ui(dicom_bytes, 0x0002)
                    or _extract_dataset_ui(dicom_bytes, 0x0016, implicit_vr=implicit_vr)
                    or ""
                )
                sop_uid = (
                    _extract_meta_ui(dicom_bytes, 0x0003)
                    or _extract_dataset_ui(dicom_bytes, 0x0018, implicit_vr=implicit_vr)
                    or ""
                )
                if sop_uid:
                    seen_sop_uids.add(str(sop_uid))
                # Resume: an instance already DELIVERED on the first pass
                # must not be re-sent or re-counted on the resumed pass. Skip it
                # (bytes were only fetched to identify it); exactly-once holds.
                if _skip_ids and sop_uid and str(sop_uid) in _skip_ids:
                    if spill_path is not None:
                        _unlink_quiet(spill_path)
                    logging.debug(
                        "[C-MOVE] Resume: skipping already-delivered instance %s "
                        "(part %d)",
                        sop_uid,
                        idx + 1,
                    )
                    continue
                # actual_ts was resolved before the byte scans above, because it
                # decides how those scans read the dataset.
                viewer_ts = accepted_contexts_map.get(sop_class_uid) or set()
                is_image = _is_image_sop(sop_class_uid, sop_registry)
                decision = _decide_send_path(viewer_ts, actual_ts, is_image)
                # The bridge NEVER opens a probe/second association.
                # A stored TS the viewer did NOT accept is either delivered on the
                # MAIN association by refetching in a producible accepted TS
                # (EVRLE -> IVRLE -> any from accepted x server intersection), or
                # - when the server cannot produce one - failed honestly.
                if decision == "needs_deliver":
                    refetch_ts = _pick_refetch_ts(viewer_ts, server_ts)
                    decision = "refetch_main" if refetch_ts else "fail"

                # Pass-through: raw bytes -> temp file -> pynetdicom reads directly.
                # No dcmread(), no write_dataset(). Bytes untouched.
                with tempfile.NamedTemporaryFile(suffix=".dcm", delete=False) as _f:
                    _f.write(
                        _ensure_file_meta_bytes(
                            dicom_bytes,
                            sop_class_uid,
                            sop_uid,
                            actual_ts,
                        )
                    )
                    if spill_path is not None:
                        # W3 (0209): append the spilled excess from disk in
                        # 1 MiB chunks - the spill file is never read whole.
                        with open(spill_path, "rb") as _src:
                            shutil.copyfileobj(_src, _f, 1024 * 1024)
                    _tmp_path = _f.name
                try:
                    if decision == "send_as_is":
                        # Stored TS accepted for this class -> main association.
                        _t0 = time.perf_counter()
                        status = assoc.send_c_store(_tmp_path)
                        try:
                            cstore_outbound_duration.labels(
                                peer=ae_label(peer_ae)
                            ).observe(time.perf_counter() - _t0)
                            bytes_total.labels(
                                direction="tx", peer=ae_label(peer_ae)
                            ).inc(part_size)
                        except Exception:
                            pass
                    elif decision == "refetch_main":
                        # Not accepted + a producible accepted TS exists ->
                        # refetch ON THE MAIN association in that TS. No fallback,
                        # no probe, no second association.
                        status = _refetch_after_rejection(
                            idx + 1,
                            sop_uid,
                            sop_class_uid,
                            actual_ts,
                            dicom_bytes,
                            peer_ae,
                        )
                        if status is None:
                            # _refetch_after_rejection returned ''-target or
                            # _refetch_and_send already counted the failure.
                            continue
                        sent_via_refetch = True
                    else:  # decision == "fail"
                        # Not accepted and no producible accepted TS (server lacks
                        # transcode or nothing in common): honest per-instance
                        # failure - the SOP UID stays in seen_sop_uids so the
                        # final (0008,0058) lists it. No silent drop (R3).
                        logging.warning(
                            "[C-MOVE] Instance %d (%s): stored TS %s not accepted "
                            "and the server cannot produce an accepted TS - "
                            "counted failed",
                            idx + 1,
                            sop_uid,
                            actual_ts,
                        )
                        failed += 1
                        continue
                    if status is None:
                        failed += 1
                        continue
                except (TypeError, ValueError) as send_err:
                    err_msg = str(send_err)
                    if "presentation context" in err_msg.lower():
                        logging.warning(
                            "[C-MOVE] Instance %d (%s): SOP class %s not negotiated "
                            "with viewer - viewer doesn't accept this class/TS. "
                            "Counted failed. Error: %s",
                            idx + 1,
                            sop_uid,
                            sop_class_uid,
                            send_err,
                        )
                        failed += 1
                        continue
                    else:
                        logging.warning(
                            "[C-MOVE] Instance %d (%s): cannot encode for C-STORE - "
                            "corrupted DICOM metadata (tag encoding error). "
                            "Counted failed. Error: %s",
                            idx + 1,
                            sop_uid,
                            send_err,
                        )
                        failed += 1
                        continue
                except Exception as e:
                    # pynetdicom network_timeout false positive: timer fires
                    # even while data IS flowing (e.g. 432 slices, 45s).
                    # If association is alive and C-STORE succeeded before  -  ignore.
                    err_msg = str(e)
                    is_timeout = any(
                        kw in err_msg.lower()
                        for kw in ("timeout", "timed out", "network")
                    )
                    assoc_alive = getattr(assoc, "is_established", False)
                    if is_timeout and assoc_alive and completed > 0:
                        logging.warning(
                            "[DIMSE-OUT] %s:%d: network_timeout fired, "
                            "but association alive, %d C-STORE sent, "
                            "%d bytes transferred. Ignoring - data IS flowing. "
                            "Error: %s",
                            getattr(assoc, "ae_title", "?"),
                            getattr(assoc, "port", 0),
                            completed,
                            total_bytes,
                            err_msg,
                        )
                        failed += 1
                        continue
                    else:
                        # Association DEAD  -  viewer crashed or disconnected. The
                        # driver may resume ONCE. We stop this pass's
                        # stream and report `lost=True` (only when the driver
                        # requested it) so `_execute_cmove_job` can re-associate
                        # and run a skip-delivered second pass.
                        logging.warning(
                            "[DIMSE-OUT] %s:%d: association LOST after %d/%d "
                            "C-STORE sent (%d failed). %s Error: %s",
                            getattr(assoc, "ae_title", "?"),
                            getattr(assoc, "port", 0),
                            completed,
                            total if total else idx + 1,
                            failed + 1,
                            (
                                "Resuming once."
                                if _report_lost and not _resumed
                                else "Viewer must re-request or resume fails."
                            ),
                            err_msg,
                        )
                        prefetch.stop()
                        resp.close()
                        return _out(lost=True)
                finally:
                    _unlink_quiet(_tmp_path)
                    if spill_path is not None:
                        _unlink_quiet(spill_path)

                if status and status.Status == 0x0000:
                    completed += 1
                    if sent_via_refetch:
                        transcoded += 1
                    else:
                        passthrough += 1
                    # Bridge: track successfully sent SOPs for Storage Commitment
                    if sop_uid:
                        sent_sop_uids.append((sop_class_uid, sop_uid))
                else:
                    failed += 1
                    logging.warning(
                        "[C-MOVE] C-STORE failed for instance %d: status=%s",
                        idx + 1,
                        _dimse_status_name(status.Status if status else None),
                    )
            except (requests.RequestException, OSError, ValueError, KeyError) as e:
                failed += 1
                if spill_path is not None:
                    # W3: the send file was never handed to pynetdicom - drop
                    # the spill file right here (the inner finally is not hit).
                    _unlink_quiet(spill_path)
                logging.warning(
                    "[C-MOVE] C-STORE error for instance %d/%d: %s",
                    idx + 1,
                    total,
                    e,
                )
                logging.debug(
                    "[C-MOVE] C-STORE traceback for instance %d/%d:",
                    idx + 1,
                    total,
                    exc_info=True,
                )

            # Bridge: update job progress counters
            move_jobs.update_job_progress(job_id, completed, failed, total)

    finally:
        prefetch.stop()
        resp.close()

    if total == 0:
        logging.info(
            "[C-MOVE] Job #%d: study %s returned 0 instances", job_id, study_uid
        )

    logging.info(
        "[C-MOVE] Job #%d study %s: passthrough=%d transcoded=%d refetched=%d"
        " resumed=%d",
        job_id,
        study_uid,
        passthrough,
        transcoded,
        refetched,
        1 if _resumed else 0,
    )
    logging.info(
        "[C-MOVE] Streamed study %s: %d instances, %d bytes total",
        study_uid,
        total,
        total_bytes,
    )

    return _out()


# ---------------------------------------------------------------------------
# Bridge: A-RELEASE with A-ABORT fallback (polite first, then force)
# ---------------------------------------------------------------------------
def _release_or_abort(assoc, timeout=3.0):
    """Polite A-RELEASE, then A-ABORT if viewer doesn't respond.

    Release politely first, but do not wait forever.
    Data is already delivered (C-STORE-RSP status=0x0000 received
    for every instance). A-RELEASE is protocol courtesy, not data integrity.

    Orthanc/dcm4che: both try A-RELEASE with a timeout, then force-close.
    pynetdicom: release() blocks indefinitely if peer doesn't respond.
    This wrapper adds the missing timeout.
    """
    if assoc is None or not assoc.is_established:
        return

    released = threading.Event()

    def _do_release():
        try:
            assoc.release()
        except Exception:
            pass
        finally:
            released.set()

    t = threading.Thread(target=_do_release, daemon=True)
    t.start()

    if not released.wait(timeout=timeout):
        peer = getattr(assoc, "ae_title", "?")
        logging.warning(
            "[DIMSE] A-RELEASE timed out after %.0fs - " "sending A-ABORT to %s",
            timeout,
            peer,
        )
        try:
            assoc.abort()
        except Exception:
            pass

    t.join(timeout=1.0)


# ---------------------------------------------------------------------------
# Bridge: execute a single C-MOVE job: stream -> send -> finish
# ---------------------------------------------------------------------------
def _execute_cmove_job(
    job_id,
    study_uid,
    move_dest_ae,
    move_jobs,
    clarus_url,  # Clarus Server DICOMweb base URL (single knob, 2908)
    http_timeout,
    bridge_ae_title,  # Bridge: own AE Title for C-STORE associations
    target_host="",  # Resolved Move Destination host (subnet support)
    target_port=0,  # Resolved Move Destination port
    sop_registry=None,
    storage_commit_after_move=True,
    sop_index=None,
    peers_file=None,
    outbound_network_timeout=60,
    outbound_dimse_timeout=60,
    outbound_acse_timeout=15,
    outbound_assoc_attempts=3,
    tls=None,
    spill_threshold=SPILL_THRESHOLD,
    expose_rejected=False,
    qa_url="",
):
    """Execute C-MOVE: stream study from Clarus, send instances to target SCP.

    Stage 1 (bridge-only): before the main A-ASSOCIATE a lightweight
    pre-flight learns each class's stored TS set from the standard metadata
    resource so the association proposal can carry per-TS contexts;
    delivery follows the negotiation result. Retry policy is the single
    outbound_assoc_attempts knob with immediate retries.

    1. Pre-flight stored TS - best effort, never fails the job.
    2. Stream study as multipart DICOM from Clarus (WADO-RS) - no temp file.
    3. Send each instance via C-STORE to move_dest_ae.
    4. Check C-MOVE-CANCEL before each C-STORE.
    5. Optionally send Storage Commitment N-ACTION after success.
    """
    # Bridge: target_host/target_port come from job (resolved in handle_c_move).
    # No re-lookup needed  -  subnet resolution happened at C-MOVE time.
    if not target_host or not target_port:
        # Legacy job without target info  -  fall back to lookup
        if peers_file:
            move_jobs.reload_peers_from_file(peers_file)
        target = move_jobs.lookup_scp(move_dest_ae)
        if target is None:
            logging.error(
                "[C-MOVE] Unknown Move Destination: %s",
                move_dest_ae,
            )
            move_jobs.finish_job(
                job_id,
                success=False,
                error_message=f"Unknown Move Destination: {move_dest_ae}",
            )
            move_jobs_counter.labels(status="failed").inc()
            return
        target_host, target_port = target

    assoc = None
    try:
        # Per-peer job gate: serialize jobs to the same peer (busy exclusion).
        # No post-job cooldown: the next job starts immediately once the
        # previous one to this peer ends. Taken inside the try so the
        # finally releases it on every path, including a pre-flight raise.
        _job_gate_enter(target_host, target_port, move_dest_ae)

        # -- Pre-flight: learn each class's stored TS BEFORE the association
        # (best effort - any failure degrades to the default proposal). --
        study_ts_by_class = _preflight_study_ts_by_class(
            study_uid,
            clarus_url,
            http_timeout=2.0,
            tls=tls,
            budget_s=4.0,
            expose_rejected=expose_rejected,
            qa_url=qa_url,
        )

        try:
            assoc, accepted_contexts_map = _associate_for_move(
                move_dest_ae,
                bridge_ae_title,
                sop_registry,
                target_host,
                target_port,
                network_timeout=outbound_network_timeout,
                dimse_timeout=outbound_dimse_timeout,
                acse_timeout=outbound_acse_timeout,
                outbound_assoc_attempts=outbound_assoc_attempts,
                study_ts_by_class=study_ts_by_class,
            )
        except RuntimeError as e:
            logging.error("[C-MOVE] %s", e)
            logging.debug("[C-MOVE] Association traceback:", exc_info=True)
            move_jobs.record_scp_failure(move_dest_ae, host=target_host)
            move_jobs.finish_job(job_id, success=False, error_message=str(e))
            move_jobs_counter.labels(status="failed").inc()
            return

        server_ts_list = _get_clarus_transfer_syntaxes(clarus_url, tls=tls)

        global _suppress_pynetdicom
        _suppress_pynetdicom = True
        # -- Resume driver: at most ONE resume. The whole study is
        # re-streamed on a FRESH GET for the resumed pass (a conformant
        # continuation; PS3.7 9.1.4.2(e) / PS3.8 9.1.4 Note 2 + 9.1.5), and
        # only the SOP UIDs NOT yet delivered are re-sent, so each instance
        # is delivered at most once. Counters are reconciled by UNION of the
        # delivered sets at the end (never sum regression). --
        study_sop_uids = None
        try:
            # Authoritative study SOP list for the (0008,0058) tail ++ total.
            study_sop_uids = _study_sop_uids(
                study_uid,
                clarus_url,
                tls=tls,
                expose_rejected=expose_rejected,
                qa_url=qa_url,
            )
        except Exception:
            study_sop_uids = None
        sent_sop_uids = []  # union across passes (SC material)
        cancelled = False
        no_common_ts = False
        resumed_lost = False

        # Pass 1 (initial).
        try:
            r1 = _stream_and_send_study(
                assoc,
                study_uid,
                clarus_url,
                http_timeout,
                move_jobs,
                job_id,
                accepted_contexts_map=accepted_contexts_map,
                server_ts_list=server_ts_list,
                sop_registry=sop_registry,
                tls=tls,
                spill_threshold=spill_threshold,
                peer_ae=move_dest_ae,
                _skip_ids=None,
                _resumed=False,
                _report_lost=True,
                expose_rejected=expose_rejected,
                qa_url=qa_url,
            )
            _c1, _f1, _t1, _s1, _fs1, _ca1, _nc1, _lost1 = r1
            sent_sop_uids = list(_s1)
            cancelled = _ca1
            no_common_ts = _nc1
            resumed_lost = _lost1
        except Exception:
            sent_sop_uids = []
            cancelled = False
            no_common_ts = False
            resumed_lost = False
            raise

        # Resume ONCE on association loss (not on cancel/shutdown/no-common).
        if resumed_lost and not cancelled and not no_common_ts:
            delivered1 = {u for _c, u in sent_sop_uids if u}
            logging.info(
                "[C-MOVE] Job #%d: association lost after %d delivered - "
                "re-associating ONCE to resume (skip %d delivered)",
                job_id,
                len(delivered1),
                len(delivered1),
            )
            _release_or_abort(assoc)
            assoc = None
            try:
                assoc, accepted_contexts_map = _associate_for_move(
                    move_dest_ae,
                    bridge_ae_title,
                    sop_registry,
                    target_host,
                    target_port,
                    network_timeout=outbound_network_timeout,
                    dimse_timeout=outbound_dimse_timeout,
                    acse_timeout=outbound_acse_timeout,
                    outbound_assoc_attempts=outbound_assoc_attempts,
                    study_ts_by_class=study_ts_by_class,
                )
            except RuntimeError as e:
                # Resume's re-association failed -> GIVE_UP (honest partial).
                logging.error(
                    "[C-MOVE] Job #%d: resume re-association failed (%s) - "
                    "GIVE_UP with the delivered prefix",
                    job_id,
                    e,
                )
                resumed_lost = True
            else:
                # Pass 2 (resumed) - only the still-undelivered instances.
                try:
                    r2 = _stream_and_send_study(
                        assoc,
                        study_uid,
                        clarus_url,
                        http_timeout,
                        move_jobs,
                        job_id,
                        accepted_contexts_map=accepted_contexts_map,
                        server_ts_list=server_ts_list,
                        sop_registry=sop_registry,
                        tls=tls,
                        spill_threshold=spill_threshold,
                        peer_ae=move_dest_ae,
                        _skip_ids=delivered1,
                        _resumed=True,
                        _report_lost=True,
                        expose_rejected=expose_rejected,
                        qa_url=qa_url,
                    )
                    _c2, _f2, _t2, _s2, _fs2, _ca2, _nc2, _lost2 = r2
                    # Union the delivered sets.
                    sent_sop_uids = list(_s1) + list(_s2)
                    cancelled = cancelled or _ca2
                    no_common_ts = no_common_ts or _nc2
                    resumed_lost = _lost2  # second loss => GIVE_UP below
                except Exception:
                    resumed_lost = True

        if resumed_lost:
            logging.warning(
                "[C-MOVE] Job #%d: gave up after association failure or "
                "failed resume - finishing as a partial job",
                job_id,
            )

        # Reconcile the final counters by UNION of the delivered SOP UIDs.
        # Every instance is either delivered (its SOP uid is in sent_sop_uids)
        # or failed - never double counted, never a sum regression. The
        # authoritative study SOP list (when available) fixes
        # `total` and gives the honest (0008,0058) tail.
        delivered = {u for _c, u in sent_sop_uids if u}
        completed = len(delivered)
        _obs_total_p1 = _t1 if not no_common_ts else 0
        if study_sop_uids is not None and study_sop_uids:
            study_uids_set = set(study_sop_uids)
            total = len(study_uids_set)
            ordered_uids = study_sop_uids
        else:
            # No authoritative list (degradation): total = largest pass total.
            total = max(_obs_total_p1, completed)
            ordered_uids = None
        if study_sop_uids is not None and study_sop_uids:
            failed_sop_uids = [u for u in ordered_uids if u not in delivered]
        else:
            # Degrade to the per-attempt failed sets when present, else []. If
            # a single (non-resumed) pass ran, use its per-instance failed
            # list (exact); otherwise list nothing conclusive (honest).
            failed_sop_uids = sorted(_fs1) if not resumed_lost else []
        # PS3.4 C.4.2.1.4.2: the failure count is the number of instances that
        # were seen and NOT delivered, which is exactly the (0008,0058) list.
        # Deriving it as `total - completed` counted every instance that never
        # reached this pass at all as a failure, so a resumed job reported
        # failures it never observed.
        failed = len(failed_sop_uids)
        # Ensure total never falls below the delivered count.
        total = max(total, completed)

        # 0709: persist the (0008,0058) Failed SOP Instance UID List so the
        # C-MOVE generator can attach it to the final Failed/Warning/Cancel
        # status (PS3.4 C.4.2.1.4.2).
        try:
            move_jobs.set_job_failed_sops(job_id, failed_sop_uids)
        except Exception:
            # Best-effort: the progress/status path must not fail on logging.
            logging.debug("[C-MOVE] set_job_failed_sops failed for job #%d", job_id)

        if no_common_ts:
            # No common transfer syntax: the job ends failed with zero slices
            # delivered, so the C-MOVE generator emits a Failure (0xC000) -
            # not 0xC001 (which was a log-only claim; it is not a response
            # status). The viewer log and the wire now agree.
            logging.warning(
                "[C-MOVE] Job #%d: no common transfer syntax - "
                "job ends failed, 0 slices, C-MOVE-RSP is a Failure (0xC000)",
                job_id,
            )
            move_jobs.finish_job(
                job_id,
                success=False,
                error_message="No common transfer syntax between viewer and server",
            )
            move_jobs_counter.labels(status="failed").inc()
            return

        if cancelled:
            if move_jobs.is_job_cancelled(job_id):
                move_jobs.finish_job(
                    job_id, success=False, error_message="Cancelled by SCU"
                )
            else:
                move_jobs.finish_job(job_id, success=False, error_message="Shutdown")
            move_jobs_counter.labels(status="failed").inc()
            return

        # -- Storage Commitment after successful C-MOVE --
        # Controlled by [dimse] no_storage_commit config flag only.
        # The commit push is attempted only when Storage Commitment was
        # the commit push is attempted ONLY when Storage Commitment was
        # negotiated in the MAIN association (_associate_for_move requests
        # it). Viewers without SC (MicroDicom) burned ACSE timeouts when a
        # separate SC-only association was opened - skip instead of trying.
        if failed == 0 and total > 0:
            if storage_commit_after_move and sent_sop_uids:
                sc_negotiated = any(
                    str(ctx.abstract_syntax) == StorageCommitmentPushModel
                    for ctx in assoc.accepted_contexts
                )
                if not sc_negotiated:
                    logging.info(
                        "[C-MOVE] Job #%d: Storage Commitment not negotiated "
                        "in the main association - skipping SC push",
                        job_id,
                    )
                else:
                    try:
                        _send_storage_commitment(
                            move_dest_ae,
                            target_host,
                            target_port,
                            sent_sop_uids,
                            bridge_ae_title,
                        )
                    except Exception as e:
                        logging.warning(
                            "[C-MOVE] Job #%d: Storage Commitment push failed: %s",
                            job_id,
                            e,
                        )

        # -- Record SCP result  -  the peer is HEALTHY if the association was
        # established and (some or all) slices were delivered. A partial job
        # (failed > 0) is NOT a peer failure: a single bad minute must not
        # evict an alive peer, so only a MAIN-association failure (handled in
        # the RuntimeError catch above) touches the consecutive-failure
        # counter (D3, spec task #1).
        if failed == 0 and total > 0:
            move_jobs.record_scp_success(move_dest_ae, host=target_host)

        # -- Finish job --
        job_success = failed == 0 and total > 0
        move_jobs.finish_job(
            job_id,
            success=job_success,
            error_message=f"{completed} sent, {failed} failed" if failed > 0 else None,
        )
        move_jobs_counter.labels(status="completed" if job_success else "failed").inc()
        logging.info(
            "[C-MOVE] Job #%d finished: %d/%d success, %d failed",
            job_id,
            completed,
            total,
            failed,
        )

        if failed == 0 and total > 0:
            logging.debug("[C-MOVE] Job #%d: all %d instances succeeded", job_id, total)
        elif completed > 0 and failed > 0:
            logging.info(
                "[C-MOVE] Job #%d: partial - %d/%d succeeded, %d failed",
                job_id,
                completed,
                total,
                failed,
            )
        else:
            logging.info("[C-MOVE] Job #%d: no instances delivered", job_id)

    except requests.exceptions.RequestException as e:
        logging.error("[C-MOVE] Download/stream failed for job #%d: %s", job_id, e)
        logging.debug(
            "[C-MOVE] Download/stream traceback for job #%d:", job_id, exc_info=True
        )
        move_jobs.finish_job(
            job_id, success=False, error_message=f"Download failed: {e}"
        )
        move_jobs_counter.labels(status="failed").inc()
    finally:
        _suppress_pynetdicom = False
        _release_or_abort(assoc)
        # Release the peer gate so the next job to this peer can start.
        _job_gate_exit(target_host, target_port, move_dest_ae)


# ---------------------------------------------------------------------------
# Bridge: C-MOVE background worker pool  -  parallel execution
# ---------------------------------------------------------------------------
def _cmove_worker(
    move_jobs,
    clarus_url,  # Clarus Server DICOMweb base URL (single knob, 2908)
    http_timeout,
    bridge_ae_title,
    num_workers=2,
    sop_registry=None,
    storage_commit_after_move=True,
    sop_index=None,
    peers_file=None,
    outbound_network_timeout=60,
    outbound_dimse_timeout=60,
    outbound_acse_timeout=15,
    outbound_assoc_attempts=3,
    tls=None,
    qa_url="",
):
    """Multi-threaded C-MOVE job executor.

    Spawns num_workers threads, each picking one pending job from queue.
    Executes: pre-flight -> stream -> send -> finish.
    Sleeps 5 seconds when queue is empty.
    Checks _shutdown_requested before each iteration.
    """

    def _worker_loop(worker_id):
        """Single worker thread: pick job, execute, repeat."""
        logging.info("[C-MOVE] Worker-%d started", worker_id)

        while not _shutdown_requested.is_set():
            # Bridge: pick next pending job, LIMIT 1
            job = move_jobs.pick_pending_job()
            if job is None:
                # pick_pending_job() already waited via Condition
                continue

            job_id = job["id"]
            study_uid = job["study_uid"]
            move_dest_ae = job["move_dest_ae"]
            attempt = job["attempts"]
            # Queued over the Expose Rejected Instances AE? Read from the job
            # row, not the event: the worker outlives the association.
            job_expose_rejected = bool(job.get("expose_rejected", 0))

            # Bridge: log retry attempts for recovery awareness
            if attempt > 0:
                logging.info(
                    "[C-MOVE] Worker-%d: Job #%d retry attempt %d (recovery)",
                    worker_id,
                    job_id,
                    attempt + 1,
                )

            logging.info(
                "[C-MOVE] Worker-%d picked job #%d: Study=%s -> %s (attempt %d)",
                worker_id,
                job_id,
                study_uid,
                move_dest_ae,
                attempt + 1,
            )

            try:
                # Bridge: execute C-MOVE job
                _execute_cmove_job(
                    job_id,
                    study_uid,
                    move_dest_ae,
                    move_jobs,
                    clarus_url,
                    http_timeout,
                    bridge_ae_title,
                    target_host=job.get("target_host", ""),
                    target_port=job.get("target_port", 0),
                    sop_registry=sop_registry,
                    storage_commit_after_move=storage_commit_after_move,
                    sop_index=sop_index,
                    peers_file=peers_file,
                    outbound_network_timeout=outbound_network_timeout,
                    outbound_dimse_timeout=outbound_dimse_timeout,
                    outbound_acse_timeout=outbound_acse_timeout,
                    outbound_assoc_attempts=outbound_assoc_attempts,
                    tls=tls,
                    expose_rejected=job_expose_rejected,
                    qa_url=qa_url,
                )
            except Exception as e:
                logging.error(
                    "[C-MOVE] Worker-%d: Job #%d failed with exception: %s",
                    worker_id,
                    job_id,
                    e,
                )
                move_jobs.finish_job(job_id, success=False, error_message=str(e))
                move_jobs_counter.labels(status="failed").inc()
            finally:
                # Bridge: clear cancel callback after job completes
                move_jobs.clear_job_callback(job_id)

        logging.info("[C-MOVE] Worker-%d stopped", worker_id)

    logging.info("[C-MOVE] Starting %d worker thread(s)", num_workers)
    workers = []
    for i in range(num_workers):
        t = threading.Thread(
            target=_worker_loop,
            args=(i,),
            name=f"cmove-worker-{i}",
            daemon=True,
        )
        t.start()
        workers.append(t)

    # Bridge: wait for all workers to finish (they stop when _shutdown_requested is set)
    for t in workers:
        t.join()

    logging.info("[C-MOVE] All worker threads stopped")


# ---------------------------------------------------------------------------
# Bridge: periodic cleanup of old C-MOVE jobs
# ---------------------------------------------------------------------------
def _cmove_cleanup_worker(move_jobs, max_age_hours=24):
    """Background thread: delete success/failed jobs older than max_age_hours.

    Runs once per hour. Stops when _shutdown_requested is set.
    """
    max_age_seconds = max_age_hours * 3600
    logging.info(
        "[C-MOVE] Cleanup worker started (max age: %dh)",
        max_age_hours,
    )

    while not _shutdown_requested.is_set():
        # Bridge: sleep 1 hour between cleanup cycles
        _shutdown_requested.wait(timeout=3600.0)
        if _shutdown_requested.is_set():
            break
        move_jobs.cleanup_old_jobs(max_age_seconds)
        logging.debug("[C-MOVE] Old job cleanup completed")

    logging.info("[C-MOVE] Cleanup worker stopped")
