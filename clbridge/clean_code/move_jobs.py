# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge - C-MOVE job queue and SCP target registry (BR-007).

MoveJobManager: SQLite-backed C-MOVE job queue with SCP target auto-registration.
Thread-safe via SharedDb: one connection, one lock.
Survives bridge restarts.

Known limitations:
- C-MOVE-CANCEL not supported. Client can close association;
  Bridge finishes the current job (data won't be lost).
- No duplicate SOP check before re-send after restart.
- Double I/O: temp-file -> read -> send (streaming future).
"""

import ipaddress
import logging
import threading
import time


# ---------------------------------------------------------------------------
# DB Migration helpers (subnet support, composite keys)
# ---------------------------------------------------------------------------
def _migrate_move_jobs(conn):
    """Add target_host/target_port columns if missing (pre-existing DBs)."""
    cols = {r[1] for r in conn.execute("PRAGMA table_info('move_jobs')").fetchall()}
    if "target_host" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN target_host TEXT NOT NULL DEFAULT ''"
        )
    if "target_port" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN target_port INTEGER NOT NULL DEFAULT 0"
        )
    # (0008,0058) Failed SOP Instance UID List - newline-joined UIDs of
    # the slices that failed transfer in this job (PS3.4 C.4.2.1.4.2). The
    # C-MOVE reactor reads it to put the list on the final Failed/Cancel/
    # Warning C-MOVE-RSP Identifier.
    if "failed_sops" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN failed_sops TEXT NOT NULL DEFAULT ''"
        )
    # Per-association expose flag: 1 when this job was requested over the
    # Expose Rejected Instances AE. Carried on the job because the C-MOVE
    # worker threads are long-lived and outlive the association that queued
    # the work, so it cannot be read from the event when the job runs.
    if "expose_rejected" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN expose_rejected INTEGER NOT NULL DEFAULT 0"
        )


def _migrate_scp_targets(conn):
    """Migrate scp_targets from single PK (ae_title) to composite (ae_title, host).

    Preserves existing data. Idempotent  -  safe to run multiple times.
    """
    # Check if old schema (ae_title as sole PK) exists
    cols = {r[1] for r in conn.execute("PRAGMA table_info('scp_targets')").fetchall()}
    if not cols:
        # Table doesn't exist yet  -  create with v2 schema
        conn.execute(
            "CREATE TABLE IF NOT EXISTS scp_targets ("
            "  ae_title TEXT NOT NULL,"
            "  host TEXT NOT NULL,"
            "  port INTEGER NOT NULL DEFAULT 104,"
            "  last_seen INTEGER NOT NULL,"
            "  consecutive_failures INTEGER DEFAULT 0,"
            "  PRIMARY KEY (ae_title, host)"
            ")"
        )
        return

    # Check if already migrated: composite PK present.
    # SQLite PRAGMA table_info doesn't directly show composite PK.
    # Check by trying to insert a duplicate (ae, host)  -  if UNIQUE constraint
    # fails on (ae_title) alone, we're on old schema.
    # Simpler: check if 'host' is part of PK by looking at the CREATE TABLE sql
    create_sql = conn.execute(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='scp_targets'"
    ).fetchone()
    if create_sql and "PRIMARY KEY (ae_title, host)" in create_sql[0]:
        return  # Already v2

    # Migrate: create v2, copy data, drop v1, rename
    conn.execute(
        "CREATE TABLE scp_targets_v2 ("
        "  ae_title TEXT NOT NULL,"
        "  host TEXT NOT NULL,"
        "  port INTEGER NOT NULL DEFAULT 104,"
        "  last_seen INTEGER NOT NULL,"
        "  consecutive_failures INTEGER DEFAULT 0,"
        "  PRIMARY KEY (ae_title, host)"
        ")"
    )
    conn.execute(
        "INSERT OR IGNORE INTO scp_targets_v2"
        " SELECT ae_title, host, port, last_seen, consecutive_failures"
        " FROM scp_targets"
    )
    conn.execute("DROP TABLE scp_targets")
    conn.execute("ALTER TABLE scp_targets_v2 RENAME TO scp_targets")


# ---------------------------------------------------------------------------
# Bridge: C-MOVE job queue and SCP target registry
# ---------------------------------------------------------------------------
class MoveJobManager:
    """SQLite-backed C-MOVE job queue with SCP target auto-registration.

    Thread-safe via SharedDb  -  "one connection, one lock".
    Survives bridge restarts.

    Known limitations:
    - C-MOVE-CANCEL not supported. Client can close association;
      Bridge finishes the current job (data won't be lost).
    - No duplicate SOP check before re-send after restart.
    - Double I/O: temp-file -> read -> send (streaming future).
    """

    def __init__(self, shared_db, max_workers=2, scp_remove_failures=3):
        """shared_db: SharedDb instance (NOT raw sqlite3.Connection).

        Uses SharedDb.locked() for ALL DB operations - no own lock.
        One connection, one lock: a single serialized writer avoids both lock
        ordering bugs and SQLite busy-handler tuning.

        scp_remove_failures: consecutive-failure threshold for the auto-remove
        of an auto-registered SCP peer (CM-006). Fixed constant 3 (fed from
        `_SCP_FAILURES_BEFORE_REMOVE` in main.py); there is no config knob.
        """
        self._db = shared_db  # SharedDb, NOT sqlite3.Connection
        self._cond = (
            threading.Condition()
        )  # notify workers (own cond, not on SharedDb lock)
        self.max_workers = max_workers
        self.scp_remove_failures = max(1, int(scp_remove_failures or 3))
        self._cancel_callbacks = {}  # job_id -> callable
        self._last_peers_reload = 0.0  # timestamp of last peers reload
        self.lazyload_peers_interval = (
            30  # seconds between peers.conf reloads (0 = every call)
        )

        with self._db.locked() as conn:
            conn.execute(
                "CREATE TABLE IF NOT EXISTS move_jobs ("
                "  id INTEGER PRIMARY KEY AUTOINCREMENT,"
                "  study_uid TEXT NOT NULL,"
                "  move_dest_ae TEXT NOT NULL,"
                "  target_host TEXT NOT NULL DEFAULT '',"
                "  target_port INTEGER NOT NULL DEFAULT 0,"
                "  status TEXT NOT NULL DEFAULT 'pending',"
                "  total_instances INTEGER DEFAULT 0,"
                "  completed_instances INTEGER DEFAULT 0,"
                "  failed_instances INTEGER DEFAULT 0,"
                "  error_message TEXT,"
                "  failed_sops TEXT NOT NULL DEFAULT '',"
                "  created_at INTEGER NOT NULL,"
                "  updated_at INTEGER NOT NULL,"
                "  attempts INTEGER DEFAULT 0,"
                "  expose_rejected INTEGER NOT NULL DEFAULT 0"
                ")"
            )
            # Migration: add target_host/target_port columns if missing (pre-existing DBs)
            _migrate_move_jobs(conn)
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_move_jobs_status ON move_jobs(status)"
            )
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_move_jobs_created ON move_jobs(created_at)"
            )
            # scp_targets v2: composite primary key (ae_title, host) for subnet support
            _migrate_scp_targets(conn)
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_scp_targets_ae ON scp_targets(ae_title)"
            )
            conn.commit()

    # --- move_jobs CRUD ---

    def create_job(
        self,
        study_uid,
        move_dest_ae,
        target_host="",
        target_port=0,
        expose_rejected=False,
    ):
        """Create a new pending C-MOVE job. Returns job id.

        target_host/target_port: resolved Move Destination address.
        Stored so worker doesn't need to re-resolve (subnet support).
        expose_rejected: the job was queued over the QA AE; the worker queries
        the configured `[dicomweb] qa_url` endpoint instead of `clarus_url`.
        Stored because the worker outlives the association.
        """
        now = int(time.time())
        with self._db.locked() as conn:
            cur = conn.execute(
                "INSERT INTO move_jobs"
                " (study_uid, move_dest_ae, target_host, target_port,"
                "  status, created_at, updated_at, expose_rejected)"
                " VALUES (?, ?, ?, ?, 'pending', ?, ?, ?)",
                (
                    study_uid,
                    move_dest_ae,
                    target_host,
                    target_port,
                    now,
                    now,
                    1 if expose_rejected else 0,
                ),
            )
            conn.commit()
            with self._cond:
                self._cond.notify()  # wake one waiting worker
            return cur.lastrowid

    def count_pending(self):
        """Number of jobs in the 'pending' state (drain gauge).

        Read-only and cheap (one indexed COUNT); never raises, so a metrics
        scrape cannot be broken by a busy/locked database - returns 0 instead.
        """
        try:
            with self._db.locked() as conn:
                row = conn.execute(
                    "SELECT COUNT(*) FROM move_jobs WHERE status = 'pending'"
                ).fetchone()
            return int(row[0]) if row else 0
        except Exception:
            return 0

    def pick_pending_job(self, timeout=5.0):
        """Atomically pick one pending job.

        Returns dict or None.
        """
        while True:
            with self._db.locked() as conn:
                rows = conn.execute(
                    "SELECT id, study_uid, move_dest_ae, attempts,"
                    " target_host, target_port"
                    " FROM move_jobs WHERE status = 'pending'"
                    " ORDER BY created_at ASC"
                ).fetchall()

            if len(rows) == 0:
                with self._cond:
                    notified = self._cond.wait(timeout=timeout)
                if not notified:
                    return None
                continue

            row = rows[0]
            job_id = row[0]
            now = int(time.time())
            with self._db.locked() as conn:
                row2 = conn.execute(
                    "SELECT id, study_uid, move_dest_ae, attempts,"
                    " target_host, target_port, expose_rejected"
                    " FROM move_jobs WHERE status = 'pending' AND id = ?",
                    (job_id,),
                ).fetchone()
                if not row2:
                    continue  # Another worker claimed it

                conn.execute(
                    "UPDATE move_jobs SET status = 'in_progress',"
                    " updated_at = ?, attempts = attempts + 1"
                    " WHERE id = ?",
                    (now, job_id),
                )
                conn.commit()
                return {
                    "id": job_id,
                    "study_uid": row2[1],
                    "move_dest_ae": row2[2],
                    "attempts": row2[3],
                    "target_host": row2[4],
                    "target_port": row2[5],
                    # The worker outlives the association that queued the job,
                    # so which base URL to stream from has to come from the
                    # row. Omitting it here silently sent every QA job to
                    # clarus_url, i.e. the sequester was never lifted.
                    "expose_rejected": int(row2[6] or 0),
                }

    def update_job_progress(self, job_id, completed, failed, total):
        """Update job progress counters (CM-005)."""
        now = int(time.time())
        with self._db.locked() as conn:
            conn.execute(
                "UPDATE move_jobs SET completed_instances = ?,"
                " failed_instances = ?, total_instances = ?,"
                " updated_at = ? WHERE id = ?",
                (completed, failed, total, now, job_id),
            )
            conn.commit()

    def get_job_progress(self, job_id):
        """Return (completed, total, status) for a job. Used by C-MOVE generator for 0xFF00 polling."""
        with self._db.locked() as conn:
            row = conn.execute(
                "SELECT completed_instances, total_instances, status"
                " FROM move_jobs WHERE id = ?",
                (job_id,),
            ).fetchone()
            if row is None:
                return (0, 0, "unknown")
            return (row[0], row[1], row[2])

    def set_job_failed_sops(self, job_id, uids):
        """Record the (0008,0058) Failed SOP Instance UID List for a job.

        newline-joined UIDs of the slices that were attempted but not
        delivered. Read back by the C-MOVE generator so the final
        Failed/Cancel/Warning C-MOVE-RSP Identifier carries the list
        (PS3.4 C.4.2.1.4.2). Call once at job end.
        """
        text = "\n".join(u for u in (uids or []) if u)
        now = int(time.time())
        with self._db.locked() as conn:
            conn.execute(
                "UPDATE move_jobs SET failed_sops = ?, updated_at = ? WHERE id = ?",
                (text, now, job_id),
            )
            conn.commit()

    def get_job_failed_sops(self, job_id):
        """Return the (0008,0058) Failed SOP Instance UID list for a job."""
        with self._db.locked() as conn:
            row = conn.execute(
                "SELECT failed_sops FROM move_jobs WHERE id = ?",
                (job_id,),
            ).fetchone()
        if not row or not row[0]:
            return []
        return [u for u in str(row[0]).split("\n") if u]

    def finish_job(self, job_id, success=True, error_message=None):
        """Mark job as success or failed. A terminal row is never rewritten.

        Two wakers can finish the same job - the poll reactor on timeout and
        the worker when it reaches the end of the instance list - and the last
        writer would otherwise decide the outcome. A row that already says
        `failed` must not turn into `success` (or the reverse), because the
        viewer was already told the first verdict and the counter was already
        incremented for it.

        Returns True when THIS call moved the row to a terminal status, False
        when another waker had already finished it. The caller must use that
        answer before counting the outcome: a second waker that counts its own
        call double-counts a job that only finished once.
        """
        status = "success" if success else "failed"
        now = int(time.time())
        with self._db.locked() as conn:
            cur = conn.execute(
                "UPDATE move_jobs SET status = ?, error_message = ?,"
                " updated_at = ? WHERE id = ?"
                " AND status NOT IN ('success', 'failed')",
                (status, error_message or "", now, job_id),
            )
            conn.commit()
            return cur.rowcount > 0

    def reset_job_to_pending(self, job_id, reason=""):
        """Reset an in_progress job back to pending (viewer busy, retry later)."""
        now = int(time.time())
        with self._db.locked() as conn:
            conn.execute(
                "UPDATE move_jobs SET status = 'pending', error_message = ?,"
                " updated_at = ? WHERE id = ?",
                (reason, now, job_id),
            )
            conn.commit()
        with self._cond:
            self._cond.notify()  # Wake a worker to process the reset job (after fail_timeout)

    def recover_in_progress_jobs(self):
        """On startup: reset all in_progress jobs to pending (CM-007)."""
        now = int(time.time())
        with self._db.locked() as conn:
            cur = conn.execute(
                "UPDATE move_jobs SET status = 'pending', updated_at = ?"
                " WHERE status = 'in_progress'",
                (now,),
            )
            conn.commit()
            return cur.rowcount

    def cleanup_old_jobs(self, max_age_seconds=86400):
        """Remove success/failed jobs older than max_age_seconds (default 24h) (CM-008)."""
        with self._db.locked() as conn:
            cutoff = int(time.time()) - max_age_seconds
            conn.execute(
                "DELETE FROM move_jobs WHERE status IN ('success', 'failed')"
                " AND updated_at < ?",
                (cutoff,),
            )
            conn.commit()

    # --- scp_targets CRUD ---

    def register_peer(self, ae_title, host, port=104):
        """Register or update a manually configured peer from peers.conf.

        Composite key (ae_title, host)  -  supports multiple entries per AE
        (exact IP + CIDR subnet, or multiple subnets).
        Non-CIDR (exact IP) entries: only ONE per AE title  -  re-registration
        with a different host replaces the old one. CIDR entries coexist.
        """
        now = int(time.time())
        with self._db.locked() as conn:
            # Non-CIDR host: delete any existing non-CIDR entry for this AE
            # so re-registration with a different IP replaces the old one.
            if "/" not in host:
                conn.execute(
                    "DELETE FROM scp_targets"
                    " WHERE ae_title = ? AND host NOT LIKE '%/%'",
                    (ae_title,),
                )
            conn.execute(
                "INSERT OR REPLACE INTO scp_targets"
                " (ae_title, host, port, last_seen, consecutive_failures)"
                " VALUES (?, ?, ?, ?, 0)",
                (ae_title, host, port, now),
            )
            conn.commit()
        logging.info(
            "[PEER] Registered %s at %s:%d",
            ae_title,
            host,
            port,
        )

    # --- SCP peer lookup with subnet support --------------------

    def lookup_scp(self, ae_title, client_ip=None):
        """Look up SCP peer by AE Title, with optional IP subnet matching.

        Args:
            ae_title: Move Destination AE Title.
            client_ip: IP address of the requesting SCU (for subnet matching).
                If None, returns first exact-match (non-CIDR) entry.

        Returns:
            (host, port) tuple or None.

        Subnet matching (CIDR):
            peers.conf supports: WEASIS_AE  192.168.1.0/24  11113
            When client_ip falls within the subnet, returns the CIDR host
            (e.g. "192.168.1.0/24") and port. Caller uses the PORT for
            association; the HOST is informational (routed via client_ip).
        """
        with self._db.locked() as conn:
            rows = conn.execute(
                "SELECT host, port FROM scp_targets WHERE ae_title = ?",
                (ae_title,),
            ).fetchall()

        if not rows:
            return None

        if client_ip is None:
            # No IP  -  return first exact (non-CIDR) match, else first entry
            for host, port in rows:
                if "/" not in host:
                    return (host, port)
            return (rows[0][0], rows[0][1])

        # IP provided  -  match against subnets
        try:
            client = ipaddress.ip_address(client_ip)
        except ValueError:
            # Invalid IP string  -  fall back to exact match
            for host, port in rows:
                if "/" not in host:
                    return (host, port)
            return None

        # Priority 1: exact IP match (non-CIDR)
        for host, port in rows:
            if "/" not in host and host == client_ip:
                return (host, port)

        # Priority 2: CIDR subnet match
        for host, port in rows:
            if "/" in host:
                try:
                    network = ipaddress.ip_network(host, strict=False)
                    if client in network:
                        return (host, port)
                except ValueError:
                    continue

        return None

    def reload_peers_from_file(self, peers_file):
        """Reload peers from peers.conf into scp_targets table.

        Respects lazyload_peers_interval. Composite key (ae_title, host)
         -  multiple entries per AE (CIDR + exact) are preserved.
        """
        if self.lazyload_peers_interval > 0:
            elapsed = time.time() - self._last_peers_reload
            if elapsed < self.lazyload_peers_interval:
                return 0

        from clean_code.config import parse_peers_conf

        self._last_peers_reload = time.time()
        new_count = 0
        seen = set()
        with self._db.locked() as conn:
            for ae_title, host, port in parse_peers_conf(peers_file):
                seen.add((ae_title, host))
                row = conn.execute(
                    "SELECT host, port FROM scp_targets"
                    " WHERE ae_title = ? AND host = ?",
                    (ae_title, host),
                ).fetchone()
                existing = (row[0], row[1]) if row else None
                if existing is None or existing != (host, port):
                    now = int(time.time())
                    conn.execute(
                        "INSERT OR REPLACE INTO scp_targets"
                        " (ae_title, host, port, last_seen, consecutive_failures)"
                        " VALUES (?, ?, ?, ?, 0)",
                        (ae_title, host, port, now),
                    )
                    new_count += 1
            # A peer removed from peers.conf is gone from the operator's view,
            # so routing to the stale row was a destination the config no
            # longer names. Deleting by (ae_title, host) keeps a peer that is
            # still listed under a different port.
            stale = [
                (ae, host)
                for ae, host in conn.execute("SELECT ae_title, host FROM scp_targets")
                if (ae, host) not in seen
            ]
            for ae, host in stale:
                conn.execute(
                    "DELETE FROM scp_targets WHERE ae_title = ? AND host = ?",
                    (ae, host),
                )
            if stale:
                logging.info(
                    "[PEER] Removed %d peer(s) no longer in %s",
                    len(stale),
                    peers_file,
                )
            conn.commit()
        if new_count:
            logging.info(
                "[PEER] Reloaded %d new/updated peer(s) from %s", new_count, peers_file
            )
        return new_count

    # --- scp failure tracking ---

    def record_scp_failure(self, ae_title, host=None):
        """Record a failure to reach SCP.

        Auto-remove after 3 consecutive failures (CM-006).
        If host is provided, only updates that specific entry (subnet-safe).
        """
        with self._db.locked() as conn:
            if host:
                conn.execute(
                    "UPDATE scp_targets SET consecutive_failures ="
                    " consecutive_failures + 1"
                    " WHERE ae_title = ? AND host = ?",
                    (ae_title, host),
                )
                row = conn.execute(
                    "SELECT consecutive_failures FROM scp_targets"
                    " WHERE ae_title = ? AND host = ?",
                    (ae_title, host),
                ).fetchone()
                if row and row[0] >= self.scp_remove_failures:
                    conn.execute(
                        "DELETE FROM scp_targets" " WHERE ae_title = ? AND host = ?",
                        (ae_title, host),
                    )
                    logging.warning(
                        "[SCP] Removed %s@%s after %d consecutive failures",
                        ae_title,
                        host,
                        self.scp_remove_failures,
                    )
            else:
                # Backward compat: update all entries for this AE
                conn.execute(
                    "UPDATE scp_targets SET consecutive_failures ="
                    " consecutive_failures + 1 WHERE ae_title = ?",
                    (ae_title,),
                )
                # Check and auto-remove entries with >= N failures
                conn.execute(
                    "DELETE FROM scp_targets"
                    " WHERE ae_title = ? AND consecutive_failures >= ?",
                    (ae_title, self.scp_remove_failures),
                )
            conn.commit()

    def record_scp_success(self, ae_title, host=None):
        """Reset failure counter on successful connection (CM-006).
        If host is provided, only resets that specific entry (subnet-safe).
        """
        with self._db.locked() as conn:
            if host:
                conn.execute(
                    "UPDATE scp_targets SET consecutive_failures = 0"
                    " WHERE ae_title = ? AND host = ?",
                    (ae_title, host),
                )
            else:
                conn.execute(
                    "UPDATE scp_targets SET consecutive_failures = 0"
                    " WHERE ae_title = ?",
                    (ae_title,),
                )
            conn.commit()

    def close(self):
        """no-op  -  SharedDb owns the connection, closed in main()."""

    # --- C-MOVE-CANCEL support ---

    def set_job_cancel_callback(self, job_id, callback):
        """Store cancel callback for a job (BR-012)."""
        self._cancel_callbacks[job_id] = callback

    def is_job_cancelled(self, job_id):
        """Check if job was cancelled by SCU (BR-012)."""
        cb = self._cancel_callbacks.get(job_id)
        if cb:
            try:
                return cb()
            except Exception:
                return False
        return False

    def clear_job_callback(self, job_id):
        """Remove cancel callback after job completes (BR-012)."""
        self._cancel_callbacks.pop(job_id, None)
