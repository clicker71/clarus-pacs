# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P5 Storage Commitment tracking - one test per finding.

The tracker is driven over the REAL `CommitTracker` on a temp SQLite file, so
the persisted row is what each test inspects: the findings are all about what
survives a tick and what survives a restart, and a mocked store cannot show
either.

The report wire path is exercised over a real pynetdicom AE in
`test_the_report_goes_out_over_a_real_association`.
"""

import sqlite3
import threading

from clean_code.commit_tracker import (
    MAX_REPORT_ATTEMPTS,
    CommitJob,
    CommitTracker,
)

SOP_CLASS = "1.2.840.10008.5.1.4.1.1.2"


class _Db:
    """The SharedDb contract: one connection, one lock, `locked()`.

    `fail_on` names a statement prefix that raises until `allow[0]` is set -
    the store misbehaving for exactly one kind of write, so a test can drive
    "the abandonment write failed" or "the delete failed" without touching the
    others.
    """

    def __init__(self, path, fail_on=None, allow=None):
        self._conn = sqlite3.connect(path, check_same_thread=False)
        self._lock = threading.Lock()
        self.fail_on = fail_on
        self.allow = allow if allow is not None else [False]

    def locked(self):
        return _Locked(self._lock, self._conn, self.fail_on, self.allow)

    def close(self):
        with self._lock:
            self._conn.close()

    def rows(self, sql, params=()):
        with self._lock:
            return self._conn.execute(sql, params).fetchall()


class _Locked:
    def __init__(self, lock, conn, fail_on=None, allow=None):
        self._lock = lock
        self._conn = conn
        self._fail_on = fail_on
        self._allow = allow

    def __enter__(self):
        self._lock.acquire()
        return self

    def __exit__(self, *args):
        self._lock.release()

    def execute(self, sql, params=()):
        if (
            self._fail_on
            and not self._allow[0]
            and sql.lstrip().upper().startswith(self._fail_on)
        ):
            raise sqlite3.OperationalError("database is locked")
        return self._conn.execute(sql, params)

    def commit(self):
        return self._conn.commit()


def _sops(count):
    return [(SOP_CLASS, f"1.2.3.4.{i}") for i in range(count)]


def _tracker(db=None, verify=None, reports=None, now=None, deadline_sec=300.0):
    reports = reports if reports is not None else []
    kwargs = {}
    if now is not None:
        kwargs["now"] = now
    return CommitTracker(
        verify=verify or (lambda sop_class, sop_uid: True),
        report=lambda host, port, ae, tx, ev, ok, failed: (
            reports.append((host, port, ae, tx, ev, ok, failed)) or True
        ),
        interval=0.1,
        deadline_sec=deadline_sec,
        db=db,
        **kwargs,
    )


# ---------------------------------------------------------------------------
# :592 - resume verification after the last SOP checked
# ---------------------------------------------------------------------------
def test_the_walk_resumes_after_the_last_sop_checked():
    """A batch larger than one tick's verification budget must not re-walk
    the SAME head of `pending` every tick.

    A set is unordered, so "the first N entries" is not even a stable prefix;
    without a cursor a batch whose head never confirms could spend every tick's
    budget on the same SOPs and never reach the tail. Here the budget is 3 s
    and each verify burns 1 s, so exactly three SOPs are examined per tick.
    """
    # The budget is checked against wall time, so the probe itself is what
    # must advance the clock - never the loop's own bookkeeping.
    clock = [0.0]
    examined = []

    def verify(sop_class, sop_uid):
        examined.append(sop_uid)
        clock[0] += 1.0  # each probe costs a second of the budget
        return False  # nothing is ever confirmed, so `pending` never shrinks

    tracker = _tracker(
        verify=verify,
        now=lambda: clock[0],
        deadline_sec=10_000.0,
    )
    tracker._verify_budget_sec = 2.5
    tracker.register("TX-RESUME", "MOD", "10.0.0.5", 104, _sops(9))

    tracker._tick()
    first = list(examined)
    assert len(first) == 3, f"the budget allows three probes, got {first}"

    tracker._tick()
    second = list(examined[3:])
    assert len(second) == 3, second
    assert not set(first) & set(second), (
        "the second tick re-probed SOPs the first tick already examined: "
        f"{sorted(set(first) & set(second))}"
    )

    tracker._tick()
    third = list(examined[6:])
    assert len(third) == 3, third
    assert not set(first) & set(third), f"{sorted(set(first) & set(third))}"
    assert not set(second) & set(third), f"{sorted(set(second) & set(third))}"
    covered = set(first) | set(second) | set(third)
    assert len(covered) == 9, (
        "every pending SOP must have had a turn after three ticks, got "
        f"{sorted(covered)}"
    )


def test_the_cursor_wraps_so_unconfirmed_sops_are_retried():
    """After the tail, the next tick starts again at the head: an unconfirmed
    SOP is retried, not forgotten."""
    clock = [0.0]
    examined = []

    def verify(sop_class, sop_uid):
        examined.append(sop_uid)
        clock[0] += 1.0
        return False

    tracker = _tracker(verify=verify, now=lambda: clock[0], deadline_sec=10_000.0)
    tracker._verify_budget_sec = 1.5
    tracker.register("TX-WRAP", "MOD", "10.0.0.5", 104, _sops(4))

    tracker._tick()
    tracker._tick()
    assert len(examined) == 4, examined
    examined.clear()
    tracker._tick()
    assert len(examined) == 2, (
        "the wrap-around tick must probe again from the head, go " f"{examined!r}"
    )


# ---------------------------------------------------------------------------
# :604 - a confirmation that lands after the deadline does not count
# ---------------------------------------------------------------------------
def test_a_confirmation_after_the_deadline_does_not_count():
    """A probe may start inside the window and observe the instance only after
    it. Counting that as a confirmation tells the modality a SOP is committed
    when the SCP's own deadline said otherwise.

    The clock advances while the probe runs, so the second SOP is confirmed at
    t=120 against a deadline of t=100 - inside the tick, outside the batch's
    own window."""
    clock = [0.0]
    reports = []

    def verify(sop_class, sop_uid):
        # The LAST probe is the slow one: it starts inside the window and
        # finishes after it, which is exactly the case the finding names.
        clock[0] += 120.0 if sop_uid == "1.2.3.4.2" else 30.0
        return True

    tracker = _tracker(
        verify=verify, reports=reports, now=lambda: clock[0], deadline_sec=100.0
    )
    # The budget is deliberately far away: this test is about the DEADLINE, not
    # about the walk being cut short.
    tracker._verify_budget_sec = 10_000.0
    tracker.register("TX-LATE", "MOD", "10.0.0.5", 104, _sops(3))

    tracker._tick()

    # The batch resolves and retires in the same tick, so the verdict is read
    # from what was REPORTED rather than from the (now gone) job.
    assert len(reports) == 1, f"exactly one report: {reports!r}"
    _host, _port, _ae, _tx, event_type, ok, failed = reports[0]
    assert event_type == 2, "the batch is not fully committed"
    assert {uid for _cls, uid in ok} == {"1.2.3.4.0", "1.2.3.4.1"}, ok
    assert {uid for _cls, uid, _r in failed} == {"1.2.3.4.2"}, (
        "the probe that finished after the deadline must not be counted, got "
        f"{failed!r}"
    )
    assert tracker.pending_count() == 0


def test_a_confirmation_inside_the_window_still_counts():
    """Non-vacuity: the same walk with a deadline far away counts every SOP."""
    clock = [0.0]

    def verify(sop_class, sop_uid):
        clock[0] += 60.0
        return True

    tracker = _tracker(verify=verify, now=lambda: clock[0], deadline_sec=10_000.0)
    tracker._verify_budget_sec = 10_000.0
    tracker.register("TX-ONTIME", "MOD", "10.0.0.5", 104, _sops(3))
    tracker._tick()

    job = tracker._jobs.get("TX-ONTIME")
    if job is not None:
        assert job.frozen_verdict is None or job.frozen_verdict[2] == 1
    assert tracker.pending_count() == 0, "delivered and retired"


# ---------------------------------------------------------------------------
# :608 - read the clock when deciding the batch has expired
# ---------------------------------------------------------------------------
def test_a_batch_still_inside_its_deadline_is_not_resolved_early():
    """`now` is captured once per tick, before any batch runs. A slow report
    for an earlier batch therefore made the NEXT batch look expired: its
    verification stopped (the budget is clamped by the deadline) and it was
    resolved as a failure without its remaining SOPs ever being probed.

    Here the first batch's report takes 40 s. The second batch's deadline is
    100 s away, so after the report it still has 60 s - but a `now` read at
    t=0 would have clamped its verification to a deadline already reached."""
    clock = [0.0]
    probed = []

    def verify(sop_class, sop_uid):
        probed.append(sop_uid)
        return True

    def report(host, port, ae, tx, ev, ok, failed):
        if tx == "TX-SLOW":
            clock[0] += 40.0  # a slow peer
        return True

    tracker = CommitTracker(
        verify=verify,
        report=report,
        interval=0.1,
        deadline_sec=300.0,
        now=lambda: clock[0],
    )
    tracker.register("TX-SLOW", "MOD", "10.0.0.5", 104, _sops(1))
    tracker.register("TX-NEXT", "MOD", "10.0.0.6", 104, _sops(3))
    # The next batch is due 100 s from now - well after the slow report.
    tracker._jobs["TX-NEXT"].deadline = 100.0

    tracker._tick()

    assert tracker.pending_count() == 0
    assert len(probed) == 4, (
        "the second batch must still be verified after the slow report, "
        f"probed {probed!r}"
    )
    # And it resolved as SUCCESS, not as "everything unconfirmed".
    assert "TX-NEXT" not in tracker._jobs


def test_the_deadline_check_uses_a_fresh_clock_reading():
    """The verdict decision reads the clock at the decision, not at the top of
    the tick: a batch whose deadline falls DURING the tick is still inside it."""
    clock = [0.0]

    def verify(sop_class, sop_uid):
        clock[0] += 10.0  # the tick takes time
        return True

    tracker = CommitTracker(
        verify=verify,
        report=lambda *a, **k: True,
        interval=0.1,
        deadline_sec=25.0,
        now=lambda: clock[0],
    )
    tracker.register("TX-FRESH", "MOD", "10.0.0.5", 104, _sops(2))
    tracker._tick()
    # Two probes at 10 s each: the second lands at t=20, inside the 25 s
    # deadline, so the batch resolved by confirmation rather than at deadline.
    assert tracker.pending_count() == 0


# ---------------------------------------------------------------------------
# :650 - a slow report must not consume another batch's window
# ---------------------------------------------------------------------------
def test_a_slow_report_does_not_starve_the_next_batch(tmp_path):
    """The single worker sends the next N-EVENT-REPORT only after the previous
    one returns. With one batch per tick, a 25 s report ate the next batch's
    remaining window entirely.

    Driven over the real tracker with a database, so the delivered batch's
    row is deleted and the second batch is reported in the SAME tick."""
    path = str(tmp_path / "commit.db")
    clock = [0.0]
    order = []

    def verify(sop_class, sop_uid):
        return True

    def report(host, port, ae, tx, ev, ok, failed):
        order.append(tx)
        if tx == "TX-A":
            clock[0] += 25.0
        return True

    tracker = CommitTracker(
        verify=verify,
        report=report,
        interval=0.1,
        deadline_sec=300.0,
        now=lambda: clock[0],
        db=_Db(path),
    )
    tracker.register("TX-A", "MOD", "10.0.0.5", 104, _sops(1))
    tracker.register("TX-B", "MOD", "10.0.0.6", 104, _sops(1))
    tracker._jobs["TX-B"].deadline = 20.0  # expires during TX-A's report

    tracker._tick()

    # One tick must report BOTH: the second batch's deadline passes while the
    # first report is in flight, and it still has to be probed and reported,
    # not deferred to a tick that would find it already expired.
    assert order == ["TX-A", "TX-B"], (
        "the batch behind a slow report must be reported in the same tick, "
        f"got {order}"
    )
    assert tracker.pending_count() == 0


def test_a_failed_report_still_costs_only_its_own_batch():
    """Non-vacuity neighbour: a report that FAILS does not retire its batch and
    does not stop the next one from being reported in the same tick."""
    clock = [0.0]
    order = []

    def report(host, port, ae, tx, ev, ok, failed):
        order.append(tx)
        if tx == "TX-FAIL":
            clock[0] += 5.0
            return False
        return True

    tracker = CommitTracker(
        verify=lambda c, u: True,
        report=report,
        interval=0.1,
        deadline_sec=300.0,
        now=lambda: clock[0],
    )
    tracker.register("TX-FAIL", "MOD", "10.0.0.5", 104, _sops(1))
    tracker.register("TX-OK", "MOD", "10.0.0.6", 104, _sops(1))
    tracker._tick()

    assert order == ["TX-FAIL", "TX-OK"], order
    assert tracker.pending_count() == 1, "only the failed one stays"
    assert tracker._jobs["TX-FAIL"].report_attempts == 1


# ---------------------------------------------------------------------------
# :514 - a failed abandonment write must be retried, without another report
# ---------------------------------------------------------------------------
def test_a_failed_abandonment_write_is_retried_without_another_report(tmp_path):
    """`_park_undelivered` logs and returns on a failed write, and the caller
    had already dropped the job from `_jobs` - so the row stayed ACTIVE. A
    restart then reloaded it as deliverable and sent the report again; and the
    retention sweep only deletes rows marked abandoned, so the row was never
    collected either.

    The store refuses the write until `allow` is set."""
    path = str(tmp_path / "commit.db")
    allow = [True]  # the initial registration must land
    db = _Db(path, fail_on="INSERT OR REPLACE", allow=allow)
    reports = []

    tracker = CommitTracker(
        verify=lambda c, u: True,
        report=lambda host, port, ae, tx, ev, ok, failed: (reports.append(tx) or False),
        interval=0.1,
        deadline_sec=0.0,
        now=lambda: 1_000.0,
        db=db,
    )
    tracker.register("TX-ABANDON", "MOD", "10.0.0.5", 104, _sops(1))
    # The frozen verdict must land first - a failed VERDICT write withholds the
    # report by design - so the store only starts refusing once it has.
    tracker._tick()
    assert reports == ["TX-ABANDON"], reports
    allow[0] = False

    for _ in range(MAX_REPORT_ATTEMPTS - 1):
        tracker._tick()

    assert len(reports) == MAX_REPORT_ATTEMPTS, reports
    assert tracker.pending_count() == 0, "it left the working set"
    assert tracker.abandoned_count() == 0, "the abandonment did NOT land"
    assert tracker.settled_write_count() == 1, "but it is queued for a retry"

    # The write failed, so the row must NOT be marked abandoned - but the job
    # must still be known as an abandoned write that has to be retried.
    allow[0] = True
    tracker._tick()

    row = db.rows("SELECT abandoned FROM commit_jobs WHERE tx_uid = ?", ("TX-ABANDON",))
    assert row == [(1,)], f"the abandonment must land on a retry, got {row!r}"
    assert len(reports) == MAX_REPORT_ATTEMPTS, (
        "retrying the WRITE must not send another N-EVENT-REPORT: "
        f"{len(reports)} reports"
    )
    assert tracker.settled_write_count() == 0
    assert tracker.abandoned_count() == 1


def test_an_abandoned_write_that_failed_is_not_purged(tmp_path):
    """The retention sweep deletes only rows it can see as abandoned. A row
    whose abandonment write failed is retried first, so a restart cannot
    resurrect a report the bridge already gave up on."""
    path = str(tmp_path / "commit.db")
    allow = [True]
    db = _Db(path, fail_on="INSERT OR REPLACE", allow=allow)

    clock = [1_000.0]
    tracker = CommitTracker(
        verify=lambda c, u: True,
        report=lambda *a, **k: False,
        interval=0.1,
        deadline_sec=0.0,
        now=lambda: clock[0],
        db=db,
    )
    tracker.register("TX-NOPURGE", "MOD", "10.0.0.5", 104, _sops(1))
    # One successful tick so the verdict write lands, then the store refuses.
    tracker._tick()
    allow[0] = False
    for _ in range(MAX_REPORT_ATTEMPTS - 1):
        tracker._tick()

    clock[0] += 10 * 24 * 3600.0
    assert (
        tracker.purge_abandoned() == 0
    ), "a row whose abandonment never landed must not be purged away"

    allow[0] = True
    tracker._tick()  # the deferred abandonment write lands here
    assert tracker.purge_abandoned() == 1, "and then it is eligible"


# ---------------------------------------------------------------------------
# :688 - the report attempt count survives a restart
# ---------------------------------------------------------------------------
def test_the_report_attempt_count_survives_a_restart(tmp_path):
    """Four failed deliveries followed by a restart must not hand the dead peer
    a fresh budget: the fifth failure is the fifth failure."""
    path = str(tmp_path / "commit.db")

    first = CommitTracker(
        verify=lambda c, u: True,
        report=lambda *a, **k: False,
        interval=0.1,
        deadline_sec=0.0,
        now=lambda: 1_000.0,
        db=_Db(path),
    )
    first.register("TX-ATTEMPTS", "MOD", "10.0.0.5", 104, _sops(1))
    for _ in range(MAX_REPORT_ATTEMPTS - 1):
        first._tick()

    job = first._jobs["TX-ATTEMPTS"]
    assert job.report_attempts == MAX_REPORT_ATTEMPTS - 1
    first.close()

    reports = []
    second = CommitTracker(
        verify=lambda c, u: True,
        report=lambda host, port, ae, tx, ev, ok, failed: (reports.append(tx) or False),
        interval=0.1,
        deadline_sec=0.0,
        now=lambda: 1_000.0,
        db=_Db(path),
    )
    reloaded = second._jobs["TX-ATTEMPTS"]
    assert reloaded.report_attempts == MAX_REPORT_ATTEMPTS - 1, (
        "the attempt count must travel with the row, got " f"{reloaded.report_attempts}"
    )

    second._tick()  # the LAST allowed attempt
    assert len(reports) == 1, "one attempt, not a fresh budget"
    assert second.pending_count() == 0
    assert second.abandoned_count() == 1
    second.close()


def test_a_reloaded_batch_with_attempts_left_can_still_deliver(tmp_path):
    """Non-vacuity: the restored count is a floor, not a dead end - a batch
    restored with attempts to spare still delivers."""
    path = str(tmp_path / "commit.db")

    first = CommitTracker(
        verify=lambda c, u: True,
        report=lambda *a, **k: False,
        interval=0.1,
        deadline_sec=0.0,
        now=lambda: 1_000.0,
        db=_Db(path),
    )
    first.register("TX-RECOVER", "MOD", "10.0.0.5", 104, _sops(1))
    first._tick()
    first.close()

    reports = []
    second = CommitTracker(
        verify=lambda c, u: True,
        report=lambda host, port, ae, tx, ev, ok, failed: (reports.append(tx) or True),
        interval=0.1,
        deadline_sec=0.0,
        now=lambda: 1_000.0,
        db=_Db(path),
    )
    assert second._jobs["TX-RECOVER"].report_attempts == 1
    second._tick()
    assert reports == ["TX-RECOVER"]
    assert second.pending_count() == 0
    assert second.abandoned_count() == 0
    second.close()


# ---------------------------------------------------------------------------
# :660 - a cleanup failure must stay retryable without redelivering
# ---------------------------------------------------------------------------
def test_a_failed_row_delete_is_retried_without_redelivering(tmp_path):
    """The verdict was delivered and then `_forget` failed. The job had already
    left `_jobs`, so nothing retried the delete: the row survived, and a
    restart sent the SAME report again."""
    path = str(tmp_path / "commit.db")
    allow = [False]
    db = _Db(path, fail_on="DELETE", allow=allow)
    reports = []

    tracker = CommitTracker(
        verify=lambda c, u: True,
        report=lambda host, port, ae, tx, ev, ok, failed: (reports.append(tx) or True),
        interval=0.1,
        deadline_sec=300.0,
        now=lambda: 1_000.0,
        db=db,
    )
    tracker.register("TX-CLEANUP", "MOD", "10.0.0.5", 104, _sops(1))
    tracker._tick()
    assert reports == ["TX-CLEANUP"], "the verdict was delivered once"
    assert tracker.pending_count() == 0
    assert db.rows("SELECT tx_uid FROM commit_jobs") == [
        ("TX-CLEANUP",)
    ], "the delete failed, so the row is still there"

    allow[0] = True
    tracker._tick()
    assert (
        db.rows("SELECT tx_uid FROM commit_jobs") == []
    ), "the pending cleanup must be retried on the next tick"
    assert reports == ["TX-CLEANUP"], f"and must NOT re-send the report, got {reports}"


def test_a_cleanup_retry_survives_a_restart(tmp_path):
    """The delivered-but-undeleted state is persisted, so the retry happens
    even if the process restarts in between."""
    path = str(tmp_path / "commit.db")
    allow = [False]
    db = _Db(path, fail_on="DELETE", allow=allow)

    first = CommitTracker(
        verify=lambda c, u: True,
        report=lambda *a, **k: True,
        interval=0.1,
        deadline_sec=300.0,
        now=lambda: 1_000.0,
        db=db,
    )
    first.register("TX-RESTART", "MOD", "10.0.0.5", 104, _sops(1))
    first._tick()
    first.close()

    allow[0] = True
    reports = []
    second = CommitTracker(
        verify=lambda c, u: True,
        report=lambda host, port, ae, tx, ev, ok, failed: (reports.append(tx) or True),
        interval=0.1,
        deadline_sec=300.0,
        now=lambda: 1_000.0,
        db=_Db(path),
    )
    assert second.pending_count() == 0, "a delivered verdict is not pending"
    assert reports == [], "and it is not re-reported"
    second._tick()
    assert (
        db.rows("SELECT tx_uid FROM commit_jobs") == []
    ), "the restart resumed the delete instead of dropping it"
    assert reports == []


# ---------------------------------------------------------------------------
# The report still reaches the peer over a real association
# ---------------------------------------------------------------------------
def test_the_report_goes_out_over_a_real_association():
    """End to end for the wire path these findings sit behind: a real
    pynetdicom AE receives the N-EVENT-REPORT the tracker decides on."""
    import socket

    from pynetdicom import AE, evt
    from pynetdicom.sop_class import StorageCommitmentPushModel

    received = []

    def on_n_event(event):
        received.append(event)
        return 0x0000, None

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()

    scp = AE(ae_title="SCM_SCP")
    # The SCM is the SCP of the report it RECEIVES and the bridge sends it as
    # SCU. pynetdicom 3.0.4 requires BOTH role flags once either is set
    # (`ae.make_server`: "if one is None, both must be"), so the SCU role has to
    # be granted explicitly or the report is refused at negotiation.
    scp.add_supported_context(StorageCommitmentPushModel, scu_role=True, scp_role=True)
    server = scp.start_server(
        ("127.0.0.1", port),
        block=False,
        evt_handlers=[(evt.EVT_N_EVENT_REPORT, on_n_event)],
    )
    try:

        def report(host, port_, ae, tx, ev, ok, failed):
            from clean_code.nevent import send_n_event_report

            return send_n_event_report(
                host,
                port_,
                ae,
                tx,
                ev,
                success_sops=ok,
                failed_sops=failed,
                bridge_ae_title="BRIDGE",
            )

        tracker = CommitTracker(
            # Both SOPs are stored, so the batch resolves on CONFIRMATION.
            verify=lambda c, u: True,
            report=report,
            interval=0.1,
            deadline_sec=300.0,
            now=lambda: 1_000.0,
        )
        tracker.register(
            "1.2.3.4.5",
            "SCM_SCP",
            "127.0.0.1",
            port,
            [
                (SOP_CLASS, "1.2.3.4.5.1"),
                (SOP_CLASS, "1.2.3.4.5.2"),
            ],
        )
        tracker._tick()
        tracker.close()

        assert len(received) == 1, "the SCM must receive one N-EVENT-REPORT"
        info = received[0].event_information
        assert info.TransactionUID == "1.2.3.4.5"
        committed = list(info.ReferencedSOPSequence)
        assert [s.ReferencedSOPInstanceUID for s in committed] == [
            "1.2.3.4.5.1",
            "1.2.3.4.5.2",
        ]
        # A fully successful batch carries NO failed sequence at all (PS3.4
        # J.3.3.1: the sequence is conditional on a failure existing).
        assert "FailedSOPSequence" not in info, "nothing failed"
        assert received[0].event_type == 1
    finally:
        try:
            server.shutdown()
        except Exception:
            pass
        try:
            scp.shutdown()
        except Exception:
            pass


def test_a_job_can_be_built_with_the_documented_attributes():
    """Guard for the `__slots__` tuple: the new cursor/dirty slots must be
    declared, or every assignment raises AttributeError at runtime."""
    job = CommitJob("TX", "AE", "host", 104, _sops(1), 1.0)
    for name in ("verify_cursor", "abandon_pending", "cleanup_pending"):
        assert hasattr(job, name), f"{name} must be a declared slot"
