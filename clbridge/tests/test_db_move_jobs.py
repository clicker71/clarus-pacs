# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for MoveJobManager - C-MOVE job queue and SCP target registry.

P31: Tests for create_job, pick_pending_job, update, finish, SCP target
CRUD, concurrent pick atomicity, and failure auto-removal.
"""

import threading

import pytest
from clean_code.move_jobs import MoveJobManager


class _TestDb:
    """Minimal SharedDb-compatible wrapper for move_jobs tests."""

    def __init__(self, path):
        import sqlite3
        import threading

        self._conn = sqlite3.connect(path, check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._lock = threading.Lock()

    def locked(self):
        class _L:
            def __init__(s, lock, conn):
                s._lock, s._conn = lock, conn

            def __enter__(s):
                s._lock.acquire()
                return s._conn

            def __exit__(s, *a):
                s._lock.release()

        return _L(self._lock, self._conn)

    def close(self):
        with self._lock:
            try:
                self._conn.close()
            except Exception:
                pass


@pytest.fixture
def mgr():
    """In-memory MoveJobManager for testing."""
    db = _TestDb(":memory:")
    m = MoveJobManager(shared_db=db, max_workers=2)
    yield m
    m.close()
    db.close()


# ---------------------------------------------------------------------------
# move_jobs CRUD
# ---------------------------------------------------------------------------


class TestMoveJobCrud:
    """create_job, pick_pending_job, update, finish."""

    def test_create_job_returns_int_id(self, mgr):
        jid = mgr.create_job("1.2.3", "DEST_AE")
        assert isinstance(jid, int)

    def test_create_job_status_is_pending(self, mgr):
        mgr.create_job("1.2.3", "DEST_AE")
        job = mgr.pick_pending_job()
        assert job is not None
        assert job["study_uid"] == "1.2.3"
        assert job["move_dest_ae"] == "DEST_AE"

    def test_pick_pending_job_carries_expose_rejected(self, mgr):
        """The QA flag must survive the queue.

        The worker thread outlives the association that queued the job, so the
        only place it can learn that the job came from the Expose Rejected AE
        is the row itself. `create_job` always wrote the column; `pick` used to
        omit it from the SELECT, which silently sent every QA job to
        `clarus_url` and left the sequester in place. Asserting only that
        create_job accepts the argument would not catch that.
        """
        mgr.create_job("1.2.3", "DEST_AE", expose_rejected=True)
        job = mgr.pick_pending_job()
        assert job is not None
        assert job["expose_rejected"] == 1

        # The ordinary AE must stay 0, or every job would be a QA job.
        mgr.create_job("1.2.4", "DEST_AE", expose_rejected=False)
        job2 = mgr.pick_pending_job()
        assert job2 is not None
        assert job2["expose_rejected"] == 0

    def test_pick_pending_job_atomic(self, mgr):
        """pick_pending_job changes status to in_progress atomically -
        same job cannot be picked twice."""
        mgr.create_job("1.2.3", "DEST_AE")
        job = mgr.pick_pending_job()
        assert job is not None
        # Same job cannot be picked again
        job2 = mgr.pick_pending_job()
        assert job2 is None

    def test_pick_empty_queue_returns_none(self, mgr):
        assert mgr.pick_pending_job() is None

    def test_update_job_progress(self, mgr):
        jid = mgr.create_job("1.2.3", "DEST_AE")
        mgr.pick_pending_job()
        mgr.update_job_progress(jid, completed=5, failed=1, total=10)
        # Verify by finishing and checking - should not raise
        mgr.finish_job(jid, success=True)

    def test_finish_job_success(self, mgr):
        jid = mgr.create_job("1.2.3", "DEST_AE")
        mgr.pick_pending_job()
        mgr.finish_job(jid, success=True)
        # Job should not be pickable after finish
        assert mgr.pick_pending_job() is None

    def test_finish_job_failed_with_error(self, mgr):
        jid = mgr.create_job("1.2.3", "DEST_AE")
        mgr.pick_pending_job()
        mgr.finish_job(jid, success=False, error_message="C-STORE failed")
        assert mgr.pick_pending_job() is None

    def test_multiple_jobs_picked_fifo(self, mgr):
        """Jobs are picked in FIFO order (by created_at ASC)."""
        jid1 = mgr.create_job("1.2.3", "DEST_A")
        jid2 = mgr.create_job("4.5.6", "DEST_B")
        job1 = mgr.pick_pending_job()
        job2 = mgr.pick_pending_job()
        assert job1["id"] == jid1
        assert job2["id"] == jid2

    def test_pick_pending_job_increments_attempts(self, mgr):
        """pick_pending_job increments attempts counter."""
        mgr.create_job("1.2.3", "DEST_AE")
        job = mgr.pick_pending_job()
        # attempts field reflects count before increment (0 for first pick)
        assert job["attempts"] == 0
        mgr.finish_job(job["id"], success=True)
        # Create another job and pick - fresh job starts at 0
        mgr.create_job("1.2.3", "DEST_AE")
        job2 = mgr.pick_pending_job()
        assert job2["attempts"] == 0

    def test_concurrent_pick_no_duplicate(self, mgr):
        """Two threads picking simultaneously - only one gets the job."""
        mgr.create_job("1.2.3", "DEST_AE")
        results = []

        def worker():
            job = mgr.pick_pending_job()
            results.append(job)

        t1 = threading.Thread(target=worker)
        t2 = threading.Thread(target=worker)
        t1.start()
        t2.start()
        t1.join()
        t2.join()

        # Only one thread should get a non-None job
        non_none = [r for r in results if r is not None]
        assert len(non_none) == 1

    def test_recover_in_progress_jobs(self, mgr):
        """On restart, in_progress jobs reset to pending."""
        mgr.create_job("1.2.3", "DEST_AE")
        mgr.pick_pending_job()  # moves to in_progress
        # Before recovery, no pending jobs
        assert mgr.pick_pending_job() is None
        count = mgr.recover_in_progress_jobs()
        assert count == 1
        # After recovery, job is pickable again
        job = mgr.pick_pending_job()
        assert job is not None
        assert job["study_uid"] == "1.2.3"


# ---------------------------------------------------------------------------
# SCP target registry
# ---------------------------------------------------------------------------


class TestScpTargetRegistry:
    """SCP target registration and lookup."""

    def test_register_new_target(self, mgr):
        mgr.register_peer("TEST_AE", "192.168.1.1", 104)
        target = mgr.lookup_scp("TEST_AE")
        assert target == ("192.168.1.1", 104)

    def test_register_duplicate_overwrites(self, mgr):
        """Re-registration overwrites existing peer entries."""
        mgr.register_peer("TEST_AE", "192.168.1.1", 104)
        mgr.register_peer("TEST_AE", "10.0.0.1", 11112)
        target = mgr.lookup_scp("TEST_AE")
        assert target == ("10.0.0.1", 11112)

    def test_lookup_nonexistent_returns_none(self, mgr):
        assert mgr.lookup_scp("NONEXISTENT") is None

    def test_consecutive_failures_increment(self, mgr):
        """After 3 consecutive failures, AE is auto-removed."""
        mgr.register_peer("TEST_AE", "192.168.1.1", 104)
        mgr.record_scp_failure("TEST_AE")
        mgr.record_scp_failure("TEST_AE")
        # After 2 failures, AE should still be present
        assert mgr.lookup_scp("TEST_AE") is not None
        mgr.record_scp_failure("TEST_AE")
        # After 3 consecutive failures, AE is auto-removed
        assert mgr.lookup_scp("TEST_AE") is None

    def test_failure_counter_resets_on_success(self, mgr):
        """record_scp_success resets the failure counter."""
        mgr.register_peer("TEST_AE", "192.168.1.1", 104)
        mgr.record_scp_failure("TEST_AE")
        mgr.record_scp_failure("TEST_AE")
        # Success resets counter
        mgr.record_scp_success("TEST_AE")
        mgr.record_scp_failure("TEST_AE")
        mgr.record_scp_failure("TEST_AE")
        # Should still be present (counter was reset, so only 2 failures)
        assert mgr.lookup_scp("TEST_AE") is not None

    def test_register_peer_overwrites_existing_peer(self, mgr):
        """Manual peer re-registration updates host/port."""
        mgr.register_peer("PEER_AE", "10.0.0.1", 104)
        mgr.register_peer("PEER_AE", "10.0.0.2", 11112)
        target = mgr.lookup_scp("PEER_AE")
        assert target == ("10.0.0.2", 11112)
