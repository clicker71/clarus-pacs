# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""SOP Instance UID index  -  filesystem-backed key-value store.

nginx-way: one JSON file per SOP UID. Directory = index.
No SQLite. No locks. No busy_timeout.
"""

import json
import logging
import os
import threading
import time

from clean_code.utils import LockStripes, replace_with_retry


class SopIndexManager:
    """Filesystem-backed index: SOPInstanceUID -> (StudyInstanceUID, SeriesInstanceUID).

    Layout:
        sop_index/{sop_uid}.json -> {"study_uid": "...", "series_uid": "...",
                                     "ts": ..., "is_verified": false}

    Populated at C-STORE time after successful STOW-RS (HTTP 200/201/202).
    Looked up at N-ACTION time to verify Storage Commitment.
    """

    def __init__(self, index_dir):
        """index_dir: path to sop_index directory (created if absent)."""
        self._dir = index_dir
        os.makedirs(self._dir, exist_ok=True)
        # One guard per index file. A unique temp name stops two writers from
        # sharing a file, but they still rename onto the SAME destination, and
        # on Windows the second `os.replace` fails with a sharing violation
        # while the first destination handle is open - which loses the entry.
        # The guard covers readers too: `lookup_sop` holds the destination open
        # while it parses it, and that open handle blocks the rename just as
        # surely as a competing writer would. Striped rather than one lock per
        # path: cleanup() deletes index files but cannot reclaim their locks,
        # so a per-path dict grows for the life of the process.
        self._publish_guards = LockStripes()

    def _guard_for(self, path):
        """The guard serialising access to one index file."""
        return self._publish_guards.for_key(path)

    def _path(self, sop_uid):
        # Percent-escape rather than substitute: collapsing `\x00`, `/` and `\`
        # all to `_` made two distinct UIDs share one index file, so one SOP
        # silently overwrote the other's entry. See OutboxQueue._safe_uid.
        safe = (
            sop_uid.replace("%", "%25")
            .replace("\x00", "%00")
            .replace("/", "%2F")
            .replace("\\", "%5C")
        )
        return os.path.join(self._dir, f"{safe}.json")

    def _write_unlocked(self, path, data):
        """Write `data` to `path` atomically. The caller holds the guard.

        The index is keyed by SOP UID, so two reactor threads handling the same
        instance write the SAME path. A shared `<path>.tmp` made them interleave
        into one file and both `os.replace` it, which loses an entry outright (a
        stored SOP then reads as unconfirmed) and leaves stray `.json.tmp` files
        behind when a write fails.
        """
        tmp = f"{path}.{os.getpid()}.{threading.get_ident()}.tmp"
        try:
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(data, f)
                f.flush()
            replace_with_retry(tmp, path)
        except Exception:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise

    def _write(self, path, data):
        """Write `data` to `path` atomically, as one publish operation."""
        with self._guard_for(path):
            self._write_unlocked(path, data)

    def insert_sop(self, sop_uid, study_uid, series_uid):
        """Insert or replace a SOP entry. Idempotent."""
        data = {
            "study_uid": study_uid,
            "series_uid": series_uid,
            "ts": int(time.time()),
            "is_verified": False,
        }
        path = self._path(sop_uid)
        self._write(path, data)

    def _read_unlocked(self, path):
        """Read one index file. The caller holds the guard."""
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)

    def lookup_sop(self, sop_uid):
        """Look up a SOP UID. Returns dict or None."""
        path = self._path(sop_uid)
        if not os.path.isfile(path):
            return None
        try:
            # Under the per-file guard: on Windows this open handle would
            # otherwise block a concurrent writer's `os.replace` with a
            # sharing violation, which loses the entry it was writing.
            with self._guard_for(path):
                return self._read_unlocked(path)
        except (json.JSONDecodeError, OSError) as e:
            logging.warning("[SOP-INDEX] Corrupt %s: %s", path, e)
            return None

    def mark_verified(self, sop_uid):
        """Mark a SOP as verified after successful N-ACTION check.

        The read, the update and the write are ONE locked step. Reading with
        the guard released (`lookup_sop` then `_write`) let an `insert_sop`
        publish a fresh replacement in between, which this path then
        overwrote with the stale scope it had read and marked verified.
        """
        path = self._path(sop_uid)
        with self._guard_for(path):
            if not os.path.isfile(path):
                return
            try:
                entry = self._read_unlocked(path)
            except (json.JSONDecodeError, OSError) as e:
                logging.warning("[SOP-INDEX] Corrupt %s: %s", path, e)
                return
            entry["is_verified"] = True
            self._write_unlocked(path, entry)

    def cleanup(self, max_age_seconds=86400):
        """Remove entries older than max_age_seconds (default: 24h)."""
        cutoff = time.time() - max_age_seconds
        removed = 0
        try:
            for name in os.listdir(self._dir):
                if not name.endswith(".json"):
                    continue
                path = os.path.join(self._dir, name)
                try:
                    # The guard spans the mtime check AND the unlink: an
                    # `insert_sop` between them publishes a fresh replacement
                    # that the unlink would then delete (TOCTOU). Holding it
                    # serialises the sweep against a publisher of this file.
                    with self._guard_for(path):
                        if os.path.getmtime(path) < cutoff:
                            os.unlink(path)
                            removed += 1
                except OSError:
                    pass
        except OSError:
            pass
        if removed > 0:
            logging.info("[SOP-INDEX] Cleaned %d entries", removed)

    def count(self):
        """Return total number of entries."""
        try:
            return sum(1 for n in os.listdir(self._dir) if n.endswith(".json"))
        except OSError:
            return 0

    def close(self):
        """No-op on filesystem."""
