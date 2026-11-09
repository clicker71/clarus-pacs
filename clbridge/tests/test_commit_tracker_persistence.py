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

import sqlite3
import threading

from clean_code.commit_tracker import CommitTracker


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
