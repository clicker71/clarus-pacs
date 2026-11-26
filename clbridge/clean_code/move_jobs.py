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
import sqlite3
import threading
import time

from clean_code.utils import normalize_host


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
    # PS3.4 C.4.2.1.2: the C-MOVE-RQ names the SCOPE (STUDY/SERIES/IMAGE) and
    # the UIDs that scope it. The level and UIDs live on the job because the
    # worker thread outlives the association that queued the work. An empty
    # level means STUDY (the historical behaviour), so an existing row keeps
    # the meaning it had.
    if "request_level" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN request_level TEXT NOT NULL DEFAULT ''"
        )
    if "request_series_uid" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN request_series_uid TEXT"
            " NOT NULL DEFAULT ''"
        )
    if "request_sop_uid" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN request_sop_uid TEXT NOT NULL DEFAULT ''"
        )
    # The peers.conf KEY the destination was matched under. For a CIDR entry
    # that is the subnet ("192.168.1.0/24"), not the client IP the association
    # actually goes to: `scp_targets` is keyed by that configured host, so
    # recording success/failure against the client IP updated no row and a
    # subnet-backed peer never accumulated (or cleared) its failure streak.
    if "registry_host" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN registry_host TEXT NOT NULL DEFAULT ''"
        )
    # SOP instance UIDs this job has DELIVERED, newline-joined, written as the
    # peer acknowledges each one. The pass's returned list is the other carrier
    # and a raising pass never produces it, so without this column a mid-stream
    # exception wiped the resume skip set and the resumed pass re-sent instances
    # the destination already held.
    if "delivered_sops" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN delivered_sops TEXT NOT NULL DEFAULT ''"
        )

    # Instances the destination ACCEPTED with a C-STORE Warning (PS3.4 B.2.3.1)
    # across every pass. They are delivered, so they are not in failed_sops -
    # but Table C.4-2 gives "sub-operations completed with warnings" its own
    # status (0xB000), and the reactor builds that response, not the worker.
    if "warning_count" not in cols:
        conn.execute(
            "ALTER TABLE move_jobs ADD COLUMN warning_count INTEGER NOT NULL DEFAULT 0"
        )

    # One row per DELIVERED SOP. The `delivered_sops` TEXT column above holds
    # the same information as a newline-joined blob, and each acknowledgement
    # read it, split it, appended and wrote the WHOLE value back under the
    # shared DB lock: a 5000-instance study did 12.5 M string copies while
    # blocking every progress poll and every other worker's claim. A child
    # table makes one acknowledgement one INSERT, and the primary key makes a
    # duplicate acknowledgement idempotent without a read-modify-write.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS move_job_delivered ("
        "  job_id INTEGER NOT NULL,"
        "  sop_uid TEXT NOT NULL,"
        "  sop_class TEXT NOT NULL DEFAULT '',"
        "  PRIMARY KEY (job_id, sop_uid)"
        ")"
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

    # Migrate: create v2, copy data, drop v1, rename.
    #
    # Crash-safe (each step is idempotent on its own):
    #   * `scp_targets_v2` may already exist from a crashed run - IF NOT EXISTS
    #     turns the leftover into a no-op instead of "table already exists";
    #   * the copy is INSERT OR IGNORE, so re-running after a partial copy adds
    #     only the rows that are still missing;
    #   * DROP + RENAME happen together, so a crash between them leaves either
    #     the old table intact (safe: the migration runs again) or v2 already
    #     named `scp_targets` (safe: the PK check above returns early);
    #   * a leftover `scp_targets_v2` from a crash AFTER the rename cannot
    #     exist, because RENAME removes it.
    # Before this, a crash after the CREATE left `scp_targets_v2` behind, and
    # the next start died on "table scp_targets_v2 already exists" - a bridge
    # that could not open its own database.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS scp_targets_v2 ("
        "  ae_title TEXT NOT NULL,"
        "  host TEXT NOT NULL,"
        "  port INTEGER NOT NULL DEFAULT 104,"
        "  last_seen INTEGER NOT NULL,"
        "  consecutive_failures INTEGER DEFAULT 0,"
        "  PRIMARY KEY (ae_title, host)"
        ")"
    )
    # The v1 table may be gone already (a crash between DROP and RENAME): the
    # copy then has nothing to read, which is not an error.
    try:
        conn.execute(
            "INSERT OR IGNORE INTO scp_targets_v2"
            " SELECT ae_title, host, port, last_seen, consecutive_failures"
            " FROM scp_targets"
        )
    except sqlite3.OperationalError:
        logging.debug("[MOVE-JOBS] scp_targets_v1 already migrated away")
    conn.execute("DROP TABLE IF EXISTS scp_targets")
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
                "  warning_count INTEGER NOT NULL DEFAULT 0,"
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
        request_level="",
        request_series_uid="",
        request_sop_uid="",
        registry_host="",
        claimable=True,
    ):
        """Create a new pending C-MOVE job. Returns job id.

        target_host/target_port: resolved Move Destination address.
        Stored so worker doesn't need to re-resolve (subnet support).
        expose_rejected: the job was queued over the QA AE; the worker queries
        the configured `[dicomweb] qa_url` endpoint instead of `clarus_url`.
        Stored because the worker outlives the association.
        request_level/request_series_uid/request_sop_uid: the Query/Retrieve
        scope of the request (PS3.4 C.4.2.1.2), stored for the same reason -
        a SERIES-level C-MOVE must not retrieve the whole study.
        registry_host: the peers.conf key the destination matched under. For a
        CIDR entry this is the SUBNET while `target_host` is the client IP the
        association goes to; SCP health is recorded against the key.
        claimable: when False the row is inserted as 'held', which no worker
        picks. A multi-scope request publishes all of its jobs this way and
        then calls `release_held_jobs()`, so no worker can start the first
        scope while the handler is still submitting the rest (a second insert
        that then failed left a job already sending while the SCU got an
        error, and the worker could observe `is_job_cancelled()` as False
        before the cancel callback was installed).
        """
        now = int(time.time())
        initial_status = "pending" if claimable else "held"
        with self._db.locked() as conn:
            cur = conn.execute(
                "INSERT INTO move_jobs"
                " (study_uid, move_dest_ae, target_host, target_port,"
                "  status, created_at, updated_at, expose_rejected,"
                "  request_level, request_series_uid, request_sop_uid,"
                "  registry_host)"
                " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    study_uid,
                    move_dest_ae,
                    target_host,
                    target_port,
                    initial_status,
                    now,
                    now,
                    1 if expose_rejected else 0,
                    str(request_level or ""),
                    str(request_series_uid or ""),
                    str(request_sop_uid or ""),
                    str(registry_host or target_host or ""),
                ),
            )
            conn.commit()
            job_id = cur.lastrowid
        # NOTIFY AFTER the db lock is released. `pick_pending_job` takes
        # `_cond` and then `_db` (it must, to make its empty-check and wait
        # atomic against this notify); notifying while still holding `_db`
        # made this path `_db -> _cond`, the exact opposite order. A worker
        # holding `_cond` waiting for `_db` and a reactor holding `_db`
        # waiting for `_cond` deadlock the whole C-MOVE subsystem, including
        # the metrics scrape. The wakeup is only a hint, so releasing first
        # costs nothing: a worker that cannot see the new row yet simply
        # re-selects under `_cond`.
        with self._cond:
            self._cond.notify()
        return job_id

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

    def release_held_jobs(self, job_ids):
        """Make `held` rows claimable. Returns how many were released.

        The publication step of a multi-scope C-MOVE request: every scope is
        inserted as 'held' (see `create_job`), the request's cancellation
        state is installed, and only then are all of them made pending
        together. A single UPDATE is the barrier - no worker can observe a
        partial set, and no scope can start before the request is ready.
        """
        ids = [int(i) for i in job_ids if i is not None]
        if not ids:
            return 0
        placeholders = ",".join("?" for _ in ids)
        with self._db.locked() as conn:
            cur = conn.execute(
                f"UPDATE move_jobs SET status = 'pending'"
                f" WHERE status = 'held' AND id IN ({placeholders})",
                ids,
            )
            conn.commit()
            released = cur.rowcount or 0
        if released:
            with self._cond:
                self._cond.notify_all()
        return released

    def _select_pending(self):
        """The pending-row SELECT used by `pick_pending_job`.

        Split out so the caller can run it while holding `self._cond`, which
        is what makes the empty-check/wait pair atomic against `insert`'s
        notify.
        """
        with self._db.locked() as conn:
            return conn.execute(
                "SELECT id, study_uid, move_dest_ae, attempts,"
                " target_host, target_port"
                " FROM move_jobs WHERE status = 'pending'"
                " ORDER BY created_at ASC"
            ).fetchall()

    def pick_pending_job(self, timeout=5.0, skip=None, reserve=None, release=None):
        """Atomically pick one pending job, optionally RESERVING its peer.

        `reserve(host, port, ae) -> bool` is a non-blocking claim of the peer
        (a per-peer gate in the worker). It runs while the pick is still a
        candidate, BEFORE the row is claimed: a peer that cannot be reserved is
        skipped, so the caller never holds a claimed job it cannot start. That
        is what keeps two workers from claiming two jobs for the same peer and
        leaving a job for a DIFFERENT peer unserved behind a blocking wait.

        A candidate that is claimed but then found gone (another worker won the
        race) releases its reservation through `release`, so the gate is not
        leaked by the losing thread.

        `skip(host, port, ae) -> bool` remains as an advisory filter.

        Returns dict or None.
        """
        while True:
            # The empty-check and the wait must be ONE atomic step against the
            # notifier. `insert` wakes a waiter under `self._cond`; taking the
            # same lock around the SELECT closes the lost-wakeup window where a
            # row was inserted after an empty SELECT and before the wait, which
            # left the worker asleep with pending work on disk until the
            # timeout expired.
            with self._cond:
                rows = self._select_pending()
                if skip is not None:
                    rows = [
                        r for r in rows if not skip(r[4] or "", r[5] or 0, r[2] or "")
                    ]
                if not rows:
                    if not self._cond.wait(timeout=timeout):
                        return None
                    continue

            claimed = None
            for row in rows:
                if reserve is not None and not reserve(
                    row[4] or "", row[5] or 0, row[2] or ""
                ):
                    # Another worker holds this peer: leave the row pending and
                    # look at the next one, so a job for a different peer is
                    # still served by THIS worker.
                    continue
                # The claim can RAISE - a SQLite error while the row is being
                # updated - and an exception escaping here skipped `release`,
                # leaking the peer reservation for the life of the process. The
                # leaked gate then made every other worker skip that peer, and
                # because the raise also escaped the worker loop's SELECT the
                # worker itself died. Every unsuccessful claim therefore
                # releases, and a claim error is retried against the next
                # candidate instead of killing the worker.
                try:
                    claimed = self._claim_row(row[0])
                except Exception as claim_err:
                    logging.error(
                        "[MOVE-JOBS] claim of job %s failed (%s) - releasing "
                        "the peer reservation and trying the next candidate",
                        row[0],
                        claim_err,
                    )
                    claimed = None
                    if release is not None:
                        try:
                            release(row[4] or "", row[5] or 0, row[2] or "")
                        except Exception:
                            pass
                    continue
                if claimed is not None:
                    return claimed
                # Lost the claim race: hand the reservation back.
                if release is not None:
                    release(row[4] or "", row[5] or 0, row[2] or "")
            # Every candidate was either busy or lost: wait like an empty queue.
            with self._cond:
                if not self._cond.wait(timeout=timeout):
                    return None

    def job_has_live_worker(self, job_id):
        """True when `job_id` may still be read by a worker.

        That is 'in_progress' (a worker owns it now) AND 'pending' (no worker
        has claimed it yet, but one can, and it must observe a cancel that was
        latched before it started). Only a TERMINAL job has nobody left to
        read the latch: installing one there leaves a callback in the registry
        that no worker would ever clear.
        """
        try:
            with self._db.locked() as conn:
                row = conn.execute(
                    "SELECT status FROM move_jobs WHERE id = ?", (job_id,)
                ).fetchone()
            return bool(row) and row[0] in ("pending", "in_progress")
        except Exception:
            return False

    def _claim_row(self, job_id):
        """Claim `job_id` for this worker, or None when another worker won it."""
        now = int(time.time())
        with self._db.locked() as conn:
            row2 = conn.execute(
                "SELECT id, study_uid, move_dest_ae, attempts,"
                " target_host, target_port, expose_rejected,"
                " request_level, request_series_uid, request_sop_uid,"
                " registry_host"
                " FROM move_jobs WHERE status = 'pending' AND id = ?",
                (job_id,),
            ).fetchone()
            if not row2:
                return None  # Another worker claimed it

            conn.execute(
                "UPDATE move_jobs SET status = 'in_progress',"
                " updated_at = ?, attempts = attempts + 1"
                " WHERE id = ? AND status = 'pending'",
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
                # PS3.4 C.4.2.1.2: the scope travels with the job for the
                # same reason - the worker no longer has the event.
                "request_level": str(row2[7] or ""),
                "request_series_uid": str(row2[8] or ""),
                "request_sop_uid": str(row2[9] or ""),
                # The peers.conf key, NOT the connection address: for a
                # CIDR peer the two differ and SCP health is keyed by the
                # former.
                "registry_host": str(row2[10] or row2[4] or ""),
            }

    def requeue_job_without_retry(self, job_id):
        """Return a claimed job to pending without charging it an attempt (R9).

        The worker claims a job and THEN takes the per-peer gate, so with more
        than one worker a job could be claimed for a peer another worker was
        already streaming to. It was then invisible in `in_progress/` for the
        whole of that transfer. This hands it back so other workers can see it;
        `attempts` is decremented because the claim already incremented it and
        nothing was attempted - otherwise a busy peer would burn the job's
        retry budget without a single C-STORE.

        Only a job still in `in_progress` for this claim is released: a job the
        reaper or a cancel already moved is not ours to rewrite.
        """
        now = int(time.time())
        with self._db.locked() as conn:
            cur = conn.execute(
                "UPDATE move_jobs SET status = 'pending', attempts = "
                "CASE WHEN attempts > 0 THEN attempts - 1 ELSE 0 END,"
                " updated_at = ? WHERE id = ? AND status = 'in_progress'",
                (now, job_id),
            )
            conn.commit()
            changed = cur.rowcount or 0
        if changed:
            with self._cond:
                self._cond.notify()
        return bool(changed)

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

    def add_job_delivered_sop(self, job_id, sop_uid, sop_class=""):
        """Record one DELIVERED SOP instance (and its class) for the job.

        Written as the peer acknowledges each instance, not at the end of the
        pass: the pass's own list is lost when it raises, and the resume uses
        this record to skip what has already arrived.

        ONE INSERT into the child table. `INSERT OR IGNORE` keyed on
        (job_id, sop_uid) makes a duplicate acknowledgement idempotent without
        reading the previous value, so an acknowledgement costs the same
        whether the study holds ten instances or ten thousand.
        """
        if not sop_uid:
            return
        with self._db.locked() as conn:
            row = conn.execute(
                "SELECT id FROM move_jobs WHERE id = ?", (job_id,)
            ).fetchone()
            if row is None:
                # The job row was already retired; recording a delivery for a
                # row that no longer exists would resurrect nothing.
                return
            conn.execute(
                "INSERT OR IGNORE INTO move_job_delivered"
                " (job_id, sop_uid, sop_class) VALUES (?, ?, ?)",
                (job_id, str(sop_uid), str(sop_class or "")),
            )
            conn.commit()

    def get_job_delivered_sops(self, job_id):
        """`[(sop_class, sop_uid), ...]` recorded as delivered for this job."""
        with self._db.locked() as conn:
            rows = conn.execute(
                "SELECT sop_class, sop_uid FROM move_job_delivered"
                " WHERE job_id = ? ORDER BY rowid",
                (job_id,),
            ).fetchall()
        return [(str(r[0] or ""), str(r[1])) for r in rows]

    def set_job_failed_sops(self, job_id, uids):
        """Store the (0008,0058) Failed SOP Instance UID List for a job.

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

    def set_job_warning_count(self, job_id, count):
        """Record how many instances the destination accepted WITH a warning.

        PS3.4 B.2.3.1: a C-STORE Warning means the SCP stored the instance, so
        those UIDs are delivered and never belong in the failed tail. They are
        still not plain successes: PS3.4 Table C.4-2 reserves 0xB000 for
        "sub-operations completed with warnings", so the tally has to survive
        the worker that computed it - the reactor builds the final response.
        """
        try:
            value = max(0, int(count or 0))
        except (TypeError, ValueError):
            value = 0
        now = int(time.time())
        with self._db.locked() as conn:
            conn.execute(
                "UPDATE move_jobs SET warning_count = ?, updated_at = ? WHERE id = ?",
                (value, now, job_id),
            )
            conn.commit()

    def get_job_warning_count(self, job_id):
        """The warning tally recorded by `set_job_warning_count`. 0 if unset."""
        try:
            with self._db.locked() as conn:
                row = conn.execute(
                    "SELECT warning_count FROM move_jobs WHERE id = ?", (job_id,)
                ).fetchone()
            return int(row[0]) if row and row[0] else 0
        except Exception:
            return 0

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

    def _lookup_target(self, ae_title, host):
        """(host, port) stored for the exact (ae_title, host) key, or None.

        Read in its own lock: `reload_peers_from_file` calls this BEFORE
        delegating the upsert to `register_peer`, which acquires the same
        lock, so the existing row cannot be read while the upsert lock is held.
        """
        with self._db.locked() as conn:
            row = conn.execute(
                "SELECT host, port FROM scp_targets WHERE ae_title = ? AND host = ?",
                (ae_title, host),
            ).fetchone()
        return (row[0], row[1]) if row else None

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

        # Priority 1: exact address match (non-CIDR). Both sides are
        # normalised so `VIEWER localhost 11112` matches a request from
        # 127.0.0.1, and `::1` matches `[::1]`; a raw string compare made the
        # operator see "Unknown Move Destination" (A801) instead of a match.
        # CIDR entries are untouched here and only treated as networks below,
        # so normalisation cannot promote a subnet to an exact hit.
        want = normalize_host(client_ip)
        for host, port in rows:
            if "/" not in host and normalize_host(host) == want:
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

        Per-entry upsert delegates to `register_peer`, the single INSERT path:
        the reload used to run its own INSERT OR REPLACE here, so the two
        write paths drifted (register_peer's "one exact entry per AE"
        semantics were not applied by the file path). The file-sync half
        (removing peers the file no longer names) stays here.
        """
        if self.lazyload_peers_interval > 0:
            elapsed = time.time() - self._last_peers_reload
            if elapsed < self.lazyload_peers_interval:
                return 0

        from clean_code.config import parse_peers_conf

        self._last_peers_reload = time.time()
        parsed = list(parse_peers_conf(peers_file))
        if not parsed:
            # An EMPTY parse result must not delete anything: a file mid-save,
            # a truncated write or a crash leaves a config that parses to zero
            # peers, and treating that as "every peer was removed" silently
            # unregistered the whole peer table.
            logging.warning(
                "[PEER] %s parsed to no peers - keeping the existing table "
                "(a truncated or empty file must not wipe routing)",
                peers_file,
            )
            return 0

        seen = set()
        new_count = 0
        for ae_title, host, port in parsed:
            seen.add((ae_title, host))
            existing = self._lookup_target(ae_title, host)
            if existing == (host, port):
                # Unchanged entry: leave the row ALONE. `register_peer` writes
                # `consecutive_failures = 0`, so re-registering an identical
                # peer wiped its failure streak - a peer that had failed twice
                # (one short of the eviction threshold) started again from
                # zero at every reload, so the configured threshold was never
                # reached and a dead destination kept receiving jobs.
                continue
            new_count += 1
            self.register_peer(ae_title, host, port)

        # A peer removed from peers.conf is gone from the operator's view,
        # so routing to the stale row was a destination the config no
        # longer names. Deleting by (ae_title, host) keeps a peer that is
        # still listed under a different port.
        with self._db.locked() as conn:
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
