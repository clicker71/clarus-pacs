# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""DIMSE Association Tracker  -  nginx-way.

Tracks active DICOM associations: AE title, IP, studies received, instance counts.
Used by C-STORE handler and association event handlers to provide
administrator-visible INFO logs without per-SOP spam.

Thread-safe via threading.Lock. Cleanup on association release.
"""

import logging
import threading

from clean_code.metrics import ae_label, associations_total

# -- Association info ----------------------------------------------------


class AssocInfo:
    """Per-association tracking state."""

    __slots__ = ("address", "ae_title", "instance_count", "studies")

    def __init__(self, ae_title: str, address: str):
        self.ae_title = ae_title
        self.address = address
        # studies: dict[study_uid -> {'modality': str, 'count': int}]
        # No PatientID here: the summary this feeds only prints modality and
        # counts, so a raw identifier would be PHI held in memory for nothing.
        self.studies: dict[str, dict] = {}
        self.instance_count = 0


# Global registry: association_id -> AssocInfo
_assoc_registry: dict[int, AssocInfo] = {}
_assoc_lock = threading.Lock()


# -- Public API ----------------------------------------------------------


def register_assoc_accepted(assoc_id: int, ae_title: str, address: str) -> None:
    """Register a new DICOM association. Logs INFO on first connection.

    Called from evt.EVT_ACCEPTED handler.
    """
    with _assoc_lock:
        if assoc_id in _assoc_registry:
            return  # Already registered (should not happen)
        _assoc_registry[assoc_id] = AssocInfo(ae_title, address)

    # Counter is bumped outside the lock: prometheus_client objects are
    # already thread-safe and this is not on the C-STORE hot path.
    try:
        associations_total.labels(role="from", ae=ae_label(ae_title)).inc()
    except Exception:
        pass

    # : log outside lock to avoid holding it during I/O.
    # Mask AE title if it looks like a hostname (long, noisy).
    ae_display = ae_title if len(ae_title) <= 16 else ae_title[:16]
    logging.info(
        "[DIMSE] Association accepted: AE=%s from %s",
        ae_display,
        address,
    )


def register_assoc_released(assoc_id: int) -> None:
    """Release association and log summary.

    Called from evt.EVT_RELEASED or evt.EVT_ABORTED handler.
    """
    with _assoc_lock:
        info = _assoc_registry.pop(assoc_id, None)

    if info is None:
        return

    studies_count = len(info.studies)
    instances = info.instance_count
    ae = info.ae_title[:16] if len(info.ae_title) > 16 else info.ae_title

    if studies_count == 0:
        # NOTE: fires for EVERY association, including C-MOVE/C-FIND ones
        # that never carry C-STORE. "(no instances received)" is accurate
        # for all operation types (field: "no C-STORE received" read like
        # the bridge expected a C-STORE on a C-MOVE association).
        logging.info(
            "[DIMSE] Association released: AE=%s (no instances received)",
            ae,
        )
        return

    # Build study summary: "CTx432, SRx2" (ASCII only  -  cp1251-safe)
    study_parts = []
    for study_uid, sinfo in info.studies.items():
        mod = sinfo.get("modality", "??")
        cnt = sinfo.get("count", 0)
        study_parts.append(f"{mod}x{cnt}")

    logging.info(
        "[DIMSE] Association released: AE=%s studies=%d instances=%d [%s]",
        ae,
        studies_count,
        instances,
        ", ".join(study_parts),
    )


def record_cstore(assoc_id: int, study_uid: str, modality: str) -> bool:
    """Record a C-STORE instance in the association tracker.

    Returns True if this is the FIRST instance for this study_uid
    (caller should log INFO about new study).
    """
    if not study_uid:
        return False

    with _assoc_lock:
        info = _assoc_registry.get(assoc_id)
        if info is None:
            return False  # Association not tracked (should not happen)

        info.instance_count += 1

        if study_uid not in info.studies:
            info.studies[study_uid] = {
                "modality": modality or "??",
                "count": 1,
            }
            return True  # First instance of this study
        else:
            info.studies[study_uid]["count"] += 1
            return False  # Already tracking this study
