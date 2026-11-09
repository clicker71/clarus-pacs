# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  C-STORE and C-ECHO handlers.

handle_c_store: C-STORE -> STOW-RS forwarding with size validation,
cp1251 charset fix, and SOP indexing.
handle_c_echo: Verification.
"""

import logging
import os
import struct
import threading
import time

from clean_code.assoc_tracker import record_cstore
from clean_code.http_helpers import stow_single_instance
from clean_code.metrics import (
    ae_label,
    bytes_total,
    charset_conversions,
    cstore_requests,
    malformed_tags,
    translation_duration,
)
from clean_code.protocol import _dimse_status_name
from clean_code.registry import _is_sop_accepted
from clean_code.utils import (
    _measure_dataset_bytes,
    _shutdown_requested,
    mask_patient_id,
    mask_patient_name,
)
from clean_code.validation import sanitize_dataset

# ---------------------------------------------------------------------------
# Backpressure: limit concurrent C-STORE handlers (like nginx worker_connections)
# ---------------------------------------------------------------------------
# Prevents pynetdicom thread pool exhaustion under extreme load.
# When all slots are busy, new associations get DIMSE 0xA700 (Out of Resources)
#  -  modalities are REQUIRED by DICOM standard to retry.
# Default: max(4, cpu_count). Configurable via [advanced] max_concurrent_cstore.
_MAX_CONCURRENT_CSTORE = max(4, (os.cpu_count() or 2))
_cstore_semaphore = threading.Semaphore(_MAX_CONCURRENT_CSTORE)


def set_max_concurrent_cstore(n: int) -> None:
    """Set max concurrent C-STORE handlers (called from config parsing)."""
    global _MAX_CONCURRENT_CSTORE, _cstore_semaphore
    _MAX_CONCURRENT_CSTORE = n
    _cstore_semaphore = threading.Semaphore(n)


# ---------------------------------------------------------------------------
# PROD-HARDENING: Unified backpressure  -  single function for outbox
# ---------------------------------------------------------------------------
def _check_backpressure(pending, max_queue):
    """Unified adaptive backpressure with hysteresis.

    Returns DIMSE status:
      <70%  -> 0x0000  (Success, no delay)
      70-90% -> 0x0000 (rate-limit, linear 0-200ms)
      90-95% -> 0xA700  (Out of Resources)
      >=95%  -> 0xC000  (Cannot Perform)

    Args:
        pending: Current queue depth (integer count).
        max_queue: Maximum queue capacity (integer count).
    """
    max_q = max_queue if max_queue > 0 else 5000
    ratio = pending / max_q if max_q > 0 else 0.0

    if ratio >= 0.95:
        logging.critical(
            "[BACKPRESSURE] Queue at %d/%d (%.0f%%) - CRITICAL, returning 0xC000",
            pending,
            max_q,
            ratio * 100,
        )
        return 0xC000  # Cannot Perform
    elif ratio >= 0.90:
        logging.error(
            "[BACKPRESSURE] Queue at %d/%d (%.0f%%) - returning 0xA700",
            pending,
            max_q,
            ratio * 100,
        )
        return 0xA700  # Out of Resources
    elif ratio >= 0.70:
        # Soft rate-limit: linear delay 0-200ms between 70% and 90%
        delay = 0.2 * (ratio - 0.70) / (0.90 - 0.70)
        time.sleep(delay)
        logging.warning(
            "[BACKPRESSURE] Queue at %d/%d (%.0f%%) - rate-limit delay %.0fms",
            pending,
            max_q,
            ratio * 100,
            delay * 1000,
        )
    # <70%  -  no delay, no reject

    return 0x0000  # Success


# ---------------------------------------------------------------------------
# Unified pre-stow admission gate
# ---------------------------------------------------------------------------
class _AdmissionController:
    """Single admission decision for C-STORE (health + backpressure).

    Replaces the two independent checks (health_checker + _check_backpressure)
    with one pre-flight gate. Priorities:
      1. Server dead >= 30s  -> PARK (0x0000) when store_forward=on, else 0xC000.
      2. degraded  (< 30s)   -> 0xA700 (both modes; hysteresis)
      3. backpressure        -> 0xC000 (>=95%) / 0xA700 (90-95%) /
                                rate-limit (70-90%), else 0x0000.
         (>=95% disk limit overrides parking ALWAYS.)

    store_forward: on -> a dead>=30s C-STORE is parked (0x0000; blob already
    atomically on disk before ACK), off -> 0xC000 (Cannot Perform, not transient).

    One place decides "can we take a new C-STORE". Mode
    (never/probe-disabled) handling is preserved via HealthChecker.
    """

    DEAD_SECONDS_FATAL = 30.0

    def __init__(self, outbox=None, health_checker=None, store_forward=False):
        self._outbox = outbox
        self._health = health_checker
        self._store_forward = store_forward

    def check(self):
        """Return (dimse_status, reason). 0x0000 = admit."""
        if self._health is not None:
            healthy, status, reason = self._health.check_before_stow()
            if not healthy:
                dead = self._health.dead_seconds()
                if dead >= self.DEAD_SECONDS_FATAL:
                    if self._store_forward:
                        logging.warning(
                            "[ADMISSION] Server dead %.0fs, store_forward=on "
                            "- parked (0x0000; blob on disk)",
                            dead,
                        )
                        # Parking: accept; backpressure below may still reject.
                    else:
                        logging.error(
                            "[ADMISSION] Server dead for %.0fs -> 0xC000 "
                            "(Cannot Perform)",
                            dead,
                        )
                        return 0xC000, reason or "server dead"
                else:
                    logging.warning(
                        "[ADMISSION] Server degraded for %.0fs -> 0xA700",
                        dead,
                    )
                    return status, reason or "server degraded"

        if self._outbox is not None:
            bp = _check_backpressure(
                self._outbox.pending_count(),
                self._outbox.max_queue,
            )
            if bp != 0x0000:
                return bp, "queue backpressure"

        return 0x0000, ""


# ---------------------------------------------------------------------------
# Bridge: cp1251 mojibake fixer for old Russian X-ray machines
# ---------------------------------------------------------------------------

# Module-level Cyrillic character set for mojibake detection (DRY-001).
# Defined once at import time, not re-created per C-STORE call.
# Using frozenset  -  immutable + hashable, no runtime cost on each call.
_CYRILLIC_BLOCK = frozenset(
    "АБВГДЕЖЗИЙКЛМНОПРСТУФХЦЧШЩЪЫЬЭЮЯ" "абвгдежзийклмнопрстуфхцчшщъыьэюяё№"
)


def _fix_cp1251_krakozyabry(ds):
    """Detect and fix cp1251 mojibake in DICOM text fields.

    Problem: Old Russian X-ray machines encode PatientName/StudyDescription
    in Windows-1251 (cp1251) but either omit SpecificCharacterSet (0008,0005)
    or set it to ISO_IR 100 (Latin-1). pydicom then decodes the bytes as
    Latin-1, producing garbled text like "ÐŸÐµÑ‚Ñ€Ð¾Ð²" instead of "Петров".

    This function reverses the incorrect Latin-1 -> bytes step and re-decodes
    as cp1251. If the field was already valid UTF-8, it is left untouched.

    Strategy:
    1. Check if SpecificCharacterSet is missing or ISO_IR 100.
    2. For each target field: encode back to Latin-1 bytes -> decode as cp1251.
    3. If the result differs from original -> replace.
    4. Set SpecificCharacterSet to ISO_IR 192 (UTF-8) for Clarus Server.
    """
    charset = getattr(ds, "SpecificCharacterSet", None)
    if charset is not None:
        cs = str(charset).strip()
        if cs and cs not in ("", "ISO_IR 100", "ISO 2022 IR 100"):
            charset_conversions.labels(from_encoding="utf8").inc()
            return ds

    fields_to_fix = [
        "PatientName",
        "PatientID",
        "InstitutionName",
        "StudyDescription",
        "SeriesDescription",
    ]

    # fix v2: heuristic to distinguish cp1251 mojibake from valid Latin-1.

    def _has_cyrillic_run(text, min_run=3):
        """Return True if text contains >=min_run consecutive Cyrillic chars."""
        run = 0
        for ch in text:
            if ch in _CYRILLIC_BLOCK:
                run += 1
                if run >= min_run:
                    return True
            else:
                run = 0
        return False

    # Short-name exception:
    # Korean surnames transcribed in Cyrillic (for example the 2-3 character
    # transliterations of Ho, Yu, Lee and Kim) are 1-4 chars, all Cyrillic, but
    # never form a >=3 run in isolation. Without this exception a radiology
    # department whose staff include Korean-Russian colleagues would see those
    # names garbled in the viewer.
    def _is_all_cyrillic(text):
        return (
            len(text) >= 1
            and len(text) <= 4
            and all(ch in _CYRILLIC_BLOCK for ch in text)
        )

    fixed_any = False
    for field in fields_to_fix:
        if field not in ds:
            continue
        element = ds.data_element(field)
        if element is None:
            continue
        value = element.value
        if not isinstance(value, (str, bytes)):
            try:
                value = str(value)
            except Exception:
                continue
        if not value:
            continue

        try:
            raw_bytes = value.encode("latin-1")
            fixed = raw_bytes.decode("cp1251", errors="replace")
        except (UnicodeEncodeError, UnicodeDecodeError, Exception):
            malformed_tags.inc()
            continue

        # Only accept the fix if the result contains >=3 consecutive Cyrillic
        # characters OR is a short all-Cyrillic name (1-4 chars, no long run).
        if (
            fixed != value
            and not _has_cyrillic_run(fixed, 3)
            and not _is_all_cyrillic(fixed)
        ):
            continue

        if fixed != value:
            element.value = fixed
            fixed_any = True
            if field == "PatientName":
                logging.debug(
                    "  [CP1251] Fixed %s: %s -> %s",
                    field,
                    mask_patient_name(str(value)),
                    mask_patient_name(str(fixed)),
                )
            elif field == "PatientID":
                # A modality's PatientID is the real identifier, not the
                # bridge's AUTOPAT_ placeholder: it goes through the keyed
                # token like every other log line that carries one.
                logging.debug(
                    "  [CP1251] Fixed %s: %s -> %s",
                    field,
                    mask_patient_id(str(value)),
                    mask_patient_id(str(fixed)),
                )
            else:
                logging.debug(
                    "  [CP1251] Fixed %s: %.40s → %.40s",
                    field,
                    str(value)[:40],
                    str(fixed)[:40],
                )

    if fixed_any:
        ds.SpecificCharacterSet = "ISO_IR 192"
        charset_conversions.labels(from_encoding="cp1251").inc()
        logging.info(
            "  [CP1251] Detected and fixed cp1251 mojibake in %s",
            mask_patient_name(str(ds.get("PatientName", "unknown"))),
        )
    else:
        charset_conversions.labels(from_encoding="latin1").inc()

    return ds


# ---------------------------------------------------------------------------
# Bridge: C-STORE handler with size limit
# ---------------------------------------------------------------------------
def handle_c_store(
    event,
    clarus_url,
    verbose,
    sop_index=None,
    max_store_bytes=0,
    http_timeout=30,
    sop_registry=None,
    outbox=None,
    health_checker=None,
    store_forward=False,
    fix_empty_patient_id=True,
    fix_empty_patient_name=True,
    bridge_id_prefix="AUTOPAT_",
    bridge_name_prefix="Anonymous^",
    tls=None,
    forward_outbox=None,
    bridge_ae_title="CLARUS_BRIDGE",
):
    """Handle C-STORE: validate, enqueue to OutboxQueue, return immediately.

    OutboxQueue guarantees eventual delivery. C-STORE returns 0x0000 Success
    as soon as the SOP is persisted in SQLite  -  the modality is free immediately.
    Background outbox workers drain the queue via STOW-RS.

    Args:
        outbox: OutboxQueue instance (required for reliable persistence).
                Falls back to sync STOW-RS if None (demo/testing mode).
        store_forward: when on, a dead>=30s server parks the C-STORE
                (0x0000) instead of 0xC000.
    """
    # Bridge: reject new requests during shutdown
    if _shutdown_requested.is_set():
        return 0xC000  # Processing Failure

    # Backpressure: if all slots busy -> 0xA700 Out of Resources
    # Modality will retry  -  this is standard DICOM behavior (PS3.7 sec.C.4).
    # Like nginx worker_connections: accept only what you can process.
    if not _cstore_semaphore.acquire(blocking=False):
        logging.warning(
            "[C-STORE] Backpressure: all %d slots busy - rejecting with 0xA700",
            _MAX_CONCURRENT_CSTORE,
        )
        return 0xA700  # Out of Resources

    try:
        # Bridge: C-STORE counter
        cstore_requests.inc()
        start_time = time.monotonic()

        # Bridge: check SOP class acceptance via registry
        if sop_registry is not None:
            context_uid = event.context.abstract_syntax
            if not _is_sop_accepted(sop_registry, context_uid):
                logging.warning(
                    "[C-STORE] Rejected: SOP class %s not in registry or accept=false",
                    context_uid,
                )
                # 0x0122 "SOP Class not supported" (PS3.7 Annex C) - 0xA801 is
                # a C-MOVE code and does NOT exist in the C-STORE status table
                # (PS3.4 Table B.2-1 lists A7xx/A9xx/Cxxx/B000/B006/B007/0000).
                return 0x0122

        # pydicom may fail to parse corrupted dataset (e.g. broken
        # sequences from Explicit->Implicit VR conversion of non-pixel objects).
        # Don't kill the association  -  return C000 and let other slices through.
        try:
            ds = event.dataset
        except (OSError, struct.error) as e:
            logging.warning("[BUG-005] C-STORE parse error, returning C000: %s", e)
            return 0xC000

        # Attach the File Meta Information BEFORE any
        # re-serialization. event.file_meta carries the NEGOTIATED transfer
        # syntax of this C-STORE. Without it, _ensure_file_meta defaults to
        # ExplicitVRLittleEndian and a 4.70 instance is silently rewritten as
        # EVRLE with its encapsulated pixel stream intact - a corrupt hybrid
        # (found by the cstore_e2e test).
        try:
            ds.file_meta = event.file_meta
        except Exception as fm_err:
            logging.warning("[C-STORE] Cannot attach file_meta: %s", fm_err)

        sop_uid = str(getattr(ds, "SOPInstanceUID", ""))
        study_uid = str(getattr(ds, "StudyInstanceUID", ""))
        patient = str(getattr(ds, "PatientName", "unknown"))

        # Bridge: detect malformed tags  -  missing SOPInstanceUID
        if not sop_uid:
            malformed_tags.inc()

        # Bridge: C-STORE size limit guard. The size is measured in both
        # branches: with a limit it gates the store, without one it feeds the
        # rx byte counter.
        dataset_size = _measure_dataset_bytes(ds, event)
        if max_store_bytes > 0 and dataset_size > max_store_bytes:
            logging.warning(
                "[C-STORE] Rejected oversized dataset: %d bytes "
                "(limit: %d bytes) from %s",
                dataset_size,
                max_store_bytes,
                mask_patient_name(patient),
            )
            return 0xA700  # Out of Resources  -  Refused: Out of Resources

        # rx bytes: the accepted payload size, counted before persistence so a
        # later outbox failure cannot hide received traffic.
        if dataset_size > 0:
            try:
                bytes_total.labels(
                    direction="rx", peer=ae_label(event.assoc.requestor.ae_title)
                ).inc(dataset_size)
            except Exception:
                pass

        logging.debug(
            "[C-STORE] Received: %s (SOP: %s)", mask_patient_name(patient), sop_uid
        )
        logging.debug(
            "[C-STORE] %s SOP=%s StudyUID=%s Modality=%s",
            mask_patient_name(patient),
            sop_uid[:40] if sop_uid else "(none)",
            study_uid[:50] if study_uid else "(none)",
            str(getattr(ds, "Modality", "?")),
        )

        # Track C-STORE per association for INFO-level logging.
        # Admin sees: "CT-2 connected, receiving study 1.2.840... (CT, 437 instances)".
        # One line per new study, not per SOP.
        try:
            assoc_id = id(event.assoc)  # Python object ID  -  unique per connection
        except Exception:
            assoc_id = 0
        patient_id_raw = str(getattr(ds, "PatientID", "?"))
        modality = str(getattr(ds, "Modality", "??"))
        is_new_study = record_cstore(assoc_id, study_uid, patient_id_raw, modality)
        if is_new_study:
            logging.info(
                "[C-STORE] Receiving: study=%s patient=%s modality=%s",
                study_uid[:50] if study_uid else "?",
                mask_patient_id(patient_id_raw) if patient_id_raw else "?",
                modality,
            )

        # Unified pre-stow admission gate (health + backpressure).
        # Rejects early (: don't do heavy work if we won't accept).
        adm_status, adm_reason = _AdmissionController(
            outbox=outbox,
            health_checker=health_checker,
            store_forward=store_forward,
        ).check()
        if adm_status != 0x0000:
            logging.warning(
                "[C-STORE] Admission denied (%s): rejecting with 0x%04X",
                adm_reason or "?",
                adm_status,
            )
            return adm_status

        # Bridge: fix cp1251 mojibake before forwarding to Clarus Server
        ds = _fix_cp1251_krakozyabry(ds)

        # Sanitize empty DICOM Type 2 fields before outbox
        # Bridge fixes what it can, server validates independently  -  two layers of protection.
        # write_audit=False: audit trail (0400,0561) is deferred to outbox worker
        # to keep the C-STORE hot-path fast. Modifications metadata is passed
        # alongside the blob and applied by the worker before STOW-RS.
        # study_uid: deterministic PatientID from StudyInstanceUID.
        # All SOPs in the same study get the SAME PatientID  -  preserves DICOM
        # patient-study relationship and prevents SQLite single-patient overflow.
        try:
            mods = sanitize_dataset(
                ds,
                fix_patient_id=fix_empty_patient_id,
                fix_patient_name=fix_empty_patient_name,
                id_prefix=bridge_id_prefix,
                name_prefix=bridge_name_prefix,
                write_audit=False,
                study_uid=study_uid,
                modifying_system=bridge_ae_title,
            )
            if mods:
                for tag, old, new in mods:
                    # Mask PatientName in logs (Russian personal-data law 152-FZ); show
                    # PatientID as-is (it is generated, not PHI).
                    display_new = (
                        mask_patient_name(new) if tag == (0x0010, 0x0010) else new
                    )
                    logging.debug(
                        "Coerced %s: '%s' -> '%s' (audit trail deferred to worker)",
                        tag,
                        old,
                        display_new,
                    )
        except ValueError as e:
            logging.error("Validation failed: %s - returning 0xC000", e)
            return 0xC000

        # "Don't do heavy work on the hot path. Accept bytes -> return
        # Success -> done. Everything else happens in the background."

        # Bridge: async STOW-RS via OutboxQueue  -  reliable persistence + best-effort delivery
        # (backpressure is already enforced by the unified _AdmissionController above)
        if outbox is not None:
            # Outbox-based: persist to SQLite -> immediate 0x0000 Success
            ok = outbox.enqueue(ds, modifications=mods if mods else None)
            if ok == "duplicate":
                # Idempotent  -  SOP already in outbox, return SUCCESS
                logging.info(
                    "[C-STORE] SOP %s already in outbox (duplicate) - 0x0000 Success",
                    sop_uid,
                )
                return 0x0000  # Success  -  idempotent, already persisted
            if not ok:
                logging.error(
                    "[C-STORE] Outbox enqueue failed for %s (SOP: %s) - returning 0xA700",
                    mask_patient_name(patient),
                    sop_uid,
                )
                return 0xA700  # Out of Resources  -  transient, sender should retry

            status = 0x0000  # Success  -  SOP persisted in outbox, will be delivered

            # -- 0409 forwarder hook ----------------------------------
            # Best-effort DIMSE->DIMSE mirror to a SECOND (non-authoritative)
            # archive. Called ONLY after the PRIMARY enqueue returned True
            # (so a failed primary enqueue never touches the forward channel).
            # The forward channel can NEVER change `status`: whatever happens
            # here the modality already holds 0x0000 from the primary persist.
            if forward_outbox is not None:
                try:
                    # Blob is already atomically on disk in the primary outbox;
                    # hardlink (EXDEV fallback copy) it into forward/pending.
                    primary_blob = outbox._dcm_path(outbox._pending, sop_uid)
                    _ = forward_outbox.enqueue_from_blob(
                        primary_blob,
                        sop_uid,
                        modifications=mods if mods else None,
                    )
                    # Result is intentionally ignored: forward is best-effort.
                    # Dropped/full/failure are logged + counted inside
                    # enqueue_from_blob; the modality status stays 0x0000.
                except Exception as fwd_err:  # defensive: never touch status
                    logging.warning(
                        "[C-STORE] Forward enqueue raised %s for %s "
                        "(0x0000 unchanged)",
                        type(fwd_err).__name__,
                        sop_uid,
                    )
                    if hasattr(forward_outbox, "_inc_forward_dropped"):
                        forward_outbox._inc_forward_dropped()
        else:
            # Fallback: sync forwarding (no outbox, no legacy queue)
            logging.warning(
                "[C-STORE] No queue configured - falling back to sync forward"
            )
            status, http_status, reason = stow_single_instance(
                ds,
                clarus_url,
                http_timeout,
                tls=tls,
            )
            if status != 0x0000:
                logging.warning(
                    "[C-STORE] Rejected %s (SOP: %s): %s - %s",
                    mask_patient_name(patient),
                    sop_uid,
                    _dimse_status_name(status),
                    reason,
                )

        # Bridge: translation duration observation
        translation_duration.observe(time.monotonic() - start_time)

        return status
    finally:
        _cstore_semaphore.release()


# ---------------------------------------------------------------------------
# Bridge: C-ECHO handler
# ---------------------------------------------------------------------------
def handle_c_echo(event):
    """Handle C-ECHO (Verification)."""
    return 0x0000  # Success
