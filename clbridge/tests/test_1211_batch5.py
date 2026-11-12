# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false

"""1211 convergence batch 5 regression tests, one per finding.

Each test fails when its fix is reverted: the assertion targets the behaviour
the finding described, not the shape of the code.
"""

from pathlib import Path

_SOP_LIST = [
    ("1.2.840.10008.5.1.4.1.1.2", "1.2.3.1"),
]


# ---------------------------------------------------------------------------
# R1 - a FAILED verdict write is retried, not skipped forever
# ---------------------------------------------------------------------------
class _FlakyPersist:
    """A tracker whose row writes fail until `fail_until` attempts have run."""

    def __init__(self, tmp_path, fail_until=1):
        from tests.test_commit_tracker_persistence import _Db, _tracker

        self.path = str(tmp_path / "c.db")
        self.tracker = _tracker(_Db(self.path))
        self.fail_until = fail_until
        self.attempts = 0
        # The BARE method: reading `tracker._persist` here would hand back the
        # wrapper below, and the wrapper would call itself forever.
        real_persist = type(self.tracker)._persist

        def _flaky(job):
            self.attempts += 1
            if self.attempts <= self.fail_until:
                raise OSError("disk full")
            real_persist(self.tracker, job)

        self._flaky = _flaky

    def fail_from_now(self):
        """Start failing every row write (the registration already succeeded)."""
        self.tracker._persist = self._flaky


def test_a_failed_verdict_write_is_retried_on_the_next_tick(tmp_path):
    """The verdict was persisted under `if frozen_verdict is None`. After a
    failed write the verdict IS set, so the guard skipped every later attempt:
    the row kept the old `pending` set and a restart rebuilt a different
    verdict from it."""
    from tests.test_commit_tracker_persistence import _Db, _tracker

    flaky = _FlakyPersist(tmp_path, fail_until=1)
    tracker = flaky.tracker
    tracker.register("1.2.3.11", "MODALITY", "10.0.0.5", 104, _SOP_LIST)
    flaky.fail_from_now()
    job = tracker._jobs["1.2.3.11"]
    job.deadline = 0.0
    sent = []

    # The report itself must keep failing, so the job stays in the working set
    # long enough for a second tick to retry the write.
    tracker._resolve = lambda j: (sent.append(j.frozen_verdict), False)[1]
    tracker._tick()
    assert job.verdict_persisted is False, "the first write failed"
    assert flaky.attempts == 1

    tracker._tick()
    assert flaky.attempts >= 2, (
        "a failed verdict write must be retried, not skipped because the "
        "verdict is already in memory"
    )
    assert job.verdict_persisted is True
    tracker.close()

    restarted = _tracker(_Db(flaky.path))
    try:
        revived = restarted._jobs["1.2.3.11"]
        assert revived.frozen_verdict == job.frozen_verdict, (
            "the restarted tracker must re-send the verdict that was decided, "
            f"got {revived.frozen_verdict!r} instead of {job.frozen_verdict!r}"
        )
        assert revived.verdict_persisted is True
    finally:
        restarted.close()


# ---------------------------------------------------------------------------
# R2 - abandoning a batch is one locked transition
# ---------------------------------------------------------------------------
def _tick_with_split_window(tracker, job, reregistered):
    """Run one tick while another thread re-registers the Transaction UID.

    `register` refuses an ACTIVE UID (PS3.4 J.3.2), so the SCU's re-use can
    only land once the old job left the working set - exactly the window
    between the locked `del` and the abandoned row write. The racing thread
    retries until it lands, which is what a live reactor does.
    """
    import threading

    real_park = tracker._park_undelivered
    stopping = threading.Event()

    def _park(parked_job):
        # Hold the gap open for the racer. A correct implementation holds the
        # lock here, so the racer simply waits and the assertion below sees a
        # consistent state; the batch-4 shape released the lock, and the racer
        # walks straight in.
        stopping.wait(timeout=0.2)
        return real_park(parked_job)

    def _racer():
        while not stopping.is_set():
            if job.tx_uid not in tracker._jobs:
                try:
                    tracker.register(
                        job.tx_uid,
                        job.calling_ae,
                        job.host,
                        job.port,
                        job.sops,
                    )
                except ValueError:
                    continue
                reregistered["job"] = tracker._jobs[job.tx_uid]
                stopping.set()
                return

    tracker._park_undelivered = _park
    racer = threading.Thread(target=_racer, daemon=True)
    racer.start()
    tracker._tick()
    stopping.set()
    racer.join(timeout=5.0)
    assert not racer.is_alive(), "the racing register never finished"


