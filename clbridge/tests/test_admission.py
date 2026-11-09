# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Stage 4 tests: unified _AdmissionController (QUEUE-MERGE Part 3).

Validates the single pre-store gate that merges HealthChecker + backpressure:
  - server dead >= 30s -> 0xC000 (Cannot Perform, not transient);
  - server degraded (< 30s) -> 0xA700;
  - queue backpressure -> 0xA700 (90-95%) / 0xC000 (>=95%);
  - healthy + empty queue -> 0x0000 (admit);
  - no outbox/health -> 0x0000 (back-compat).
"""

import time

from clean_code.handlers.cstore import _AdmissionController, _check_backpressure


class _FakeHealth:
    """Minimal HealthChecker stand-in exposing the used interface."""

    def __init__(self, healthy, dead_seconds=0.0):
        self._healthy = healthy
        self._dead = dead_seconds

    def check_before_stow(self):
        if self._healthy:
            return (True, 0x0000, "")
        return (False, 0xA700, "server unhealthy")

    def dead_seconds(self):
        return self._dead


class _FakeOutbox:
    def __init__(self, pending, max_queue):
        self._pending = pending
        self.max_queue = max_queue

    def pending_count(self):
        return self._pending


class TestAdmissionController:

    def test_healthy_and_empty_admits(self):
        """Healthy server + empty queue -> 0x0000."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(0, 20000),
            health_checker=_FakeHealth(healthy=True),
        )
        assert ctl.check() == (0x0000, "")

    def test_dead_server_fatal_returns_c000(self):
        """Server dead >= 30s -> 0xC000 (Cannot Perform)."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(0, 20000),
            health_checker=_FakeHealth(healthy=False, dead_seconds=45.0),
        )
        status, _ = ctl.check()
        assert status == 0xC000

    def test_degraded_server_returns_a700(self):
        """Server unhealthy but dead < 30s -> 0xA700 (retry later)."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(0, 20000),
            health_checker=_FakeHealth(healthy=False, dead_seconds=10.0),
        )
        status, _ = ctl.check()
        assert status == 0xA700

    def test_queue_95_percent_returns_c000(self):
        """Queue >= 95% -> 0xC000 (backpressure)."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(19000, 20000),  # 95%
            health_checker=_FakeHealth(healthy=True),
        )
        status, _ = ctl.check()
        assert status == 0xC000

    def test_queue_92_percent_returns_a700(self):
        """Queue 90-95% -> 0xA700 (Out of Resources)."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(18400, 20000),  # 92%
            health_checker=_FakeHealth(healthy=True),
        )
        status, _ = ctl.check()
        assert status == 0xA700

    def test_no_outbox_no_health_admits(self):
        """Neither outbox nor health provided -> 0x0000 (back-compat)."""
        ctl = _AdmissionController()
        assert ctl.check() == (0x0000, "")

    def test_healthy_but_outbox_90_95_a700(self):
        """Health OK but queue pressure -> backpressure decides."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(18400, 20000),
            health_checker=_FakeHealth(healthy=True),
        )
        status, reason = ctl.check()
        assert status == 0xA700
        assert reason == "queue backpressure"


class TestCheckBackpressure:
    """The shared queue-depth gate (unchanged behavior)."""

    def test_below_70_no_reject(self):
        assert _check_backpressure(pending=100, max_queue=1000) == 0x0000

    def test_95_rejects_c000(self):
        assert _check_backpressure(pending=950, max_queue=1000) == 0xC000

    def test_92_rejects_a700(self):
        assert _check_backpressure(pending=920, max_queue=1000) == 0xA700

    def test_70_90_rate_limits_without_reject(self):
        # In the 70-90% band it sleeps (~0.05s at 80%) then returns 0x0000.
        start = time.time()
        status = _check_backpressure(pending=800, max_queue=1000)  # 80%
        assert status == 0x0000
        assert time.time() - start >= 0.01  # a rate-limit delay did run


class TestStoreForwardParking:
    """Stage 5: store_forward parking branch of _AdmissionController."""

    def _dead_health(self, dead=45.0):
        return _FakeHealth(healthy=False, dead_seconds=dead)

    def test_park_when_store_forward_on(self):
        """Dead >= 30s + store_forward=on -> park (0x0000)."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(0, 20000),
            health_checker=self._dead_health(45.0),
            store_forward=True,
        )
        status, _ = ctl.check()
        assert status == 0x0000

    def test_reject_when_store_forward_off(self):
        """Dead >= 30s + store_forward=off -> 0xC000 (Cannot Perform)."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(0, 20000),
            health_checker=self._dead_health(45.0),
            store_forward=False,
        )
        status, _ = ctl.check()
        assert status == 0xC000

    def test_hysteresis_degraded_a700_both_modes(self):
        """Dead < 30s -> 0xA700 regardless of store_forward (hysteresis)."""
        for sf in (True, False):
            ctl = _AdmissionController(
                outbox=_FakeOutbox(0, 20000),
                health_checker=_FakeHealth(healthy=False, dead_seconds=10.0),
                store_forward=sf,
            )
            status, _ = ctl.check()
            assert status == 0xA700, f"store_forward={sf}"

    def test_backpressure_overrides_parking(self):
        """Dead >= 30s + store_forward=on but queue 95% -> 0xC000 (disk limit)."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(19000, 20000),  # 95%
            health_checker=self._dead_health(45.0),
            store_forward=True,
        )
        status, reason = ctl.check()
        assert status == 0xC000
        assert reason == "queue backpressure"

    def test_backpressure_90_95_a700_even_when_park(self):
        """Dead >= 30s + store_forward=on but queue 92% -> 0xA700."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(18400, 20000),  # 92%
            health_checker=self._dead_health(45.0),
            store_forward=True,
        )
        status, _ = ctl.check()
        assert status == 0xA700

    def test_park_healthy_queue_below_70_still_admits(self):
        """Parked dead server but empty queue -> 0x0000 (and thus enqueued)."""
        ctl = _AdmissionController(
            outbox=_FakeOutbox(0, 20000),
            health_checker=self._dead_health(60.0),
            store_forward=True,
        )
        assert ctl.check() == (0x0000, "")
