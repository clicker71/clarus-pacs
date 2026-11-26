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

# Wall-clock budget for ONE batch's verification walk inside a single tick.
# `verify()` may be a round trip to the origin, and the tracker has exactly one
# worker: an unbounded walk over a large batch delayed every other peer past
# its own deadline. 30 s is well inside the default 300 s batch deadline, so a
# healthy origin never hits it, while a sick one costs at most this much before
# the remaining SOPs are reported as unconfirmed.
DEFAULT_VERIFY_BUDGET_SEC = 30.0

# Delivery attempts per batch before the verdict is abandoned. One attempt is
# not enough (the peer may be restarting) and an unbounded retry is a DoS: the
# tracker's single worker ticks every `interval`, and each N-EVENT-REPORT burns
# acse + network + dimse timeouts before it fails, so one dead peer starves
# every other batch FOREVER. 5 attempts at the default 5 s interval covers
# roughly half a minute of a restarting peer; PS3.4 J.3.3.1 makes the report
# the result, so a batch that cannot be reported is abandoned loudly rather
# than retried into the heat death of the universe.
MAX_REPORT_ATTEMPTS = 5

# Failure Reason (0008,1197) for an SOP never confirmed by the deadline:
# 0110H Processing Failure (PS3.3 C.14.1.1). 0111H is not an enumerated
# Failure Reason - it exists only as an N-CREATE status in PS3.7.
REASON_UNCONFIRMED = 0x0110

# R10: how long an ABANDONED row is kept for inspection before it is deleted.
# An abandoned verdict has already cost MAX_REPORT_ATTEMPTS associations and
# can never be delivered, so keeping it forever only grows the table and the
# inspection list with every dead peer. A day is long enough to read the
# ERROR line that came with it; `abandoned_at` is what the sweep measures
# against, so the retention is independent of the job's own deadline.
ABANDONED_RETENTION_SEC = 24 * 3600.0


def _pair(item):
    """One `(sop_class, sop_uid)` tuple from a persisted JSON pair.

    A tuple, not a list: `pending` is a SET of these, so the elements have to
    be hashable, and the comparison against `job.sops` (built from tuples)
    would otherwise never match.
    """
    sop_class, sop_uid = item
    return (sop_class, sop_uid)


def _verdict_to_json(verdict):
    """Serialise a frozen verdict for the row, or None.

    Lists, not tuples: JSON has no tuple, and the shapes are rebuilt by
    `_verdict_from_json`.
    """
    if verdict is None:
        return None
    ok, failed, event_type = verdict
    return [[list(item) for item in ok], [list(item) for item in failed], event_type]


def _verdict_from_json(raw):
    """Rebuild a frozen verdict from its row form (None when unset/unreadable)."""
    if not raw:
        return None
    try:
        data = json.loads(raw)
        ok = [_pair(item) for item in data[0]]
        # A failed entry is (class, uid, reason) - the reason is an int.
        failed = [(item[0], item[1], item[2]) for item in data[1]]
        return (ok, failed, int(data[2]))
    except (ValueError, TypeError, IndexError, KeyError):
        return None


