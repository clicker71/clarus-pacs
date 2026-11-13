# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""M7 regression: a registered Storage Commitment batch survives a restart.

The tracker kept its batches in a dict, so a bridge restart dropped every
commitment the modality was still waiting on: the transaction UID stayed
active on the modality's side and no N-EVENT-REPORT ever followed. The rows
now live in SQLite, and these tests pin both directions of that:

  - a batch registered before the restart is resolved after it, with its
    ORIGINAL deadline (a batch that expired while the bridge was down is
    resolved on the first tick, not given a fresh deadline);
  - a batch whose verdict WAS delivered does not come back.
"""

import json
import sqlite3
import threading

import pytest
from clean_code.commit_tracker import MAX_REPORT_ATTEMPTS, CommitTracker


class _Db:
    """The SharedDb contract: one connection, one lock, `locked()`."""

    def __init__(self, path):
        self._conn = sqlite3.connect(path, check_same_thread=False)
        self._lock = threading.Lock()

    def locked(self):
        return _Locked(self._lock, self._conn)

    def close(self):
        with self._lock:
            self._conn.close()


class _Locked:
    def __init__(self, lock, conn):
        self._lock = lock
        self._conn = conn

    def __enter__(self):
        self._lock.acquire()
        return self._conn

    def __exit__(self, *args):
        self._lock.release()


SOP_LIST = [
    ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.999"),
]


def _tracker(db, verify=None, reports=None):
    reports = reports if reports is not None else []
    return CommitTracker(
        verify=verify or (lambda sop_class, sop_uid: True),
        report=lambda *args, **kwargs: reports.append(args),
        interval=0.1,
        deadline_sec=300.0,
        db=db,
    )


def test_a_registered_batch_is_resolved_after_a_restart(tmp_path):
    path = str(tmp_path / "move_jobs.db")

    first = _tracker(_Db(path))
    first.register("1.2.3.4", "MODALITY", "10.0.0.5", 104, SOP_LIST)
    assert first.pending_count() == 1
    first.close()

    # A NEW process: the dict starts empty and the row is all there is.
    reports = []
    second = _tracker(_Db(path), reports=reports)
    assert (
        second.pending_count() == 1
    ), "the batch registered before the restart must be reloaded"
    reloaded = second._jobs["1.2.3.4"]
    assert reloaded.calling_ae == "MODALITY"
    assert reloaded.host == "10.0.0.5"
    assert reloaded.port == 104
    assert reloaded.sops == SOP_LIST
    assert reloaded.pending == set(SOP_LIST)

    second._tick()
    assert second.pending_count() == 0, "the recovered batch is resolved"
    assert len(reports) == 1, "the N-EVENT-REPORT was sent after the restart"
    second.close()


def test_a_recovered_batch_keeps_its_original_deadline(tmp_path):
    """A deadline that passed while the bridge was down is not extended.

    The deadline is a promise the SCP already made: "everything not
    confirmed by this instant is a failure". Recomputing it on reload would
    silently grant the modality an extra 300 s and, worse, report failures
    later than the SCP said it would.
    """
    path = str(tmp_path / "move_jobs.db")

    first = CommitTracker(
        verify=lambda sop_class, sop_uid: True,
        report=lambda *args, **kwargs: None,
        deadline_sec=300.0,
        now=lambda: 1_000_000.0,
        db=_Db(path),
    )
    first.register("1.2.3.5", "MODALITY", "10.0.0.5", 104, SOP_LIST)
    first.close()

    # Restart long after the deadline.
    second = CommitTracker(
        verify=lambda sop_class, sop_uid: False,  # never confirmed
        report=lambda *args, **kwargs: None,
        deadline_sec=300.0,
        now=lambda: 2_000_000.0,
        db=_Db(path),
    )
    job = second._jobs["1.2.3.5"]
    assert job.deadline == 1_000_300.0, (
        "the reloaded deadline must be the one the batch was registered "
        f"with, got {job.deadline}"
    )

    # The first tick sees the expired deadline and resolves it as a failure.
    resolved = []
    second._resolve_fn = None
    second._report = lambda *args: resolved.append(args) or True
    second._tick()
    assert second.pending_count() == 0
    assert len(resolved) == 1
    assert resolved[0][4] == 2, "event type 2: complete, failures exist"
    second.close()


def test_a_delivered_batch_does_not_come_back_after_a_restart(tmp_path):
    path = str(tmp_path / "move_jobs.db")

    first = _tracker(_Db(path))
    first.register("1.2.3.6", "MODALITY", "10.0.0.5", 104, SOP_LIST)
    first._tick()  # verify says stored -> resolved and reported
    assert first.pending_count() == 0
    first.close()

    second = _tracker(_Db(path))
    assert (
        second.pending_count() == 0
    ), "a batch whose verdict was delivered must not be replayed"
    second.close()


def test_a_confirmed_sop_shrinks_the_persisted_pending_set(tmp_path):
    """A partial confirmation survives the restart, so the retry is exact.

    The report has to cover the COMPLETE referenced set (PS3.4 J.3.3.1.2),
    and the split between committed and failed comes from what was
    confirmed. Losing that across a restart would report already-confirmed
    SOPs as failures.
    """
    path = str(tmp_path / "move_jobs.db")
    two = SOP_LIST + [("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.1000")]

    first = _tracker(
        _Db(path), verify=lambda sop_class, sop_uid: sop_uid.endswith("999")
    )
    first.register("1.2.3.7", "MODALITY", "10.0.0.5", 104, two)
    first._tick()  # confirms the first SOP, leaves the second pending
    assert first.pending_count() == 1, "not confirmed in full yet"
    first.close()

    second = _tracker(_Db(path), verify=lambda sop_class, sop_uid: False)
    job = second._jobs["1.2.3.7"]
    assert job.pending == {two[1]}, (
        "the confirmed SOP must not be pending again after the restart: "
        f"{job.pending}"
    )
    second.close()


def test_a_failed_persist_keeps_the_earlier_job_in_the_working_set(tmp_path):
    """A persist failure must not publish a job that has no row.

    The old order wrote `_jobs[tx_uid] = job` first, then a failed persist
    triggered a rollback `pop` - so the EARLIER job disappeared from the
    working set while its database row survived. `_tick` iterates `_jobs`,
    so that earlier commitment was never resolved until a restart.

    R6 changed how a repeat is handled: a second N-ACTION for an ACTIVE
    Transaction UID is now refused outright, so this test drives the same
    invariant through the UID-free path - a failed persist of a NEW batch must
    leave the working set empty and no row behind.
    """
    path = str(tmp_path / "commit.db")
    tracker = _tracker(_Db(path))
    first_sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4.5.6.1")]
    second_sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4.5.6.2")]

    tracker.register("1.2.3.9", "MODALITY", "10.0.0.5", 104, first_sops)
    original = tracker._jobs["1.2.3.9"]

    def boom(_job):
        raise RuntimeError("disk full")

    tracker._persist = boom
    with pytest.raises(RuntimeError):
        tracker.register("1.2.3.90", "MODALITY", "10.0.0.5", 104, second_sops)

    assert (
        "1.2.3.90" not in tracker._jobs
    ), "a batch whose persist failed must not become visible to _tick"
    surviving = tracker._jobs["1.2.3.9"]
    assert surviving is original, "the earlier job must survive the failed persist"
    assert surviving.sops == first_sops
    assert tracker.pending_count() == 1
    tracker.close()


def test_a_second_naction_for_an_active_transaction_uid_is_refused(tmp_path):
    """R6: PS3.4 J.3.2 - a Transaction UID names ONE transaction.

    Re-registering it replaced the first batch's row and in-memory job, so the
    first acknowledged commitment silently lost its N-EVENT-REPORT. The
    handler turns the ValueError into 0x0110.
    """
    path = str(tmp_path / "commit.db")
    tracker = _tracker(_Db(path))
    tx = "1.2.3.10"
    first_sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.10.1")]
    new_sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.10.2")]

    tracker.register(tx, "MODALITY", "10.0.0.5", 104, first_sops)
    original = tracker._jobs[tx]

    with pytest.raises(ValueError) as exc:
        tracker.register(tx, "MODALITY", "10.0.0.5", 104, new_sops)
    assert "already active" in str(exc.value)

    # The FIRST batch is untouched, in memory and on disk.
    assert tracker._jobs[tx] is original
    assert tracker._jobs[tx].sops == first_sops
    with tracker._db.locked() as conn:
        row = conn.execute(
            "SELECT sops FROM commit_jobs WHERE tx_uid = ?", (tx,)
        ).fetchone()
    assert json.loads(row[0]) == [
        list(s) for s in first_sops
    ], "the refused duplicate must not have overwritten the first batch's row"
    tracker.close()


def test_a_stale_tick_does_not_retire_a_different_batch(tmp_path):
    """R7/R9: the ownership test and the row delete are one locked step.

    `_tick` walks a snapshot. A job registered for a DIFFERENT Transaction UID
    while this one is in flight must not be touched by the stale iteration, and
    the retiring job's own row must go with it.
    """
    path = str(tmp_path / "commit.db")
    tracker = _tracker(_Db(path))
    tx = "1.2.3.10"
    other = "1.2.3.20"
    first_sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.10.1")]
    other_sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.20.1")]

    tracker.register(tx, "MODALITY", "10.0.0.5", 104, first_sops)
    old_job = tracker._jobs[tx]

    def register_other(job):
        assert job is old_job
        tracker.register(other, "MODALITY", "10.0.0.5", 104, other_sops)
        return True

    tracker._resolve = register_other
    tracker._tick()

    assert tx not in tracker._jobs, "the resolved job leaves the working set"
    other_job = tracker._jobs.get(other)
    assert other_job is not None, "the unrelated batch must survive the tick"
    assert other_job.sops == other_sops
    with tracker._db.locked() as conn:
        rows = dict(
            (r[0], r[1]) for r in conn.execute("SELECT tx_uid, sops FROM commit_jobs")
        )
    assert tx not in rows, "the resolved batch's row must be deleted"
    assert json.loads(rows[other]) == [list(s) for s in other_sops]
    tracker.close()


def test_an_abandoned_batch_stays_abandoned_across_a_restart(tmp_path):
    """R8: an abandoned row reloaded as active granted the dead peer another
    MAX_REPORT_ATTEMPTS rounds on the single commit worker at every restart."""
    path = str(tmp_path / "commit.db")
    tracker = _tracker(_Db(path))
    tx = "1.2.3.11"
    sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.11.1")]

    tracker.register(tx, "MODALITY", "10.0.0.5", 104, sops)
    job = tracker._jobs[tx]
    tracker._resolve = lambda *a, **k: False
    job.report_attempts = MAX_REPORT_ATTEMPTS - 1
    tracker._tick()

    assert tx not in tracker._jobs, "an abandoned batch leaves the working set"
    with tracker._db.locked() as conn:
        row = conn.execute(
            "SELECT abandoned FROM commit_jobs WHERE tx_uid = ?", (tx,)
        ).fetchone()
    assert row is not None, "the abandoned row is kept for inspection"
    assert row[0] == 1, "the row must record that it was abandoned"
    tracker.close()

    # Restart: the same database, a fresh tracker. R10 keeps the abandoned
    # batch OUT of the working set (it is never ticked and never counted as
    # pending) and in the inspection list instead.
    restarted = _tracker(_Db(path))
    try:
        assert (
            tx not in restarted._jobs
        ), "an abandoned batch must not rejoin the working set"
        assert restarted.pending_count() == 0
        revived = restarted._abandoned.get(tx)
        assert revived is not None, "the row is kept for inspection"
        assert revived.abandoned is True
        assert (
            revived.report_attempts >= MAX_REPORT_ATTEMPTS
        ), "a reloaded abandoned batch must not get a fresh report budget"
    finally:
        restarted.close()


def test_a_stale_abandonment_does_not_retire_another_batch(tmp_path):
    """R7/R9 on the abandonment path: the ownership test and the parked row
    are one locked step, and an unrelated batch is left alone."""
    path = str(tmp_path / "commit.db")
    tracker = _tracker(_Db(path))
    tx = "1.2.3.11"
    other = "1.2.3.21"
    first_sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.11.1")]
    other_sops = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.21.1")]

    tracker.register(tx, "MODALITY", "10.0.0.5", 104, first_sops)
    old_job = tracker._jobs[tx]

    def re_register_then_fail(job):
        assert job is old_job
        tracker.register(other, "MODALITY", "10.0.0.5", 104, other_sops)
        return False  # the report never got through

    tracker._resolve = re_register_then_fail
    old_job.report_attempts = MAX_REPORT_ATTEMPTS - 1
    tracker._tick()

    assert other in tracker._jobs, "the unrelated batch must survive"
    with tracker._db.locked() as conn:
        rows = dict(
            (r[0], r[1]) for r in conn.execute("SELECT tx_uid, sops FROM commit_jobs")
        )
    assert tx in rows, "the abandoned row is kept"
    assert json.loads(rows[other]) == [list(s) for s in other_sops]
    tracker.close()
