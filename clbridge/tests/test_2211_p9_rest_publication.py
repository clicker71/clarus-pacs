# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P9 REST part 2 - the C-MOVE job publication barrier.

A multi-scope request must be fully submitted, with its cancellation state
installed, before ANY worker can claim a scope.
"""

import os
import tempfile

import pytest


@pytest.fixture()
def manager():
    """A manager over the SharedDb-compatible stub the move_jobs tests use."""
    from clean_code.move_jobs import MoveJobManager

    from tests.test_db_move_jobs import _TestDb

    d = tempfile.mkdtemp()
    return MoveJobManager(
        shared_db=_TestDb(os.path.join(d, "move_jobs.db")), max_workers=2
    )


class TestHeldJobsAreNotClaimable:
    def test_a_held_job_is_invisible_to_the_picker(self, manager):
        """The barrier: a scope inserted 'held' must not be claimable, or the
        first job of a request can start sending while the rest is still being
        submitted (and before its cancel callback exists)."""
        manager.create_job("1.2.3", "DEST", "127.0.0.1", 11112, claimable=False)

        assert manager.pick_pending_job(timeout=0.05) is None
        assert manager.count_pending() == 0

    def test_releasing_makes_every_scope_claimable_at_once(self, manager):
        ids = [
            manager.create_job(f"1.2.{n}", "DEST", "127.0.0.1", 11112, claimable=False)
            for n in range(3)
        ]

        assert manager.release_held_jobs(ids) == 3
        assert manager.count_pending() == 3
        assert manager.pick_pending_job(timeout=0.05) is not None

    def test_a_claimable_job_publishes_immediately(self, manager):
        """The single-scope path keeps its old behaviour."""
        manager.create_job("1.2.3", "DEST", "127.0.0.1", 11112)

        assert manager.count_pending() == 1


class TestCancelLatchScope:
    def test_a_pending_job_still_reads_as_cancellable(self, manager):
        """A 'pending' row has no worker YET, but one can claim it and must
        observe a cancel latched before it started."""
        job_id = manager.create_job("1.2.3", "DEST", "127.0.0.1", 11112)

        assert manager.job_has_live_worker(job_id) is True

    def test_a_terminal_job_has_nobody_left_to_read_the_latch(self, manager):
        """Installing a latch for a finished job leaves a callback in the
        registry that no worker would ever clear."""
        job_id = manager.create_job("1.2.3", "DEST", "127.0.0.1", 11112)
        manager.finish_job(job_id, success=True)

        assert manager.job_has_live_worker(job_id) is False