class CommitJob:
    """One N-ACTION Storage Commitment batch awaiting resolution."""

    __slots__ = (
        "abandon_pending",
        "abandoned",
        "abandoned_at",
        "calling_ae",
        "cleanup_pending",
        "deadline",
        "frozen_verdict",
        "host",
        "pending",
        "pending_persisted",
        "port",
        "report_attempts",
        "sops",
        "tx_uid",
        "verdict_persisted",
        "verify_cursor",
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
        # N-EVENT-REPORT delivery attempts so far (see MAX_REPORT_ATTEMPTS).
        self.report_attempts = 0
        # CC3: the verdict decided on the FIRST delivery attempt, as
        # `(ok, failed, event_type)`. Once set it is what every retry reports:
        # the outcome may not change between attempts.
        self.frozen_verdict = None
        # Whether the row on disk actually carries `frozen_verdict`. A tuple
        # cannot be "not None and unsaved", so the flag is what makes a FAILED
        # verdict write retryable: without it the next tick skipped the write
        # because the verdict was already set, and a restart rebuilt a
        # different verdict from the old `pending` set.
        self.verdict_persisted = False
        # R8: set when the report budget is exhausted and the verdict can never
        # leave this process. Persisted, because a reload that treats an
        # abandoned row as active grants the dead peer another full round of
        # delivery attempts at EVERY restart, on the single commit worker.
        self.abandoned = False
        # R10: when the row became abandoned (retention reference point).
        self.abandoned_at = 0.0
        # Where the verification walk resumes. `pending` is a SET, so `list()`
        # over it is not a stable order: a batch whose head never confirms
        # re-probed the same SOPs every tick and the tail never got a turn.
        # The cursor is the uid the next tick starts AT; it wraps to the head
        # once the sorted order is exhausted, so an unconfirmed SOP is retried
        # after every pending entry has had one.
        self.verify_cursor = None
        # The ABANDONED state has not reached the row yet (the write raised).
        # The job is out of the working set but must not be eligible for
        # retention: a restart would reload the row as ACTIVE and send the
        # verdict the bridge already gave up on.
        self.abandon_pending = False
        # The verdict WAS delivered and the row delete failed. Kept out of the
        # working set (it must never be reported again) and retried from the
        # cleanup list, so a restart does not re-send a verdict the modality
        # already received.
        self.cleanup_pending = False


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
        verify_budget_sec=DEFAULT_VERIFY_BUDGET_SEC,
    ):
        self._verify = verify  # verify(sop_class, sop_uid) -> bool
        self._report = report  # report(...)
        # resolve(job) -> None; None keeps the local heuristic below. The
        # callable owns sending the N-EVENT-REPORT for its batch.
        self._resolve_fn = resolve
        self._interval = interval
        self._deadline_sec = deadline_sec
        # CC2: wall-clock budget for ONE job's verification walk in a tick.
        # Each `_verify` can be a network round trip to the origin; without a
        # budget a large batch held the single commit worker past every other
        # peer's deadline.
        self._verify_budget_sec = verify_budget_sec
        self._jobs = {}
        # R10: abandoned batches, kept OUT of the working set. They are never
        # ticked and never counted as pending - only inspected - so keeping
        # them in `_jobs` made `pending_count()` grow for every dead peer.
        # Bounded by `purge_abandoned`.
        self._abandoned = {}
        # Delivered batches whose row delete failed, and batches whose
        # abandonment write failed. Kept out of `_jobs` because they must never
        # be reported again - only their ROW needs another attempt.
        self._cleanup = {}
        self._abandon_pending = {}
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
                "  deadline REAL NOT NULL,"
                "  frozen TEXT,"  # JSON [ok, failed, event_type] or NULL
                "  abandoned INTEGER NOT NULL DEFAULT 0,"
                "  abandoned_at REAL NOT NULL DEFAULT 0,"
                "  report_attempts INTEGER NOT NULL DEFAULT 0,"
                "  cleanup_pending INTEGER NOT NULL DEFAULT 0"
                ")"
            )
            conn.commit()
        # A table created before these columns existed gains them in place.
        # Without `frozen` a restart rebuilt a possibly different verdict and
        # reported it as if it were the one already sent; without `abandoned`
        # every reload treated a dead peer's parked batch as active and gave
        # it five more delivery rounds on the single worker at every restart.
        for column, ddl in (
            ("frozen", "TEXT"),
            ("abandoned", "INTEGER NOT NULL DEFAULT 0"),
            ("abandoned_at", "REAL NOT NULL DEFAULT 0"),
            # Without the attempt count a restart handed a dead peer a fresh
            # MAX_REPORT_ATTEMPTS budget, on the single commit worker.
            ("report_attempts", "INTEGER NOT NULL DEFAULT 0"),
            # A delivered verdict whose row delete failed has to stay
            # findable, or a restart re-sends a report the modality already
            # received.
            ("cleanup_pending", "INTEGER NOT NULL DEFAULT 0"),
        ):
            try:
                with self._db.locked() as conn:
                    conn.execute(f"ALTER TABLE commit_jobs ADD COLUMN {column} {ddl}")
                    conn.commit()
            except Exception:
                # The column already exists (the normal case after the first
                # upgrade). Any other failure surfaces on the first read/write.
                pass

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
        abandoned = 0
        cleaned = 0
        with self._db.locked() as conn:
            rows = conn.execute(
                "SELECT tx_uid, calling_ae, host, port, sops, pending, deadline,"
                " frozen, abandoned, abandoned_at, report_attempts, cleanup_pending"
                " FROM commit_jobs"
            ).fetchall()
        for (
            tx_uid,
            calling_ae,
            host,
            port,
            sops,
            pending,
            deadline,
            frozen,
            abandoned_flag,
            abandoned_at,
            report_attempts,
            cleanup_flag,
        ) in rows:
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
                # R10: the verdict already decided travels with the row, so a
                # restart re-sends the SAME result instead of computing a new
                # one that may contradict what the SCM was told.
                job.frozen_verdict = _verdict_from_json(frozen)
                job.verdict_persisted = job.frozen_verdict is not None
                if cleanup_flag:
                    # The verdict was DELIVERED and only the row delete failed.
                    # Reloading it as active re-sent a report the modality
                    # already acknowledged; the row is a cleanup chore, not a
                    # commitment, so it goes straight to the retry list.
                    job.cleanup_pending = True
                    self._cleanup[job.tx_uid] = job
                    cleaned += 1
                    continue
                if abandoned_flag:
                    # R10: an abandoned row stays abandoned. Reloading it as
                    # active would grant the dead peer another MAX_REPORT_ATTEMPTS
                    # rounds on the single commit worker at every restart; it
                    # would also be counted as pending forever.
                    job.abandoned = True
                    job.report_attempts = MAX_REPORT_ATTEMPTS
                    # R10: the retention clock travels with the row; without
                    # it a reloaded batch read as age 0 and was never purged.
                    job.abandoned_at = float(abandoned_at or 0.0)
                    job.report_attempts = max(
                        int(report_attempts or 0), MAX_REPORT_ATTEMPTS
                    )
                    self._abandoned[tx_uid] = job
                    abandoned += 1
                    continue
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
            # The attempt count survives the restart. Restoring it as zero gave
            # an unavailable peer a fresh MAX_REPORT_ATTEMPTS budget at every
            # restart of an otherwise healthy bridge.
            job.report_attempts = max(0, int(report_attempts or 0))
            restored += 1
        if restored:
            logging.warning(
                "[COMMIT] recovered %d outstanding batch(es) from the previous "
                "session; the modality is still waiting for their verdicts",
                restored,
            )
        if abandoned:
            logging.error(
                "[COMMIT] %d recovered batch(es) were already abandoned - their "
                "verdicts can no longer be delivered and are kept for inspection",
                abandoned,
            )
        if cleaned:
            logging.warning(
                "[COMMIT] %d recovered batch(es) were delivered but their row "
                "delete failed - retrying the delete, NOT the report",
                cleaned,
            )

    def purge_abandoned(self, max_age_sec=ABANDONED_RETENTION_SEC, now=None):
        """Delete abandoned rows older than `max_age_sec`; return the count.

        R10: nothing removed an abandoned row, so the table and the inspection
        list grew with every dead peer. The row's own deadline is NOT the
        reference point - it may be days old when a long pause finally exhausts
        the report budget - so the age is measured from `abandoned_at`, which
        `_park_undelivered` stamps.
        """
        if self._db is None:
            return 0
        cutoff = (self._now() if now is None else now) - max_age_sec
        removed = 0
        with self._lock:
            # A job whose abandonment write never landed is NOT in `_abandoned`
            # and its row still says active. If it were swept by age here the
            # verdict's only remaining trace would be gone while a restart
            # could still re-deliver it, so it is excluded by construction:
            # only `_abandoned` entries are candidates, and `_retry_settled_rows`
            # moves a job there once the write succeeds.
            stale = [
                tx
                for tx, job in self._abandoned.items()
                if getattr(job, "abandoned_at", 0.0) <= cutoff
            ]
        for tx in stale:
            try:
                with self._db.locked() as conn:
                    cur = conn.execute(
                        "DELETE FROM commit_jobs" " WHERE tx_uid = ? AND abandoned = 1",
                        (tx,),
                    )
                    conn.commit()
                    removed += cur.rowcount or 0
                # Drop the in-memory entry only AFTER the row is gone. The
                # old order deleted it first, so a failed DELETE left the row
                # on disk with nobody to find it on the next sweep.
                with self._lock:
                    self._abandoned.pop(tx, None)
            except Exception as e:
                # A failed delete leaves the row AND the in-memory entry for
                # the next sweep; it must not abort the retention pass for
                # the other batches.
                logging.error(
                    "[COMMIT] tx=%s: abandoned row could not be purged (%s)",
                    tx,
                    e,
                )
        if removed:
            logging.info(
                "[COMMIT] purged %d abandoned batch(es) older than %.0fs",
                removed,
                max_age_sec,
            )
        return removed

    def _persist(self, job):
        if self._db is None:
            return
        with self._db.locked() as conn:
            conn.execute(
                "INSERT OR REPLACE INTO commit_jobs"
                " (tx_uid, calling_ae, host, port, sops, pending, deadline,"
                "  frozen, abandoned, abandoned_at, report_attempts,"
                "  cleanup_pending)"
                " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    job.tx_uid,
                    job.calling_ae,
                    job.host,
                    int(job.port),
                    json.dumps([list(item) for item in job.sops]),
                    json.dumps(sorted(list(item) for item in job.pending)),
                    float(job.deadline),
                    # R10: the verdict travels with the row. A restart after a
                    # report was sent but its response was lost must re-send
                    # the SAME result, not a freshly computed one that may
                    # differ for the same transaction.
                    (
                        json.dumps(_verdict_to_json(job.frozen_verdict))
                        if job.frozen_verdict is not None
                        else None
                    ),
                    1 if job.abandoned else 0,
                    float(getattr(job, "abandoned_at", 0.0)),
                    int(job.report_attempts),
                    1 if job.cleanup_pending else 0,
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
        """Register an N-ACTION batch for async resolution. Fire-and-forget.

        `_persist` can raise (disk full, locked or corrupt database). Persist
        BEFORE the in-memory registration: a failed persist then leaves BOTH
        the on-disk row and the in-memory `_jobs` entry from an EARLIER
        registration for the same tx_uid untouched. The old order overwrote
        `_jobs[tx_uid]` first, so a failed persist and the following rollback
        `pop` removed the EARLIER job from the working set while its row
        survived - `_tick` then never resolved that commitment until a
        restart. Persisting first also means `_tick` can never observe a job
        whose persist failed and push an N-EVENT-REPORT for a request the
        caller is about to answer 0x0110. The caller's `except Exception`
        turns the failure into a clean 0x0110.

        `_forget()` is deliberately NOT used here: it runs a DELETE, and after
        a failed INSERT there may be a row from an earlier registration for the
        same tx_uid that must survive - deleting it would lose a commitment the
        SCU was already told about.

        CC1: the ROW and its in-memory owner move together. `_persist` runs
        under the same lock that publishes the job, so a concurrent `_tick`
        never sees the row of one registration paired with the object of
        another, and a crash cannot leave a row whose owner nobody knows. The
        lock is a plain `threading.Lock` held across an SQLite write - the
        store has its own lock and no path takes the two in the other order.
        """
        deadline = self._now() + self._deadline_sec
        job = CommitJob(tx_uid, calling_ae, host, port, sops, deadline)
        with self._lock:
            # R6: a Transaction UID names ONE transaction (PS3.4 J.3.2). A
            # second N-ACTION carrying it is a protocol error, not a retry: the
            # SCU reused an identifier the SCP has already acknowledged, and
            # accepting it would replace the first batch's row and in-memory
            # job - the first N-EVENT-REPORT would never be sent, silently.
            # The check and the registration are one locked step so two
            # concurrent requests cannot both pass it.
            if tx_uid in self._jobs:
                raise ValueError(
                    f"Transaction UID {tx_uid} is already active - the SCU "
                    "must use a new UID for a new request"
                )
            # A settled batch's row write is still PENDING (its DELETE or its
            # abandonment UPDATE failed and is retried by `_retry_settled_rows`).
            # Accepting a registration now would let that retry land on top of
            # the new row: the abandonment would overwrite an accepted
            # transaction, or the cleanup DELETE would erase it, and a restart
            # then lost the commitment the SCU had been told about. The UID is
            # still owned by the old batch until its row write succeeds.
            if tx_uid in self._abandon_pending or tx_uid in self._cleanup:
                raise ValueError(
                    f"Transaction UID {tx_uid} is still owned by a settled "
                    "batch whose row write has not completed - retry later"
                )
            self._persist(job)
            self._jobs[tx_uid] = job

    # ---- resolution ------------------------------------------------------

    def _build_verdict(self, job):
        """Decide the batch's verdict ONCE: `(ok, failed, event_type)`.

        Called on the first delivery attempt and stored on the job, so every
        retry reports the SAME outcome. Recomputing per attempt meant a SOP
        confirmed (or a report that failed and was retried) between attempts
        could change what the modality had already been told - and for the
        success/failure split that is a contradiction, not a retry.
        """
        ok = []
        failed = []
        for sop_class, sop_uid in job.sops:
            if (sop_class, sop_uid) in job.pending:
                failed.append((sop_class, sop_uid, REASON_UNCONFIRMED))
            else:
                ok.append((sop_class, sop_uid))
        event_type = 1 if not failed else 2  # success / complete-with-failures
        return (ok, failed, event_type)

    def _park_undelivered(self, job):
        """Keep an undelivered verdict durable (CC4).

        The job leaves the working set so it stops blocking the single commit
        worker, but its ROW is kept: it is the record that these SOPs were
        never confirmed and that no N-EVENT-REPORT ever reached the modality.
        Deleting it erased the only trace of the outcome.

        R10: `abandoned_at` is stamped here, because this is the instant the
        row becomes eligible for retention - the job's own deadline may be days
        older than the moment the report budget finally ran out.

        Returns True when the row now says "abandoned". A False return means an
        ACTIVE row survived the give-up, which a restart would reload as a
        commitment still to deliver - so the caller must keep the job in the
        write-retry list rather than treat it as parked.
        """
        job.abandoned_at = self._now()
        try:
            self._persist(job)
        except Exception as e:
            logging.error(
                "[COMMIT] tx=%s: undelivered verdict could not be recorded "
                "(%s) - the row stays active and the write is retried without "
                "another report",
                job.tx_uid,
                e,
            )
            return False
        return True

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

        # CC3: report the FROZEN verdict when there is one.
        if job.frozen_verdict is not None:
            ok, failed, event_type = job.frozen_verdict
        else:
            ok, failed, event_type = self._build_verdict(job)
        try:
            delivered = self._report(
                job.host, job.port, job.calling_ae, job.tx_uid, event_type, ok, failed
            )
        except Exception as e:
            logging.error("[COMMIT] report for tx=%s failed: %s", job.tx_uid, e)
            return False
        if delivered is False:
            return False
        # The verdict counters belong here, not in the handler: this is where
        # a SOP actually acquires a verdict. The handler answers 0x0000
        # without checking anything (PS3.4 J.3.2), so counting there would
        # report a verdict the bridge had no evidence for at that instant.
        # Counting BEFORE the report made an undelivered verdict bump the
        # counters on every retry of the same batch, so the metric described
        # attempts rather than outcomes.
        try:
            if ok:
                naction_total.labels(verdict="success").inc(len(ok))
            if failed:
                naction_total.labels(verdict="failed").inc(len(failed))
        except Exception:
            pass
        return True

    def _verify_walk(self, job, verify_deadline):
        """Probe `job.pending` from the resume cursor, within the budget.

        PS3.4 J.3.3.1 makes the report the result, so a SOP the walk never
        reached is reported as unconfirmed - "not stored" - which is a claim
        the bridge has no evidence for. `pending` is a SET, so `list()` over it
        is not a stable order and re-walking it from the start each tick
        examined the same head forever whenever those SOPs did not confirm.

        The walk therefore resumes at the cursor and wraps once: every pending
        SOP gets a turn before any of them gets a second one.
        """
        ordered = sorted(job.pending)
        if not ordered:
            return
        start = 0
        if job.verify_cursor is not None:
            # The next tick resumes AFTER the cursor, not at it: re-probing the
            # boundary SOP costs one round trip per tick for nothing.
            #
            # The cursor is the FULL `(sop_class, sop_uid)` pair, i.e. the same
            # key this list is sorted by. Resuming by UID alone lost every row
            # the UID ordering disagreed with: with the budget consumed by
            # (A, 9), no UID greater than "9" was found, the walk restarted at
            # (A, 9) on every tick, and (B, 1) and (B, 2) were never probed
            # before the deadline - so they were reported "not stored" without
            # the bridge ever having asked.
            for index, item in enumerate(ordered):
                if item > job.verify_cursor:
                    start = index
                    break
            else:
                # Past the tail: wrap to the head, which is what retries the
                # SOPs examined on earlier ticks.
                start = 0
        for offset in range(len(ordered)):
            sop_item = ordered[(start + offset) % len(ordered)]
            if self._now() >= verify_deadline:
                logging.info(
                    "[COMMIT] tx=%s: verification budget reached (%d left "
                    "unverified) - resolving with what is known",
                    job.tx_uid,
                    len(job.pending),
                )
                break
            # The cursor advances BEFORE the probe, so a probe that fails at
            # the tail does not make the next tick start there again.
            job.verify_cursor = sop_item
            sop_class, sop_uid = sop_item
            try:
                confirmed = self._verify(sop_class, sop_uid)
            except Exception as e:
                logging.warning("[COMMIT] verify %s error: %s", sop_uid, e)
                continue
            if not confirmed:
                continue
            # A probe can start inside the window and observe the instance only
            # AFTER it. Counting that as a confirmation tells the modality a
            # SOP is committed when the batch's own deadline said otherwise -
            # so the result is only applied if it arrived in time.
            if self._now() > job.deadline and job.deadline <= verify_deadline:
                logging.info(
                    "[COMMIT] tx=%s: confirmation of %s arrived after the "
                    "deadline - not counted",
                    job.tx_uid,
                    sop_uid,
                )
                continue
            job.pending.discard(sop_item)
        # A cursor past the last uid wraps on the next tick by the `for/else`
        # above; clearing it here would restart mid-set instead.
        if not job.pending:
            job.verify_cursor = None

    def _retry_settled_rows(self):
        """Retry only the ROW writes for batches whose verdict is settled.

        `_park_undelivered` and `_forget` both run after the job left `_jobs`,
        and both can fail. Losing either left an ACTIVE row behind, which a
        restart reloaded as a commitment still to deliver - so the modality was
        sent the same verdict again, or one the bridge had already given up on.
        """
        with self._lock:
            pending_abandon = list(self._abandon_pending.items())
            pending_cleanup = list(self._cleanup.items())
        for tx_uid, job in pending_abandon:
            # The ownership test and the write are ONE locked step. Testing
            # ownership outside the lock let `register()` accept the same UID
            # between the two, and the retry's `_persist(job)` then replaced the
            # NEW row with the old batch's abandoned one - the freshly accepted
            # transaction was silently turned into an abandonment, and the SCU
            # never received the verdict for the request it had just sent.
            # `register()` refuses a UID that is still in `_abandon_pending`, so
            # holding the lock across both is sufficient.
            with self._lock:
                try:
                    # The report is NOT re-sent: only the row write is retried.
                    self._persist(job)
                except Exception as e:
                    logging.error(
                        "[COMMIT] tx=%s: abandonment still not recorded (%s) - "
                        "the row stays active for now",
                        tx_uid,
                        e,
                    )
                    continue
                self._abandon_pending.pop(tx_uid, None)
                self._abandoned[tx_uid] = job
            logging.error("[COMMIT] tx=%s: abandonment recorded on a retry", tx_uid)
        for tx_uid, job in pending_cleanup:
            # Ownership and the DELETE are ONE locked step. Checking the owner
            # and releasing the lock let a `register()` for the same UID
            # persist a NEW row in between, and the retry then deleted it - a
            # restart lost an accepted transaction the SCU had already been
            # told about. `register()` refuses a UID that is still in
            # `_cleanup`, so holding the lock across both is sufficient.
            with self._lock:
                if tx_uid in self._jobs:
                    self._cleanup.pop(tx_uid, None)
                    logging.info(
                        "[COMMIT] tx=%s was re-registered - dropping the "
                        "pending row delete",
                        tx_uid,
                    )
                    continue
                try:
                    self._forget(tx_uid)
                except Exception as e:
                    logging.error(
                        "[COMMIT] tx=%s: row still not deleted (%s) - retried "
                        "next tick, verdict NOT re-sent",
                        tx_uid,
                        e,
                    )
                    continue
            with self._lock:
                self._cleanup.pop(tx_uid, None)

    def _tick(self):
        # Rows whose verdict is already settled: a delivered report whose row
        # delete failed, and an abandonment that never reached the store. Both
        # are OUT of `_jobs` (they must never be reported again), so they need
        # this separate pass or nothing retries their write.
        self._retry_settled_rows()
        with self._lock:
            jobs = list(self._jobs.values())
        for job in jobs:
            # Verification is bounded: each `_verify` may be a network round
            # trip to the origin, and an unbounded walk over a large batch held
            # the single commit worker past the job's own deadline - delaying
            # every other peer's verdict. The deadline is the job's, because
            # that is the instant its answer stops improving: past it the
            # remaining SOPs are reported as unconfirmed regardless.
            #
            # A FROZEN verdict is never re-examined: the outcome was decided
            # (and, once persisted, is on disk), so the remaining ticks only
            # retry the REPORT. Walking `pending` anyway burned up to
            # `verify_budget_sec` of origin round trips per tick on the single
            # commit worker, for an answer nothing may use any more.
            verify_deadline = min(job.deadline, self._now() + self._verify_budget_sec)
            if job.frozen_verdict is None:
                self._verify_walk(job, verify_deadline)
            # The deadline test reads a FRESH clock. `now` captured at the top
            # of the tick predates every report this tick sent, so a batch due
            # while an earlier (slow) report was in flight looked expired here
            # and was resolved as "everything unconfirmed" without a probe.
            if not job.pending or self._now() >= job.deadline:
                # CC3: the verdict is FROZEN on the first attempt. The report
                # must describe the batch as it stood when the outcome was
                # decided: recomputing it per attempt let a later successful
                # verify (or a late confirmation) change what had already been
                # reported, so the retry could contradict the first try.
                if job.frozen_verdict is None:
                    job.frozen_verdict = self._build_verdict(job)
                # R10: the verdict is durable BEFORE the first report goes
                # out. Memory-only, a crash after the N-EVENT-REPORT left the
                # response lost and the restart rebuilt a verdict from a later
                # `pending` set - so the SAME transaction could be reported
                # twice with different outcomes.
                #
                # The write is keyed on `verdict_persisted`, not on the verdict
                # being absent, and it runs on EVERY attempt until it lands: a
                # single failed write left the verdict in memory, so a guard on
                # `frozen_verdict is None` skipped every later attempt and the
                # row kept arguing from the OLD `pending` set at every restart.
                # With a database the operator asked for durability, so a
                # failed persist WITHHOLDS the report: sending first and
                # crashing before `_forget` left a restart to rebuild a
                # DIFFERENT verdict from a later `pending` set - the same
                # transaction reported twice with different outcomes (PS3.4
                # J.3.3.1: the report IS the result). The job stays in the
                # working set so the next tick retries the persist.
                if self._db is not None and not job.verdict_persisted:
                    try:
                        self._persist(job)
                        job.verdict_persisted = True
                        job.pending_persisted = len(job.pending)
                    except Exception as e:
                        logging.critical(
                            "[COMMIT] tx=%s: frozen verdict still not persisted "
                            "(%s) - report withheld until it is",
                            job.tx_uid,
                            e,
                        )
                        continue
                # Retire the job only once its verdict was actually delivered:
                # dropping it after a failed N-EVENT-REPORT lost the outcome
                # the modality was waiting for.
                #
                # The attempt limit is enforced BEFORE `_resolve`, not after.
                # `_reload()` can restore an active row whose persisted
                # `report_attempts` already reached the limit - the process may
                # have stopped between persisting attempt five and persisting
                # the abandonment - and `_tick` used to send one more report
                # before noticing. A row that has already spent its budget goes
                # straight to the abandonment transition instead.
                if job.report_attempts >= MAX_REPORT_ATTEMPTS:
                    with self._lock:
                        still_ours = self._jobs.get(job.tx_uid) is job
                        if still_ours:
                            del self._jobs[job.tx_uid]
                            self._abandon_pending[job.tx_uid] = job
                    logging.error(
                        "[COMMIT] tx=%s: report attempt limit (%d) already "
                        "spent - abandoning without another report",
                        job.tx_uid,
                        MAX_REPORT_ATTEMPTS,
                    )
                    continue
                if self._resolve(job):
                    # R7/R9: the ownership test, the in-memory removal and the
                    # row delete are ONE locked transition. Releasing the lock
                    # between them let a `register` for the same tx_uid persist
                    # a NEW row that this `_forget` then deleted - the new job
                    # survived in memory but its row did not, so a restart lost
                    # a commitment the SCU had just been told to expect.
                    with self._lock:
                        still_ours = self._jobs.get(job.tx_uid) is job
                        if still_ours:
                            del self._jobs[job.tx_uid]
                            # A failed delete must NOT escape: the verdict is
                            # already delivered, and aborting the tick leaves
                            # the row behind, so the next restart reloads the
                            # batch and sends a SECOND report for a verdict
                            # the modality already received. The delete is
                            # therefore retried from `_cleanup`, which carries
                            # a flag on the row so a restart resumes it too.
                            try:
                                self._forget(job.tx_uid)
                            except Exception as e:
                                job.cleanup_pending = True
                                self._cleanup[job.tx_uid] = job
                                logging.error(
                                    "[COMMIT] tx=%s: verdict delivered but the "
                                    "row could not be deleted (%s) - the delete "
                                    "is retried, the report is NOT re-sent",
                                    job.tx_uid,
                                    e,
                                )
                                try:
                                    self._persist(job)
                                except Exception as pe:
                                    logging.error(
                                        "[COMMIT] tx=%s: cleanup state not "
                                        "recorded (%s) - a restart may re-send "
                                        "the report once",
                                        job.tx_uid,
                                        pe,
                                    )
                    if not still_ours:
                        # Re-registered while the report was in flight: the
                        # NEW job and its row belong to the new request.
                        # Retiring them here would lose a commitment the SCU
                        # was just told to expect.
                        logging.info(
                            "[COMMIT] tx=%s was re-registered during resolution "
                            "- keeping the new job",
                            job.tx_uid,
                        )
                        continue
                else:
                    job.report_attempts += 1
                    # The count is durable as it grows. Without it a restart
                    # restored zero and handed an unavailable peer a FRESH
                    # MAX_REPORT_ATTEMPTS budget at every restart.
                    if self._db is not None:
                        try:
                            self._persist(job)
                        except Exception as e:
                            logging.error(
                                "[COMMIT] tx=%s: attempt count not persisted "
                                "(%s) - a restart resets the budget",
                                job.tx_uid,
                                e,
                            )
                    if job.report_attempts >= MAX_REPORT_ATTEMPTS:
                        # The modality will never hear this verdict: retire
                        # the batch and say so at ERROR level. Keeping it
                        # would re-block the single commit worker on every
                        # tick, delaying every other peer's verdict.
                        logging.error(
                            "[COMMIT] tx=%s: giving up after %d failed "
                            "N-EVENT-REPORT attempts - the verdict was never "
                            "delivered",
                            job.tx_uid,
                            job.report_attempts,
                        )
                        # CC4: an UNDELIVERED verdict is the record of what
                        # happened, not a transient state. The job leaves the
                        # working set (it would otherwise re-block this single
                        # worker on every tick) but its ROW STAYS, so the
                        # outcome survives a restart and can be re-reported or
                        # inspected. Deleting it would erase the only trace
                        # that these SOPs were never confirmed.
                        #
                        # Ownership test, in-memory removal and the ABANDONED
                        # row are ONE locked transition, the same rule `_forget`
                        # follows below. Writing the row after releasing the
                        # lock let a `register` of the same Transaction UID land
                        # in between, and the parked row then overwrote the NEW
                        # request's acknowledged commitment.
                        with self._lock:
                            still_ours = self._jobs.get(job.tx_uid) is job
                            if still_ours:
                                job.abandoned = True
                                del self._jobs[job.tx_uid]
                                # The write can fail (disk full, locked store).
                                # The job is already out of the working set, so
                                # a failed write used to leave an ACTIVE row
                                # that a restart re-delivered and that
                                # `purge_abandoned` would never collect. It is
                                # parked for a WRITE-ONLY retry instead.
                                if not self._park_undelivered(job):
                                    job.abandon_pending = True
                                    self._abandon_pending[job.tx_uid] = job
                                else:
                                    self._abandoned[job.tx_uid] = job
                        if not still_ours:
                            # Re-registered while the verdict was being
                            # abandoned: the NEW job and its row belong to the
                            # new request.
                            logging.info(
                                "[COMMIT] tx=%s was re-registered during "
                                "abandonment - keeping the new job",
                                job.tx_uid,
                            )
                            continue
            elif self._db is not None:
                # Confirmations shrink `job.pending` and that set is what a
                # restart reads back. Rewriting the row on EVERY tick would put
                # a commit on the tracker's own timer for a batch that may sit
                # for the full deadline; the row is written when the set
                # actually shrank, which is the only state change here that a
                # restart can observe.
                #
                # The write only belongs to a job that STILL OWNS its row. A
                # re-registration for the same tx_uid replaces `_jobs[tx_uid]`
                # with a fresh CommitJob and its own row; this tick is then
                # holding a snapshot of the OLD object, and writing it would
                # REPLACE the new job's row with stale SOPs and an old
                # deadline. Drawing on one's own row is exactly what "tie the
                # write to the job object" means here.
                if len(job.pending) != job.pending_persisted:
                    with self._lock:
                        still_ours = self._jobs.get(job.tx_uid) is job
                    if still_ours:
                        # CM1: advance the watermark only once the write has
                        # SUCCEEDED. Setting it first meant a failed persist
                        # was never retried - the batch then looked persisted
                        # for the rest of its life, so a restart brought back
                        # the OLD pending set and re-verified (or re-reported)
                        # SOPs that had already been confirmed.
                        #
                        # One batch's store failure must not abort this tick:
                        # the remaining batches would wait an extra interval
                        # for a reason that has nothing to do with them.
                        try:
                            self._persist(job)
                            job.pending_persisted = len(job.pending)
                        except Exception as e:
                            logging.error(
                                "[COMMIT] tx=%s: pending state not persisted (%s)",
                                job.tx_uid,
                                e,
                            )
                    else:
                        logging.debug(
                            "[COMMIT] tx=%s: re-registered - stale pending "
                            "state not written back",
                            job.tx_uid,
                        )

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

    def abandoned_count(self):
        """Undelivered batches kept for inspection (R10). Never ticked."""
        with self._lock:
            return len(self._abandoned)

    def settled_write_count(self):
        """Settled batches whose ROW write is still outstanding.

        A delivered-but-undeleted row and an abandonment that never reached the
        store. Both are retried without another N-EVENT-REPORT.
        """
        with self._lock:
            return len(self._cleanup) + len(self._abandon_pending)

    def close(self):
        self._stop.set()
