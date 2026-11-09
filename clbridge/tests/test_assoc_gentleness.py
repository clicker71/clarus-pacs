# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Retained constant-behaviour tests after the 0709 gentleness knobs removal.

The 0709 knobs `outbound_assoc_retry_delay`, `outbound_assoc_gap` and
`scp_failures_before_remove` are removed (0909 knob cleanup). What REMAINS
and is pinned here:
- the per-peer job gate still serializes concurrent jobs to the SAME peer
  (busy exclusion) with NO post-job cooldown;
- `_associate_for_move` retries the main association IMMEDIATELY (no delay
  knob) up to `outbound_assoc_attempts` and then raises.
Peer eviction == the fixed constant 3 is NOT tested here - it is covered by
`test_db_move_jobs.py::test_consecutive_failures_increment`.
"""

import threading

import clean_code.handlers.cmove as cm
import pytest
from pydicom.uid import ExplicitVRLittleEndian

MR = "1.2.840.10008.5.1.4.1.1.4"
EVRLE = ExplicitVRLittleEndian
KEY = ("1.2.3.4", 104, "SCP")


# ---------------------------------------------------------------------------
# per-peer job gate (_job_gate_enter / _job_gate_exit): busy serialisation,
# no post-job cooldown (multi-context C-MOVE delivery runs a single study
# per association and two jobs to one peer must not overlap).
# ---------------------------------------------------------------------------
def _reset_gate():
    cm._ASSOC_BUSY.clear()
    cm._ASSOC_GATE_LOG_LAST.clear()


def test_gate_enter_marks_busy_and_exit_releases():
    _reset_gate()
    cm._job_gate_enter(*KEY)
    assert KEY in cm._ASSOC_BUSY
    cm._job_gate_exit(*KEY)
    assert KEY not in cm._ASSOC_BUSY


def test_gate_serializes_two_jobs_same_peer_no_gap():
    """Second job to an already-busy peer waits (busy exclusion) and starts
    as soon as the first exits - no post-job cooldown/gap is applied."""
    _reset_gate()
    cm._job_gate_enter(*KEY)  # job A holds the peer
    entered = threading.Event()

    def second_job():
        cm._job_gate_enter(*KEY)  # blocks until A exits
        entered.set()

    t = threading.Thread(target=second_job, daemon=True)
    t.start()
    entered.wait(timeout=0.2)
    assert not entered.is_set()  # B is still gated behind busy A
    cm._job_gate_exit(*KEY)  # A finishes
    assert entered.wait(timeout=2.0)  # B now enters immediately
    cm._job_gate_exit(*KEY)
    t.join(timeout=2.0)
    assert not t.is_alive()


def test_gate_different_peer_not_blocked():
    """Busy exclusion is per-peer: a job to another peer proceeds at once."""
    _reset_gate()
    cm._job_gate_enter(*KEY)
    other = ("5.6.7.8", 105, "SCP2")
    cm._job_gate_enter(*other)  # not blocked
    assert other in cm._ASSOC_BUSY
    cm._job_gate_exit(*other)


def test_gate_log_throttled_once_per_peer(caplog):
    """C3: while a peer is busy the "busy - waiting" DEBUG line is emitted at
    most once (throttled to 1/10s per peer), and a single "waited" entry
    line reports the total gate time. Without the throttle a ~0.55 s wait
    would log ~10 DEBUG lines."""
    import logging

    _reset_gate()
    cm._job_gate_enter(*KEY)  # hold the peer busy
    entered = threading.Event()

    def second_job():
        cm._job_gate_enter(*KEY)
        entered.set()

    with caplog.at_level(logging.DEBUG):
        t = threading.Thread(target=second_job, daemon=True)
        t.start()
        entered.wait(timeout=0.2)
        assert not entered.is_set()
        # Wait ~0.25 s so the old code (logging each 0.05 s tick) would have
        # logged ~5 busy lines; the throttle caps it well below that.
        import time as _t

        _t.sleep(0.25)
        cm._job_gate_exit(*KEY)  # let B through
        assert entered.wait(timeout=2.0)
        t.join(timeout=2.0)
        cm._job_gate_exit(*KEY)

    busy_lines = [r for r in caplog.records if "busy - waiting" in r.getMessage()]
    waited_lines = [
        r
        for r in caplog.records
        if "waited" in r.getMessage() and "ASSOC-GATE" in r.getMessage()
    ]
    assert len(busy_lines) <= 1, f"busy log not throttled: {len(busy_lines)}"
    assert (
        len(waited_lines) == 1
    ), f"expected one 'waited' entry line, got {waited_lines}"
    cm._job_gate_exit(*KEY)


# ---------------------------------------------------------------------------
# _associate_for_move retries the main association with NO delay (immediate
# retries - the 0709 `outbound_assoc_retry_delay` knob is removed).
# ---------------------------------------------------------------------------
class _FakeAssoc:
    def __init__(self, established):
        self._ok = established
        self.accepted_contexts = []

    @property
    def is_established(self):
        return self._ok


class _FakeAE:
    def __init__(self, results):
        self._results = list(results)
        self.requested = []

    def add_requested_context(self, *a, **k):
        self.requested.append(a)

    def associate(self, host, port, ae_title=None):
        established = self._results.pop(0) if self._results else True
        return _FakeAssoc(established)


def _make_associate_fail_twice(monkeypatch):
    """First two associates fail, third establishes; records any time.sleep."""
    fake_ae = _FakeAE([False, False, True])
    monkeypatch.setattr(cm, "AE", lambda ae_title=None: fake_ae)
    slept = []
    monkeypatch.setattr(cm.time, "sleep", lambda s: slept.append(s))
    _reset_gate()
    assoc, accepted = cm._associate_for_move(
        "SCP",
        "BRIDGE",
        {MR: {"accept": True}},
        "127.0.0.1",
        11112,
        outbound_assoc_attempts=3,
    )
    return assoc, accepted, slept


def test_assoc_retries_immediately_no_delay_sleep(monkeypatch):
    """Retries happen with NO pacing sleep: immediate (0609 behaviour) now
    that the retry-delay knob is gone."""
    assoc, _accepted, slept = _make_associate_fail_twice(monkeypatch)
    assert assoc.is_established
    assert slept == [], "no retry pacing delay may be applied"


def test_assoc_exhausts_attempts_and_raises(monkeypatch):
    fake_ae = _FakeAE([False, False, False])
    monkeypatch.setattr(cm, "AE", lambda ae_title=None: fake_ae)
    monkeypatch.setattr(cm.time, "sleep", lambda s: None)
    _reset_gate()
    with pytest.raises(RuntimeError):
        cm._associate_for_move(
            "SCP",
            "BRIDGE",
            {MR: {"accept": True}},
            "127.0.0.1",
            11112,
            outbound_assoc_attempts=3,
        )
