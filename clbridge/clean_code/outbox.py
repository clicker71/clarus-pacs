# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""Outbox Queue  -  filesystem-backed reliable DICOM persistence + delivery.

nginx-way: directories = queue state. Atomic os.rename = state transition.
No SQLite. No WAL. No busy_timeout. No _write_lock.

Layout:
    outbox/
      pending/       <- items waiting for first attempt or retry
        {uid}.dcm    <- DICOM blob (atomic write: .tmp -> os.replace)
        {uid}.meta   <- {"created_at": ..., "retry_count": 0, "next_retry_at": ..., "last_error": "..."}
        {uid}.mods   <- audit trail sidecar (FR-001), optional
      in_progress/   <- atomic claim: os.rename from pending/
      done/          <- only when cleanup=False (retain for audit)
      undelivered/   <- max retries exhausted (manual intervention)

Guarantees:
  - Atomic claim: os.rename(pending/x.*, in_progress/x.*)  -  only ONE worker succeeds
  - Crash-safe: recover_stuck() moves in_progress/* back to pending/ on startup
  - Idempotent: enqueue checks if {uid}.dcm already exists in any dir -> skip
  - No duplicate delivery: exactly one in_progress/ at a time

Thread safety:
  - enqueue(): concurrent-safe via atomic os.replace (no shared state)
  - dequeue(): os.rename is atomic on same filesystem  -  collision -> skip, try next
  - Single worker (KISS)  -  no multi-worker contention in practice
"""

import json
import logging
import os
import shutil
import threading
import time

from pydicom import dcmread, dcmwrite
from pydicom.dataset import FileMetaDataset
from pydicom.uid import ExplicitVRLittleEndian

from clean_code.metrics import (
    outbox_enqueue_latency_seconds,
    outbox_pending,
)

# How often the worker checks for stuck items (crashed during delivery).
STUCK_ITEM_CHECK_INTERVAL = 60.0  # seconds

# Items stuck in in_progress/ longer than this -> recovered.
STUCK_ITEM_TIMEOUT = 300.0  # 5 minutes


def _ensure_file_meta(dataset):
    """Ensure dataset has a valid FileMetaDataset with TransferSyntaxUID.

    ALWAYS moves stray (0002,eeee) group elements out of the DIMSE dataset
    body into file_meta - pydicom rejects a write that leaves (0002,eeee)
    inline ("must be in a FileMetaDataset instance", Weasis C-STORE 0xA700).
    A tag already present in file_meta is NOT overwritten, so the negotiated
    TransferSyntaxUID from event.file_meta (C-STORE path sets ds.file_meta)
    always wins. Preserves upstream TS when present (cltranscode inserts
    (0002,0010) into DIMSE datasets). Falls back to ExplicitVRLittleEndian.
    """
    if (
        hasattr(dataset, "file_meta")
        and isinstance(dataset.file_meta, FileMetaDataset)
        and dataset.file_meta
    ):
        file_meta = dataset.file_meta
    else:
        file_meta = FileMetaDataset()
        dataset.file_meta = file_meta

    group_0002_tags = [tag for tag in dataset._dict if (tag >> 16) == 0x0002]
    for tag in group_0002_tags:
        elem = dataset[tag]
        if tag not in file_meta:
            file_meta.add(elem)
        del dataset[tag]

    if hasattr(file_meta, "TransferSyntaxUID") and file_meta.TransferSyntaxUID:
        return
    # A dataset without TS info is suspicious -
    # say it loud instead of silently corrupting compressed pixel data.
    logging.warning(
        "[OUTBOX] SOP %s has no TransferSyntaxUID - defaulting to "
        "ExplicitVRLittleEndian (compressed instances MUST pass the "
        "negotiated TS via event.file_meta)",
        str(getattr(dataset, "SOPInstanceUID", "?")),
    )
    file_meta.TransferSyntaxUID = ExplicitVRLittleEndian


class OutboxQueue:
    """Filesystem-backed outbox  -  directories + atomic rename = queue."""

    def __init__(
        self,
        outbox_dir,
        max_queue=20000,
        max_retries=10,
        cleanup=True,
        retry_interval=0,
        pending_gauge=None,
        undelivered_total=None,
    ):
        self._dir = outbox_dir
        self.max_queue = max_queue
        self.MAX_RETRIES = max_retries
        self.cleanup = cleanup
        self.retry_interval = retry_interval

        # 0409 forwarder: this OutboxQueue backs BOTH the primary STOW drain
        # and (as a second instance) the best-effort forward channel. The
        # pending gauge tells the metric layer WHICH queue is reporting, so the
        # two never clobber each other. Default None = primary (outbox_pending).
        self._pending_gauge = (
            pending_gauge if pending_gauge is not None else outbox_pending
        )
        self._undelivered_total = undelivered_total

        # Subdirectories
        self._pending = os.path.join(self._dir, "pending")
        self._in_progress = os.path.join(self._dir, "in_progress")
        self._done = os.path.join(self._dir, "done")
        self._undelivered = os.path.join(self._dir, "undelivered")

        for d in [self._pending, self._in_progress, self._done, self._undelivered]:
            os.makedirs(d, exist_ok=True)

        # In-memory pending counter (avoids os.listdir on hot C-STORE path).
        self._pending_count = 0
        self._pending_lock = threading.Lock()

        # Sync from what's already on disk (survives restart).
        self._sync_pending_count()

    def _set_pending_gauge(self):
        """Push the in-memory pending count to this queue's Prometheus gauge.

        0409: routed through self._pending_gauge so the primary STOW queue and
        the forward queue each report to their own metric (isolation).
        """
        try:
            self._pending_gauge.set(self._pending_count)
        except Exception:
            pass

    # -- path helpers --------------------------------------------------

    def _safe_uid(self, sop_uid):
        return sop_uid.replace("\x00", "_").replace("/", "_").replace("\\", "_")

    def _dcm_path(self, dir_, sop_uid):
        return os.path.join(dir_, f"{self._safe_uid(sop_uid)}.dcm")

    def _meta_path(self, dir_, sop_uid):
        return os.path.join(dir_, f"{self._safe_uid(sop_uid)}.meta")

    def _mods_path(self, dir_, sop_uid):
        return os.path.join(dir_, f"{self._safe_uid(sop_uid)}.mods")

    # -- atomic file write ---------------------------------------------

    @staticmethod
    def _atomic_write(path, data, binary=False):
        """Write data atomically: tmp -> flush -> os.replace."""
        tmp = path + ".tmp"
        mode = "wb" if binary else "w"
        encoding = None if binary else "utf-8"
        with open(tmp, mode, encoding=encoding) as f:
            f.write(data)
            f.flush()
        os.replace(tmp, path)

    # -- modifications sidecar -----------------------------------------

    def _write_mods_sidecar(self, sop_uid, modifications):
        """Write FR-001 audit trail sidecar (best-effort)."""
        try:
            mods_json = []
            for tag, old_val, new_val in modifications:
                mods_json.append(
                    {
                        "tag": [tag[0], tag[1]],
                        "old": old_val,
                        "new": new_val,
                    }
                )
            self._atomic_write(
                self._mods_path(self._pending, sop_uid),
                json.dumps(mods_json),
            )
        except Exception as e:
            logging.warning("[OUTBOX] Failed to write .mods for %s: %s", sop_uid, e)

    @staticmethod
    def read_mods_sidecar(blob_path):
        """Read modifications sidecar for a blob file. Returns None if absent."""
        if not blob_path.endswith(".dcm"):
            return None
        mods_path = blob_path[:-4] + ".mods"
        if not os.path.isfile(mods_path):
            return None
        try:
            with open(mods_path, "r", encoding="utf-8") as f:
                return json.load(f)
        except Exception:
            return None

    @staticmethod
    def delete_mods_sidecar(blob_path):
        """Delete a modifications sidecar (best-effort)."""
        if not blob_path.endswith(".dcm"):
            return
        mods_path = blob_path[:-4] + ".mods"
        try:
            os.unlink(mods_path)
        except OSError:
            pass

    # -- core operations -----------------------------------------------

    def enqueue(self, dataset, modifications=None):
        """Persist DICOM instance to outbox. Thread-safe, atomic writes.

        Order: .dcm -> .mods -> .meta (LAST). Orphan .dcm without .meta =
        safe to ignore on startup (recover_stuck skips them).

        Returns True (inserted), False (error), or "duplicate" (already in queue).
        """
        t0 = time.time()
        sop_uid = str(getattr(dataset, "SOPInstanceUID", ""))
        if not sop_uid:
            logging.error("[OUTBOX] Dataset has no SOPInstanceUID - cannot enqueue")
            return False

        # Check if already in any queue directory (dedup).
        for d in [self._pending, self._in_progress, self._done]:
            if os.path.isfile(self._dcm_path(d, sop_uid)):
                logging.info(
                    "[OUTBOX] SOP %s already in queue - duplicate, 0x0000", sop_uid
                )
                return "duplicate"

        dcm_path = self._dcm_path(self._pending, sop_uid)

        # Step 1: Write DICOM blob (atomic).
        _ensure_file_meta(dataset)
        try:
            tmp = dcm_path + ".tmp"
            dcmwrite(tmp, dataset, enforce_file_format=True)
            os.replace(tmp, dcm_path)
        except Exception as e:
            logging.error("[OUTBOX] Failed to write blob for %s: %s", sop_uid, e)
            try:
                os.unlink(dcm_path + ".tmp")
            except OSError:
                pass
            return False

        # Step 2: Write modifications sidecar (best-effort, BEFORE .meta).
        if modifications:
            self._write_mods_sidecar(sop_uid, modifications)

        # Step 3: Write metadata (LAST  -  signals "this item is ready").
        now = time.time()
        meta = {
            "created_at": now,
            "retry_count": 0,
            "next_retry_at": now,
            "last_error": None,
        }
        meta_path = self._meta_path(self._pending, sop_uid)
        self._atomic_write(meta_path, json.dumps(meta))

        with self._pending_lock:
            self._pending_count += 1
        self._set_pending_gauge()

        logging.debug("[OUTBOX] Enqueued SOP %s", sop_uid)

        elapsed = time.time() - t0
        try:
            outbox_enqueue_latency_seconds.observe(elapsed)
        except Exception:
            pass

        return True

    # -- forward channel (0409) ----------------------------------------

    @staticmethod
    def _inc_forward_dropped():
        """Increment the forward DROP counter (0409 best-effort mirror)."""
        try:
            from clean_code.metrics import forward_dropped_total

            forward_dropped_total.inc()
        except Exception:
            pass

    def _link_or_copy(self, src, dst):
        """Place src bytes at dst with ZERO extra disk usage when possible.

        0409 forwarder: the blob is already atomically written in the PRIMARY
        outbox. Rather than duplicating its bytes, try a hard link
        first (same filesystem -> no extra bytes). If that fails (cross-device
        EXDEV, archived fs, no hardlink support) fall back to a byte copy via
        tmp + os.replace (still atomic, still crash-safe).

        Returns True on success, False on failure.
        """
        try:
            os.link(src, dst)
            return True
        except OSError:
            pass
        try:
            tmp = dst + ".tmp"
            shutil.copyfile(src, tmp)
            os.replace(tmp, dst)
            return True
        except OSError:
            try:
                os.unlink(dst + ".tmp")
            except OSError:
                pass
            return False

    def enqueue_from_blob(self, blob_path, sop_uid, modifications=None):
        """Mirror an ALREADY-persisted primary blob into the forward queue.

        The forward channel is best-effort: it must NEVER change the C-STORE
        status the modality already received from the primary enqueue. Called
        from handle_c_store AFTER the primary enqueue returns True.

        Contract (identical ready-marker semantics to enqueue(), outbox.py):
          forward/<dir>/{uid}.dcm  <- hardlinked/copied primary pending blob
          forward/<dir>/{uid}.mods <- audit sidecar (optional)
          forward/<dir>/{uid}.meta <- written LAST = "this item is ready"

        Returns:
          True       -> blob placed in forward/pending
          "duplicate"-> already mirrored (idempotent re-C-STORE)
          "full"     -> forward queue at max_queue (dropped + counter)
          False      -> placement failed (dropped + counter)

        The caller ignores return for the modality status, but logs/drops.
        """
        if not sop_uid or not blob_path or not os.path.isfile(blob_path):
            self._inc_forward_dropped()
            return False

        # Dedup: already mirrored in forward/pending or in_progress -> nothing
        # to do (re-C-STORE of the same SOP, or a leftover from a crash).
        for d in [self._pending, self._in_progress]:
            if os.path.isfile(self._dcm_path(d, sop_uid)):
                logging.debug(
                    "[OUTBOX] SOP %s already in forward queue - duplicate",
                    sop_uid,
                )
                return "duplicate"

        # Queue-full: drop-newest (best-effort, never backpressure primary).
        if self.max_queue and self._pending_count >= self.max_queue:
            logging.warning(
                "[OUTBOX] Forward queue full (%d) - dropping %s "
                "(primary 0x0000 unchanged)",
                self.max_queue,
                sop_uid,
            )
            self._inc_forward_dropped()
            return "full"

        dcm_path = self._dcm_path(self._pending, sop_uid)

        # Step 1: hardlink/copy the primary blob into forward/pending.
        #
        # The primary worker can claim and remove this blob at any moment: the
        # primary enqueue that produced it returns before the worker runs, so
        # the `isfile` check above races the delivery. One retry covers the
        # window where the file is present but briefly unavailable; a second
        # failure means the item really is gone, which the primary channel has
        # already accounted for.
        if not self._link_or_copy(blob_path, dcm_path):
            if os.path.isfile(blob_path) and self._link_or_copy(blob_path, dcm_path):
                pass
            else:
                logging.error(
                    "[OUTBOX] Forward enqueue failed to place blob for %s "
                    "(primary 0x0000 unchanged)",
                    sop_uid,
                )
                self._inc_forward_dropped()
                return False

        # Step 2: audit sidecar (best-effort, BEFORE .meta).
        if modifications:
            self._write_mods_sidecar(sop_uid, modifications)

        # Step 3: metadata LAST  -  signals "this item is ready" to the worker.
        now = time.time()
        meta = {
            "created_at": now,
            "retry_count": 0,
            "next_retry_at": now,
            "last_error": None,
        }
        meta_path = self._meta_path(self._pending, sop_uid)
        self._atomic_write(meta_path, json.dumps(meta))

        with self._pending_lock:
            self._pending_count += 1
        self._set_pending_gauge()

        logging.debug("[OUTBOX] Forward-enqueued SOP %s", sop_uid)
        return True

    def dequeue(self):
        """Atomically claim the oldest ready SOP.

        Scans pending/*.meta for items with next_retry_at <= now.
        os.rename from pending/ to in_progress/  -  atomic claim.
        On collision -> skip, try next.

        Returns (sop_uid, ds, blob_path, raw_bytes) 4-tuple,
        or (None, None, None, None) if queue is empty.
        """
        now = time.time()
        sop_uid = None

        try:
            names = os.listdir(self._pending)
        except OSError:
            return (None, None, None, None)

        # Find first ready item.
        for name in names:
            if not name.endswith(".meta"):
                continue
            uid = name[:-5]  # strip .meta
            meta_path = self._meta_path(self._pending, uid)

            try:
                with open(meta_path, "r", encoding="utf-8") as f:
                    meta = json.load(f)
            except (json.JSONDecodeError, OSError):
                continue

            next_retry = meta.get("next_retry_at", 0)
            retry_count = meta.get("retry_count", 0)
            if next_retry > now or retry_count >= self.MAX_RETRIES:
                continue

            # Atomic claim: move ALL files for this UID to in_progress/.
            dcm = self._dcm_path(self._pending, uid)
            mods = self._mods_path(self._pending, uid)

            try:
                os.rename(dcm, self._dcm_path(self._in_progress, uid))
                if os.path.isfile(mods):
                    os.rename(mods, self._mods_path(self._in_progress, uid))
                os.rename(meta_path, self._meta_path(self._in_progress, uid))
                sop_uid = uid
                break
            except OSError:
                # Another worker claimed it (or file removed)  -  skip.
                continue

        if sop_uid is None:
            return (None, None, None, None)

        # Read DICOM from claimed file.
        blob_path = self._dcm_path(self._in_progress, sop_uid)
        try:
            # Lazy parse (OPT-1): a full Dataset (incl. pixels) is needed ONLY
            # when the blob carries a .mods audit sidecar - the drain then
            # re-serializes it (stow re-write must keep PixelData). The
            # unmodified majority is delivered as raw_bytes passthrough; the
            # header-only parse (stop_before_pixels) still yields every field
            # the drain needs for study bucketing, stats and logging without
            # walking the bulky pixel element.
            modified = os.path.isfile(self._mods_path(self._in_progress, sop_uid))
            if modified:
                ds = dcmread(blob_path)
            else:
                ds = dcmread(blob_path, stop_before_pixels=True)
            with open(blob_path, "rb") as f:
                raw_bytes = f.read()
            return (sop_uid, ds, blob_path, raw_bytes)
        except Exception as e:
            logging.error("[OUTBOX] Failed to read blob %s: %s", blob_path, e)
            self.mark_failed(sop_uid, f"Blob read error: {e}")
            retries = self._get_retry_count(sop_uid)
            if retries >= self.MAX_RETRIES:
                self.move_to_undelivered(sop_uid)
            return (None, None, None, None)

    # -- delivery status -----------------------------------------------

    def mark_delivered(self, sop_uid):
        """Mark SOP as successfully delivered."""
        # Find which dir the SOP is in (normally in_progress/, but allow pending/ for test flexibility).
        dir_ = self._in_progress
        if not os.path.isfile(self._dcm_path(dir_, sop_uid)):
            dir_ = self._pending
        dcm = self._dcm_path(dir_, sop_uid)
        meta = self._meta_path(dir_, sop_uid)
        mods = self._mods_path(dir_, sop_uid)

        if self.cleanup:
            for p in [dcm, meta, mods]:
                try:
                    os.unlink(p)
                except OSError:
                    pass
        else:
            for p in [dcm, meta]:
                dst = p.replace(dir_, self._done)
                try:
                    os.rename(p, dst)
                except OSError:
                    pass
            if os.path.isfile(mods):
                try:
                    os.rename(mods, self._mods_path(self._done, sop_uid))
                except OSError:
                    pass

        with self._pending_lock:
            self._pending_count = max(0, self._pending_count - 1)
        self._set_pending_gauge()
        logging.debug("[OUTBOX] Delivered SOP %s", sop_uid)

    def delivered(self, sop_uid):
        """True when this SOP is no longer waiting for delivery anywhere.

        The bridge's own delivery state is the durable fact and the ONLY
        readiness signal: the server ACKs a STOW-RS only after durable
        persist, so "not in this queue any more" means the origin has it.
        A C-STORE that was never enqueued (a modality re-request, a restart
        with cleanup on) also reads as delivered, which is correct: the
        bridge is not holding it, so nothing local stands between the SOP
        and the origin's verdict.

        Read-side only - the worker is the single writer of this state.
        Checks undelivered/ FIRST: a SOP that exhausted its retries must
        never read as delivered.
        """
        if os.path.isfile(self._dcm_path(self._undelivered, sop_uid)):
            return False
        for dir_ in (self._pending, self._in_progress):
            if os.path.isfile(self._dcm_path(dir_, sop_uid)):
                return False
        return True

    def mark_failed(self, sop_uid, error):
        """Mark delivery as failed. Retry with backoff.

        Works on items in pending/ or in_progress/. Updates .meta
        with retry_count and next_retry_at. If already in pending/,
        just updates .meta in place (no rename needed).
        Only calls move_to_undelivered when retry_count >= MAX_RETRIES.
        """
        # Find which dir the SOP is in.
        dir_ = self._in_progress
        if not os.path.isfile(self._dcm_path(dir_, sop_uid)):
            dir_ = self._pending
        meta_path = self._meta_path(dir_, sop_uid)

        retry_count = self._get_retry_count(sop_uid) + 1
        if self.retry_interval == 0:
            delay = min(2**retry_count, 120)
        else:
            delay = self.retry_interval
        next_retry = time.time() + delay

        meta = {
            "created_at": time.time(),
            "retry_count": retry_count,
            "next_retry_at": next_retry,
            "last_error": str(error)[:200],
        }

        # Atomic update of .meta.
        tmp = meta_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(meta, f)
            f.flush()
        os.replace(tmp, meta_path)

        # If in in_progress/, move back to pending/.
        if dir_ == self._in_progress:
            dcm = self._dcm_path(self._in_progress, sop_uid)
            mods = self._mods_path(self._in_progress, sop_uid)
            try:
                os.rename(dcm, self._dcm_path(self._pending, sop_uid))
                if os.path.isfile(mods):
                    os.rename(mods, self._mods_path(self._pending, sop_uid))
                os.rename(meta_path, self._meta_path(self._pending, sop_uid))
            except OSError as e:
                logging.error(
                    "[OUTBOX] Failed to move %s back to pending: %s", sop_uid, e
                )

        if retry_count >= self.MAX_RETRIES:
            logging.error(
                "[OUTBOX] SOP %s: max retries (%d) exceeded - needs manual intervention",
                sop_uid,
                self.MAX_RETRIES,
            )
            self.move_to_undelivered(sop_uid)
        else:
            logging.warning(
                "[OUTBOX] SOP %s: delivery failed (retry %d/%d): %s",
                sop_uid,
                retry_count,
                self.MAX_RETRIES,
                error,
            )

    def requeue(self, sop_uid, reason):
        """Move a claimed item back to pending/ WITHOUT bumping retry count.

        2908: used for 507 Insufficient Storage backpressure. The item is
        NOT a failed delivery - it stays pending (retry_count unchanged),
        only last_error is recorded for diagnostics.
        """
        dcm = self._dcm_path(self._in_progress, sop_uid)
        if not os.path.isfile(dcm):
            return
        mods = self._mods_path(self._in_progress, sop_uid)
        meta_path = self._meta_path(self._in_progress, sop_uid)
        try:
            os.rename(dcm, self._dcm_path(self._pending, sop_uid))
            if os.path.isfile(mods):
                os.rename(mods, self._mods_path(self._pending, sop_uid))
            os.rename(meta_path, self._meta_path(self._pending, sop_uid))
        except OSError as e:
            logging.error(
                "[OUTBOX] Failed to requeue %s back to pending: %s",
                sop_uid,
                e,
            )
            return

        # Update last_error only - retry_count stays (507 is not a failure).
        try:
            meta_target = self._meta_path(self._pending, sop_uid)
            with open(meta_target, "r", encoding="utf-8") as f:
                meta = json.load(f)
            meta["last_error"] = str(reason)[:200]
            self._atomic_write(meta_target, json.dumps(meta))
        except (OSError, json.JSONDecodeError):
            pass
        logging.warning(
            "[OUTBOX] SOP %s requeued to pending (no retry bump): %s",
            sop_uid,
            reason,
        )

    def move_to_undelivered(self, sop_uid):
        """Move exhausted-retry SOP to undelivered/  -  manual intervention needed."""
        from clean_code.metrics import outbox_undelivered_total

        # 0409: forward queue increments its own counter, not the primary's.
        undelivered_counter = (
            self._undelivered_total
            if self._undelivered_total is not None
            else outbox_undelivered_total
        )

        dcm = self._dcm_path(self._pending, sop_uid)
        # Also check in_progress (edge case: max retries hit during in-flight failure).
        if not os.path.isfile(dcm):
            dcm = self._dcm_path(self._in_progress, sop_uid)
        dst = self._dcm_path(self._undelivered, sop_uid)

        try:
            shutil.move(dcm, dst)
            logging.critical("[OUTBOX] SOP %s moved to UNDELIVERED: %s", sop_uid, dst)
        except OSError as e:
            logging.critical(
                "[OUTBOX] Failed to move %s to undelivered: %s", sop_uid, e
            )

        # Clean up .meta and .mods.
        for dir_ in [self._pending, self._in_progress]:
            for p in [self._meta_path(dir_, sop_uid), self._mods_path(dir_, sop_uid)]:
                try:
                    os.unlink(p)
                except OSError:
                    pass

        with self._pending_lock:
            self._pending_count = max(0, self._pending_count - 1)
        self._set_pending_gauge()
        try:
            undelivered_counter.inc()
        except Exception:
            pass

    # -- recovery ------------------------------------------------------

    def recover_stuck(self):
        """Move stuck in_progress/ items back to pending/. Called at startup and periodically.

        Two categories:
          1. in_progress/ files with mtime older than STUCK_ITEM_TIMEOUT -> back to pending/
          2. pending/ items with retry_count >= MAX_RETRIES -> move to undelivered/
        """
        now = time.time()
        cutoff = now - STUCK_ITEM_TIMEOUT
        recovered = 0
        undelivered_count = 0

        # Category 1: stuck in_progress -> pending.
        try:
            for name in os.listdir(self._in_progress):
                if not name.endswith(".dcm"):
                    continue
                uid = name[:-4]
                dcm_path = os.path.join(self._in_progress, name)
                try:
                    if os.path.getmtime(dcm_path) < cutoff:
                        for ext in [".dcm", ".meta", ".mods"]:
                            src = os.path.join(self._in_progress, uid + ext)
                            dst = os.path.join(self._pending, uid + ext)
                            if os.path.isfile(src):
                                os.rename(src, dst)
                        recovered += 1
                except OSError:
                    pass
        except OSError:
            pass

        # Category 1b: orphan .dcm files in pending/ without .meta (crashed before metadata write).
        # Create a .meta so dequeue() can find them.
        try:
            for name in os.listdir(self._pending):
                if not name.endswith(".dcm"):
                    continue
                uid = name[:-4]
                meta_path = self._meta_path(self._pending, uid)
                if not os.path.isfile(meta_path):
                    now_ts = time.time()
                    meta = {
                        "created_at": now_ts,
                        "retry_count": 0,
                        "next_retry_at": now_ts,
                        "last_error": None,
                    }
                    self._atomic_write(meta_path, json.dumps(meta))
                    recovered += 1
                    logging.info("[OUTBOX] Created .meta for orphan %s", uid)
        except OSError:
            pass

        if recovered > 0:
            logging.warning(
                "[OUTBOX] Recovered %d stuck in_progress item(s)", recovered
            )

        # Category 2: max-retry items -> undelivered.
        try:
            for name in os.listdir(self._pending):
                if not name.endswith(".meta"):
                    continue
                uid = name[:-5]
                meta_path = os.path.join(self._pending, name)
                try:
                    with open(meta_path, "r", encoding="utf-8") as f:
                        meta = json.load(f)
                except (json.JSONDecodeError, OSError):
                    continue
                if meta.get("retry_count", 0) >= self.MAX_RETRIES:
                    dcm = self._dcm_path(self._pending, uid)
                    if os.path.isfile(dcm):
                        self.move_to_undelivered(uid)
                        undelivered_count += 1
        except OSError:
            pass

        if undelivered_count > 0:
            logging.warning(
                "[OUTBOX] Moved %d max-retry item(s) to undelivered", undelivered_count
            )

        # Sync in-memory counter from FS.
        self._sync_pending_count()

        return (recovered, undelivered_count)

    # -- query helpers -------------------------------------------------

    def pending_count(self):
        """Return approximate pending count (in-memory, O(1))."""
        with self._pending_lock:
            return self._pending_count

    def _sync_pending_count(self):
        """Recalculate pending count from FS (startup / recovery only)."""
        try:
            count = sum(1 for n in os.listdir(self._pending) if n.endswith(".dcm"))
        except OSError:
            count = 0
        with self._pending_lock:
            self._pending_count = count
        self._set_pending_gauge()
        return count

    def next_retry_wait_sec(self):
        """Return seconds until the next SOP is ready for retry, or 5.0 if empty.

        Scans pending/*.meta to find min next_retry_at. O(n) on <=1000 files  -  fine.
        """
        now = time.time()
        earliest = None
        try:
            for name in os.listdir(self._pending):
                if not name.endswith(".meta"):
                    continue
                meta_path = os.path.join(self._pending, name)
                try:
                    with open(meta_path, "r", encoding="utf-8") as f:
                        meta = json.load(f)
                except (json.JSONDecodeError, OSError):
                    continue
                retry_count = meta.get("retry_count", 0)
                next_retry = meta.get("next_retry_at", 0)
                if retry_count < self.MAX_RETRIES and next_retry > now:
                    if earliest is None or next_retry < earliest:
                        earliest = next_retry
        except OSError:
            pass

        if earliest is not None:
            wait = earliest - now
            return max(0.1, min(wait, 5.0))
        return 5.0

    def _get_retry_count(self, sop_uid):
        """Return current retry count for a SOP."""
        for dir_ in [self._pending, self._in_progress]:
            meta_path = self._meta_path(dir_, sop_uid)
            if os.path.isfile(meta_path):
                try:
                    with open(meta_path, "r", encoding="utf-8") as f:
                        return json.load(f).get("retry_count", 0)
                except (json.JSONDecodeError, OSError):
                    pass
        return 0

    def close(self):
        """No-op on filesystem."""
