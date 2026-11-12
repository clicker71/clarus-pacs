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

from pydicom.datadict import keyword_dict, keyword_for_tag
from pydicom.dataset import Dataset
from pydicom.multival import MultiValue
from pydicom.tag import BaseTag, Tag

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
def _check_backpressure(pending, max_queue, pending_bytes=0, max_queue_bytes=0):
    """Unified adaptive backpressure with hysteresis.

    Returns DIMSE status:
      <70%  -> 0x0000  (Success, no delay)
      70-90% -> 0x0000 (rate-limit, linear 0-200ms)
      90-95% -> 0xA700  (Out of Resources)
      >=95%  -> 0xC000  (Cannot Perform)

    Args:
        pending: Current queue depth (integer count).
        max_queue: Maximum queue capacity (integer count).
        pending_bytes: Bytes currently held by the queue (0 = not measured).
        max_queue_bytes: Byte budget (0 = no byte limit).

    The fill ratio is the WORSE of the slot ratio and the byte ratio. Slots
    alone let `advanced.stow_queue_max_mb` be a documented knob that nothing
    read: 20000 slots of 0.5-50 MB is up to 10 GB against a configured 500 MB
    budget, and a queue that is 2% full by count can be far past its memory
    budget.
    """
    max_q = max_queue if max_queue > 0 else 5000
    ratio = pending / max_q if max_q > 0 else 0.0
    if max_queue_bytes > 0 and pending_bytes > 0:
        ratio = max(ratio, pending_bytes / max_queue_bytes)

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
            # The byte budget is optional: a queue that cannot report its
            # pending bytes (or has no budget configured) is judged on slots
            # alone, exactly as before.
            _pending_bytes = 0
            try:
                if getattr(self._outbox, "max_queue_bytes", 0):
                    _pending_bytes = self._outbox.pending_bytes()
            except Exception:
                _pending_bytes = 0
            bp = _check_backpressure(
                self._outbox.pending_count(),
                self._outbox.max_queue,
                pending_bytes=_pending_bytes,
                max_queue_bytes=getattr(self._outbox, "max_queue_bytes", 0),
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

    # Every TEXT-valued element is a candidate, not just five named fields.
    # Setting ISO_IR 192 for the whole dataset while repairing only PatientName
    # and friends relabelled the remaining cp1251 bytes (ReferringPhysicianName,
    # PerformingPhysicianName, OperatorsName, comments, ...) as UTF-8, which
    # turns readable Cyrillic into replacement characters downstream. The walk
    # below visits all of them (PS3.5 Table 6.2-1 text VRs).
    _TEXT_VRS = frozenset({"PN", "LO", "SH", "ST", "LT", "UT", "UC", "DA", "TM"})

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

    def _cp1251_fix_one(value):
        """Repair one text value. Returns (new_value, examined).

        `examined` is False when the value could not be tested at all; the
        caller then keeps the dataset's original charset, because relabelling
        UTF-8 over a field nobody checked is exactly the mislabel this repair
        must not create.
        """
        try:
            raw_bytes = str(value).encode("latin-1")
        except (UnicodeEncodeError, UnicodeDecodeError):
            # Not representable byte-for-byte, so it was never cp1251-as-
            # latin-1; it is already something else and is left alone.
            malformed_tags.inc()
            return value, False
        except Exception:
            malformed_tags.inc()
            return value, False

        # R6: a value whose Latin-1 bytes decode STRICTLY as UTF-8 is text
        # that reached us through a missing SpecificCharacterSet: pydicom
        # read the UTF-8 bytes as Latin-1, so `str(value)` looks like
        # mojibake but its byte content is valid UTF-8. Re-decoding those
        # bytes as cp1251 turned "Петров" into permanent garbage and then
        # relabelled the dataset ISO_IR 192. The strict probe runs FIRST and
        # returns on success; only a strict-decode failure falls through to
        # the cp1251 repair (genuine cp1251 Cyrillic is almost never valid
        # UTF-8).
        try:
            return raw_bytes.decode("utf-8"), True
        except UnicodeDecodeError:
            pass

        fixed = raw_bytes.decode("cp1251", errors="replace")

        # Only accept the fix if the result contains >=3 consecutive Cyrillic
        # characters OR is a short all-Cyrillic name (1-4 chars, no long run).
        if (
            fixed != value
            and not _has_cyrillic_run(fixed, 3)
            and not _is_all_cyrillic(fixed)
        ):
            return value, True
        return fixed, True

    fixed_any = False
    # Set when some text element was NOT examined or its repair was uncertain:
    # the dataset then keeps its original charset instead of being relabelled
    # UTF-8 over a mixture of encodings.
    uncertain = False
    # id(element) -> (element, value BEFORE any repair), so a partial repair can
    # be undone whole (`_repair_dataset` writes into `element.value` as it
    # walks, and the decision to keep the original charset is only made at the
    # top).
    #
    # Keyed by the ELEMENT IDENTITY, not by tag: the SAME tag appears in every
    # item of a sequence (two SR Content Items each carry a TextValue, two
    # nested items each carry a CodeMeaning). Keying by tag kept only the
    # first item's original and then wrote it into EVERY element with that
    # tag on undo - silently replacing one item's clinical text with
    # another's.
    #
    # `id()` rather than the element itself: pydicom's DataElement defines no
    # `__hash__`, so it cannot be a dict key. The element is stored beside its
    # original, which also keeps the object alive for the life of the map.
    _uncertain_replaced = {}

    # Built from the walk so a tag added to the dataset later is not silently
    # missed: the five historical names stay FIRST (their log lines are
    # field-specific), then every other text-valued element.
    #
    # PS3.5 SpecificCharacterSet (0008,0005) applies to the WHOLE data set,
    # including every nested Sequence item: (0040,A730) Content Sequence and
    # any other SQ carries text elements in the same source encoding. Relabelling
    # the top level to ISO_IR 192 while repairing only top-level elements left
    # every nested PN/LO/ST as cp1251 bytes declared UTF-8. The walk therefore
    # descends into sequences (`_top_level=False` below).
    def _repair_dataset(ds_obj, top_level):
        """Repair one dataset level. Returns (fixed_any, uncertain)."""
        fixed_here = False
        uncertain_here = False
        _walk_tags = []
        for elem in ds_obj:
            # A sequence ELEMENT is a DataElement whose VR is 'SQ' and whose
            # .value is the Sequence - it is NOT itself a Sequence instance.
            # Testing `isinstance(elem, Sequence)` therefore never matched, so
            # the descent below never ran and nested text was never repaired.
            if elem.VR == "SQ":
                continue
            if elem.VR in _TEXT_VRS or (
                top_level and elem.keyword in fields_to_fix
            ):
                _walk_tags.append(elem.tag)
        _ordered_tags = []
        if top_level:
            for field in fields_to_fix:
                tag = keyword_dict.get(field)
                if tag is not None:
                    _ordered_tags.append(Tag(tag))
        for tag in _walk_tags:
            if tag not in _ordered_tags:
                _ordered_tags.append(tag)

        for _tag in _ordered_tags:
            # `keyword_dict` yields a Tag and the walk yields a Tag: normalise
            # once so the dataset lookup and the element access agree on the
            # key. (`pydicom.tag.Tag` is a FACTORY function, so `isinstance`
            # has to test the concrete `BaseTag` class.)
            tag = _tag if isinstance(_tag, BaseTag) else Tag(_tag)
            # The keyword only drives the LOG wording; an unknown/private tag
            # has none and falls into the redacted branch.
            field = keyword_for_tag(tag)
            if tag not in ds_obj:
                continue
            element = ds_obj[tag]
            if element is None:
                continue
            if element.VR not in _TEXT_VRS:
                # A keyword listed above whose VR is not text (PatientID is
                # LO, so this is only reachable for a non-conformant file):
                # leave it and mark the walk partial.
                uncertain_here = True
                continue
            value = element.value
            if isinstance(value, (list, tuple, MultiValue)):
                # A multi-valued text element is repaired value by value; if
                # any component cannot be examined the element is uncertain.
                new_values = []
                for part in value:
                    fixed_part, ok = _cp1251_fix_one(part)
                    if not ok:
                        uncertain_here = True
                    new_values.append(fixed_part)
                    if fixed_part != part:
                        fixed_here = True
                        if id(element) not in _uncertain_replaced:
                            _uncertain_replaced[id(element)] = (element, list(value))
                element.value = new_values
                continue
            if not isinstance(value, (str, bytes)):
                try:
                    value = str(value)
                except Exception:
                    uncertain_here = True
                    continue
            if not value:
                continue
            fixed, ok = _cp1251_fix_one(value)
            if not ok:
                uncertain_here = True
                continue
            if fixed != value:
                if id(element) not in _uncertain_replaced:
                    _uncertain_replaced[id(element)] = (element, value)
                element.value = fixed
                fixed_here = True
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
                    # Free text a site fills with names and addresses, and on a
                    # cp1251-fixed instance exactly as identifying as
                    # PatientName. Neither masker applies (not a PN, not a keyed
                    # ID), so the value is NOT logged at all: the tag and the
                    # length are enough to confirm the repair happened.
                    logging.debug(
                        "  [CP1251] Fixed %s: value redacted (%d -> %d chars)",
                        field,
                        len(str(value)),
                        len(str(fixed)),
                    )

        # Descend into every sequence at this level, including sequences
        # nested inside items of other sequences.
        for elem in ds_obj:
            if elem.VR != "SQ":
                continue
            for item in elem.value or []:
                if not isinstance(item, Dataset):
                    # A non-Dataset item cannot carry tagged text elements;
                    # it is not an encoding question, so it is not "uncertain".
                    continue
                sub_fixed, sub_uncertain = _repair_dataset(item, False)
                fixed_here = fixed_here or sub_fixed
                uncertain_here = uncertain_here or sub_uncertain
        return fixed_here, uncertain_here

    fixed_any, uncertain = _repair_dataset(ds, True)
    fixed_any = bool(fixed_any)
    uncertain = bool(uncertain)

    if fixed_any:
        if uncertain:
            # Some text element could not be examined, so the dataset is not
            # uniformly repairable. Relabelling UTF-8 would declare a charset
            # the unexamined element does not honour; but KEEPING the repaired
            # values under the original charset is just as wrong in the other
            # direction - PS3.5 (0008,0005) applies to the whole data set, so a
            # Cyrillic string left under ISO_IR 100 is written out as Latin-1
            # bytes (or raises) and the text is corrupted on the wire. Neither
            # half-label is acceptable, so the whole repair is undone: value
            # and charset stay exactly as received, mutually consistent.
            #
            # The recorded ELEMENTS are restored directly, so each one gets its
            # OWN original back and no walking is needed (nor would a walk
            # help: `ds[tag]` reaches only the top level, and matching by tag
            # is exactly the swap this undo must not make).
            for _element, _orig in list(_uncertain_replaced.values()):
                try:
                    _element.value = _orig
                except Exception:
                    pass
            # Nothing was changed after the undo, so the caller must not treat
            # this instance as repaired (the metric below records the no-op).
            fixed_any = False
            logging.warning(
                "  [CP1251] Repaired text elements but could not examine every "
                "text value - reverting the repair so the charset and the "
                "bytes stay consistent",
            )
            charset_conversions.labels(from_encoding="latin1").inc()
        else:
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
    bridge_ae_title="CLBRIDGE",
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
    # R17: bind the semaphore to a LOCAL before the acquire and release that
    # same object in the finally. A reloaded [advanced] max_concurrent_cstore
    # replaces the module global, and a handler still holding the OLD
    # semaphore then released the NEW one - raising its counter above the
    # configured limit for the rest of the process.
    sem = _cstore_semaphore
    if not sem.acquire(blocking=False):
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
            # SOPInstanceUID (0008,0018) is Type 1 for every Storage SOP
            # Class (PS3.4 B.2 / PS3.3 C.12.1.1.2). Enqueueing an instance
            # without it produced an empty outbox key: the row read as a
            # perpetual "duplicate", the blob was never persisted, and the
            # modality still got 0x0000 for an instance nobody stored.
            # 0xA900 "Data Set Does Not Match SOP Class" (PS3.4 Table B.2-1)
            # is the honest refusal.
            logging.warning("[C-STORE] Missing SOPInstanceUID - rejecting 0xA900")
            return 0xA900

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
        # The tracker is told the modality only: it summarises per-study
        # counts, and PatientID has no reader there.
        is_new_study = record_cstore(assoc_id, study_uid, modality)
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
                    # The log needs a readable placeholder for an empty old
                    # value; the audit tuple that travelled with this line
                    # carries the raw empty string, never "(empty)".
                    logging.debug(
                        "Coerced %s: '%s' -> '%s' (audit trail deferred to worker)",
                        tag,
                        old or "(empty)",
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
                    #
                    # The drain worker can claim the item (pending/ ->
                    # in_progress/) between the enqueue returning and this
                    # hook running, so pending/ alone is not where the blob has
                    # to be. A hardlink to either name is safe: mark_delivered
                    # only unlinks the name the PRIMARY owns.
                    primary_blob = outbox._dcm_path(outbox._pending, sop_uid)
                    if not os.path.isfile(primary_blob):
                        primary_blob = outbox._dcm_path(outbox._in_progress, sop_uid)
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
        sem.release()


# ---------------------------------------------------------------------------
# Bridge: C-ECHO handler
# ---------------------------------------------------------------------------
def handle_c_echo(event):
    """Handle C-ECHO (Verification)."""
    return 0x0000  # Success
