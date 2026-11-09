# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""CommitTracker - async Storage Commitment resolution (N-EVENT-REPORT, Stage 2).

nginx-way: N-ACTION is fire-and-forget. A background worker resolves each
commitment batch once ALL referenced SOPs are stored OR the hard deadline
(default 300s) passes; unconfirmed SOPs at deadline are reported as failures
(EventTypeID=2) so the modality keeps its local copy.

Backpressure is structural (single worker, fire-and-forget dispatch inside the
DIMSE reactor) - no cap / 0xA700 is introduced.

Decoupled via three injected callables so this module is unit-testable without
  a live DICOM/HTTP environment:
  verify(sop_class, sop_uid) -> bool    stored/confirmed?
  report(host, port, ae, tx_uid, event_type, ok, failed)
  resolve(job) -> None                  how the batch becomes a verdict

The third is what the 2409 migration adds. It defaults to the local
heuristic this module has always used (unconfirmed SOPs -> 0x0110), so the
default is byte-for-byte today's behaviour; true-mode supplies a callable
that asks the ORIGIN once and reports the origin's own verdict verbatim.
"""

import json
import logging
import threading
import time

from clean_code.metrics import naction_total

DEFAULT_DEADLINE_SEC = 300.0
DEFAULT_RESOLVE_INTERVAL = 5.0

# Failure Reason (0008,1197) for an SOP never confirmed by the deadline:
# 0110H Processing Failure (PS3.3 C.14.1.1). 0111H is not an enumerated
# Failure Reason - it exists only as an N-CREATE status in PS3.7.
REASON_UNCONFIRMED = 0x0110


def _pair(item):
    """One `(sop_class, sop_uid)` tuple from a persisted JSON pair.

    A tuple, not a list: `pending` is a SET of these, so the elements have to
    be hashable, and the comparison against `job.sops` (built from tuples)
    would otherwise never match.
    """
    sop_class, sop_uid = item
    return (sop_class, sop_uid)


class CommitJob:
    """One N-ACTION Storage Commitment batch awaiting resolution."""

    __slots__ = (
        "calling_ae",
        "deadline",
        "host",
        "pending",
        "pending_persisted",
        "port",
        "sops",
        "tx_uid",
    )

    def __init__(self, tx_uid, calling_ae, host, port, sops, deadline):
        self.tx_uid = tx_uid
        self.calling_ae = calling_ae
        self.host = host
        self.port = port
        self.sops = list(sops)  # [(sop_class, sop_uid), ...]
        self.pending = set(sops)  # not-yet-confirmed (cls, uid)
        self.deadline = deadline
        # Size of `pending` as last written to the store. Starts equal to the
        # live set so a freshly registered batch is not rewritten on the first
        # tick for no reason.
        self.pending_persisted = len(self.pending)


class CommitTracker:
    """Tracks commitment batches; resolves and reports them asynchronously.

    Persistence (M7): a batch registered by an N-ACTION is a promise to the
    modality that an N-EVENT-REPORT will follow, and PS3.4 J.3.3.1 makes that
    report the result. Holding the promise only in `_jobs` meant a restart
    lost every outstanding batch: the transaction UID stayed active on the
    modality's side forever and the commitment never resolved. The jobs are
    therefore mirrored into a SQLite table, the same store the C-MOVE state
    machine already uses (`move_jobs.db`, via `SharedDb.locked()`), and a
    tracker constructed with a `db` reloads the pending rows before its loop
    starts.

    The dict stays the working set: it is what `_tick` iterates and what the
    tests inspect, and a row only exists to survive a process restart.
    """

    def __init__(
        self,
        verify,
        report,
        resolve=None,
        interval=DEFAULT_RESOLVE_INTERVAL,
        deadline_sec=DEFAULT_DEADLINE_SEC,
        shutdown=None,
        now=time.time,
        db=None,
    ):
        self._verify = verify  # verify(sop_class, sop_uid) -> bool
        self._report = report  # report(...)
        # resolve(job) -> None; None keeps the local heuristic below. The
        # callable owns sending the N-EVENT-REPORT for its batch.
        self._resolve_fn = resolve
        self._interval = interval
        self._deadline_sec = deadline_sec
        self._jobs = {}
        self._lock = threading.Lock()
        self._stop = shutdown if shutdown is not None else threading.Event()
        self._now = now  # injectable clock for tests
        self._db = db
        if db is not None:
            self._create_schema()
            self._reload()

    # ---- persistence -----------------------------------------------------

    def _create_schema(self):
        """Create the batch table.

        `pending` is stored as JSON rather than as a second table: the column
        is read and written whole, never queried by SOP, and a normalised
        table would add a join and a row-set to keep consistent for no
        question anyone asks. `sops` is stored whole too - the report needs
        the COMPLETE referenced set (J.3.3.1.2), so a batch is never resolved
        from a partial list.
        """
        with self._db.locked() as conn:
            conn.execute(
                "CREATE TABLE IF NOT EXISTS commit_jobs ("
                "  tx_uid TEXT PRIMARY KEY,"
                "  calling_ae TEXT NOT NULL,"
                "  host TEXT NOT NULL,"
                "  port INTEGER NOT NULL,"
                "  sops TEXT NOT NULL,"  # JSON [[cls, uid], ...]
                "  pending TEXT NOT NULL,"  # JSON [[cls, uid], ...]
                "  deadline REAL NOT NULL"
                ")"
            )
            conn.commit()

    def _reload(self):
        """Restore the pending batches left by a previous process.

        Rows are restored verbatim, INCLUDING their original deadline: a
        batch whose deadline passed while the bridge was down is resolved on
        the first tick, which is the correct answer - the modality is still
        waiting, and "everything unconfirmed by then" is what the deadline
        means. Rewriting the deadline on reload would extend a promise the
        SCP already made.
        """
        restored = 0
        with self._db.locked() as conn:
            rows = conn.execute(
                "SELECT tx_uid, calling_ae, host, port, sops, pending, deadline"
                " FROM commit_jobs"
            ).fetchall()
        for tx_uid, calling_ae, host, port, sops, pending, deadline in rows:
            try:
                sop_pairs = [_pair(item) for item in json.loads(sops)]
                pending_pairs = {_pair(item) for item in json.loads(pending)}
                job = CommitJob(
                    tx_uid,
                    calling_ae,
                    host,
                    int(port),
                    sop_pairs,
                    float(deadline),
                )
                job.pending = pending_pairs
                job.pending_persisted = len(pending_pairs)
            except (ValueError, TypeError) as e:
                # A row that cannot be read is not a reason to refuse the
                # whole reload: the other batches are still deliverable, and
                # this one is named in the log rather than silently dropped.
                logging.error(
                    "[COMMIT] tx=%s: unreadable persisted batch (%s) - skipped",
                    tx_uid,
                    e,
                )
                continue
            self._jobs[tx_uid] = job
            restored += 1
        if restored:
            logging.warning(
                "[COMMIT] recovered %d outstanding batch(es) from the previous "
                "session; the modality is still waiting for their verdicts",
                restored,
            )

    def _persist(self, job):
        if self._db is None:
            return
        with self._db.locked() as conn:
            conn.execute(
                "INSERT OR REPLACE INTO commit_jobs"
                " (tx_uid, calling_ae, host, port, sops, pending, deadline)"
                " VALUES (?, ?, ?, ?, ?, ?, ?)",
                (
                    job.tx_uid,
                    job.calling_ae,
                    job.host,
                    int(job.port),
                    json.dumps([list(item) for item in job.sops]),
                    json.dumps(sorted(list(item) for item in job.pending)),
                    float(job.deadline),
                ),
            )
            conn.commit()

    def _forget(self, tx_uid):
        if self._db is None:
            return
        with self._db.locked() as conn:
            conn.execute("DELETE FROM commit_jobs WHERE tx_uid = ?", (tx_uid,))
            conn.commit()

    def register(self, tx_uid, calling_ae, host, port, sops):
        """Register an N-ACTION batch for async resolution. Fire-and-forget."""
        deadline = self._now() + self._deadline_sec
        job = CommitJob(tx_uid, calling_ae, host, port, sops, deadline)
        with self._lock:
            self._jobs[tx_uid] = job
        self._persist(job)

    # ---- resolution ------------------------------------------------------

    def _resolve(self, job):
        """Send one N-EVENT-REPORT for the batch.

        The mode branch is here, at the single point where a batch becomes a
        verdict: a supplied `resolve` callable owns the whole resolution
        (true-mode asks the origin and reports its answer), and the default
        is the local heuristic that has always run.

        Returns True when the verdict was delivered. A failed delivery must NOT
        retire the job: the modality was never told the outcome, so discarding
        it lost the commitment silently (PS3.4 J.3.3.1 - the N-EVENT-REPORT IS
        the result).
        """
        if self._resolve_fn is not None:
            try:
                self._resolve_fn(job)
                return True
            except Exception as e:
                logging.error("[COMMIT] resolve for tx=%s failed: %s", job.tx_uid, e)
                return False

        ok = []
        failed = []
        for sop_class, sop_uid in job.sops:
            if (sop_class, sop_uid) in job.pending:
                failed.append((sop_class, sop_uid, REASON_UNCONFIRMED))
            else:
                ok.append((sop_class, sop_uid))
        event_type = 1 if not failed else 2  # success / complete-with-failures
        # The verdict counters belong here, not in the handler: this is where
        # a SOP actually acquires a verdict. The handler answers 0x0000
        # without checking anything (PS3.4 J.3.2), so counting there would
        # report a verdict the bridge had no evidence for at that instant.
        try:
            if ok:
                naction_total.labels(verdict="success").inc(len(ok))
            if failed:
                naction_total.labels(verdict="failed").inc(len(failed))
        except Exception:
            pass
        try:
            delivered = self._report(
                job.host, job.port, job.calling_ae, job.tx_uid, event_type, ok, failed
            )
        except Exception as e:
            logging.error("[COMMIT] report for tx=%s failed: %s", job.tx_uid, e)
            return False
        return delivered is not False

    def _tick(self):
        with self._lock:
            jobs = list(self._jobs.values())
        now = self._now()
        for job in jobs:
            for sop_item in list(job.pending):
                sop_class, sop_uid = sop_item
                try:
                    if self._verify(sop_class, sop_uid):
                        job.pending.discard(sop_item)
                except Exception as e:
                    logging.warning("[COMMIT] verify %s error: %s", sop_uid, e)
            if not job.pending or now >= job.deadline:
                # Retire the job only once its verdict was actually delivered:
                # dropping it after a failed N-EVENT-REPORT lost the outcome
                # the modality was waiting for.
                if self._resolve(job):
                    with self._lock:
                        self._jobs.pop(job.tx_uid, None)
                    # Drop the row only after the verdict left the process:
                    # a row deleted before the report would lose the batch on
                    # a crash between the two, which is the failure this
                    # persistence exists to prevent.
                    self._forget(job.tx_uid)
                else:
                    logging.warning(
                        "[COMMIT] tx=%s kept for another attempt: the verdict "
                        "was not delivered",
                        job.tx_uid,
                    )
            elif self._db is not None:
                # Confirmations shrink `job.pending` and that set is what a
                # restart reads back. Rewriting the row on EVERY tick would put
                # a commit on the tracker's own timer for a batch that may sit
                # for the full deadline; the row is written when the set
                # actually shrank, which is the only state change here that a
                # restart can observe.
                if len(job.pending) != job.pending_persisted:
                    job.pending_persisted = len(job.pending)
                    self._persist(job)

    # ---- lifecycle -------------------------------------------------------

    def run(self):
        """Background loop; resolve batches on an interval. Blocks until close."""
        while not self._stop.is_set():
            try:
                self._tick()
            except Exception as e:
                logging.error("[COMMIT] tick error: %s", e)
            self._stop.wait(self._interval)

    def pending_count(self):
        with self._lock:
            return len(self._jobs)

    def close(self):
        self._stop.set()
