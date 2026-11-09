# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""SOP Instance UID index  -  filesystem-backed key-value store.

nginx-way: one JSON file per SOP UID. Directory = index.
No SQLite. No locks. No busy_timeout.
"""

import json
import logging
import os
import time


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

    def _path(self, sop_uid):
        safe = sop_uid.replace("\x00", "_").replace("/", "_").replace("\\", "_")
        return os.path.join(self._dir, f"{safe}.json")

    def insert_sop(self, sop_uid, study_uid, series_uid):
        """Insert or replace a SOP entry. Idempotent."""
        data = {
            "study_uid": study_uid,
            "series_uid": series_uid,
            "ts": int(time.time()),
            "is_verified": False,
        }
        path = self._path(sop_uid)
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f)
            f.flush()
        os.replace(tmp, path)

    def lookup_sop(self, sop_uid):
        """Look up a SOP UID. Returns dict or None."""
        path = self._path(sop_uid)
        if not os.path.isfile(path):
            return None
        try:
            with open(path, "r", encoding="utf-8") as f:
                return json.load(f)
        except (json.JSONDecodeError, OSError) as e:
            logging.warning("[SOP-INDEX] Corrupt %s: %s", path, e)
            return None

    def mark_verified(self, sop_uid):
        """Mark a SOP as verified after successful N-ACTION check."""
        entry = self.lookup_sop(sop_uid)
        if entry is None:
            return
        entry["is_verified"] = True
        path = self._path(sop_uid)
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(entry, f)
            f.flush()
        os.replace(tmp, path)

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