def test_a_reregistration_during_abandonment_keeps_its_row(tmp_path):
    """The `del` ran under the lock and the ABANDONED row was written after it
    was released. A `register` of the same Transaction UID in that window wrote
    its own row, which the parked abandoned row then overwrote: the new
    commitment was lost on the next restart."""
    from clean_code.commit_tracker import MAX_REPORT_ATTEMPTS

    from tests.test_commit_tracker_persistence import _Db, _tracker

    path = str(tmp_path / "c.db")
    tracker = _tracker(_Db(path))
    tracker.register("1.2.3.12", "MODALITY", "10.0.0.5", 104, _SOP_LIST)
    job = tracker._jobs["1.2.3.12"]
    job.deadline = 0.0
    job.report_attempts = MAX_REPORT_ATTEMPTS - 1
    reregistered = {}

    tracker._resolve = lambda j: False
    _tick_with_split_window(tracker, job, reregistered)

    assert job.abandoned is True, "the old batch was abandoned"
    new_job = reregistered["job"]
    assert tracker._jobs.get("1.2.3.12") is new_job, "the new job owns the slot"
    assert new_job.abandoned is False
    tracker.close()

    restarted = _tracker(_Db(path))
    try:
        revived = restarted._jobs["1.2.3.12"]
        assert revived.abandoned is False, (
            "the parked abandoned row overwrote the new request's row"
        )
        assert revived.frozen_verdict is None
        assert revived.report_attempts == 0
    finally:
        restarted.close()


# ---------------------------------------------------------------------------
# R3 - a reloaded abandoned job is never delivered again
# ---------------------------------------------------------------------------
def test_a_reloaded_abandoned_job_is_not_ticked(tmp_path):
    """`_reload` restores an abandoned row into `_jobs` for inspection, but
    `_tick` never looked at `job.abandoned`: the deadline had long passed, so
    every restart spent another N-EVENT-REPORT on a dead peer."""
    from clean_code.commit_tracker import MAX_REPORT_ATTEMPTS

    from tests.test_commit_tracker_persistence import _Db, _tracker

    path = str(tmp_path / "c.db")
    first = _tracker(_Db(path))
    first.register("1.2.3.13", "MODALITY", "10.0.0.5", 104, _SOP_LIST)
    job = first._jobs["1.2.3.13"]
    job.deadline = 0.0
    job.report_attempts = MAX_REPORT_ATTEMPTS - 1
    first._resolve = lambda j: False
    first._tick()
    assert "1.2.3.13" not in first._jobs
    first.close()

    reports = []
    restarted = _tracker(_Db(path), reports=reports)
    try:
        revived = restarted._abandoned["1.2.3.13"]
        assert revived.abandoned is True
        assert revived.report_attempts >= MAX_REPORT_ATTEMPTS
        assert "1.2.3.13" not in restarted._jobs, (
            "R10: an abandoned batch must not rejoin the working set"
        )

        restarted._resolve = lambda j: (reports.append(j.tx_uid), True)[1]
        restarted._tick()
        assert reports == [], (
            "an abandoned batch must not be resolved again on restart"
        )
        assert "1.2.3.13" in restarted._abandoned, (
            "the row stays for inspection"
        )
    finally:
        restarted.close()

    # R10: the retention sweep is the only thing that removes it, and only
    # once it is old enough.
    purged = _tracker(_Db(path))
    try:
        assert purged.purge_abandoned(max_age_sec=3600.0) == 0, (
            "a fresh abandonment is not expired yet"
        )
        assert "1.2.3.13" in purged._abandoned
        assert purged.purge_abandoned(max_age_sec=-1.0) == 1
        assert purged._abandoned == {}
    finally:
        purged.close()


def test_the_tick_skips_abandoned_jobs_in_both_trackers():
    """The tracker exists twice - `clean_code/` and the bundle mirror. Both
    must keep the abandonment write inside the ownership transition, and
    neither may iterate an abandoned batch in `_tick`.

    R10 moved abandoned batches to `_abandoned`, so the tick no longer needs a
    per-job guard; what still has to hold is that the flag, the removal from
    the working set and the parked row are one locked step.
    """
    root = Path(__file__).resolve().parents[1]
    for rel in ("clean_code/commit_tracker.py", "clbridge.py"):
        src = (root / rel).read_text(encoding="utf-8")
        tick = src.split("    def _tick(self):", 1)[1]
        tick = tick.split("\n    # ---- lifecycle", 1)[0]

        listed = tick.split("jobs = ", 1)[1].split("\n", 1)[0]
        assert "self._jobs.values()" in listed, (
            f"{rel}: _tick must iterate the working set, not the abandoned list"
        )

        lines = tick.splitlines()
        for i, line in enumerate(lines):
            if line.strip() != "job.abandoned = True" or "still_ours" in line:
                continue
            # The flag must sit inside the SAME block as the `del` that hands
            # the slot over, i.e. between that `with self._lock:` line and the
            # first line dedented back to it. Releasing the lock after `del`
            # and writing the abandoned row afterwards is the batch-4 shape.
            depth = len(line) - len(line.lstrip())
            opener = max(
                j
                for j in range(i)
                if lines[j].strip() == "with self._lock:"
                and len(lines[j]) - len(lines[j].lstrip()) < depth
            )
            block = []
            for ln in lines[opener + 1 :]:
                if ln.strip() and (len(ln) - len(ln.lstrip())) <= len(
                    lines[opener]
                ) - len(lines[opener].lstrip()):
                    break
                block.append(ln)
            body = "\n".join(block)
            assert "del self._jobs[job.tx_uid]" in body, (
                f"{rel}: the abandoned flag must be set in the block that "
                "removes the job, not after the lock was released"
            )
            assert "_park_undelivered(job)" in body, (
                f"{rel}: the abandoned row must be written in the same locked "
                "step"
            )
            break
        else:
            raise AssertionError(f"{rel}: no abandonment site found")
