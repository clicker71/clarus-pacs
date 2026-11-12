# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Outbox watchdog: the stall branch must be reachable.

The idle loop is:

    wait = outbox.next_retry_wait_sec()
    _shutdown_requested.wait(timeout=wait)
    dead_sec = time.time() - _last_progress   # age of the PREVIOUS heartbeat
    _last_progress = time.time()              # refresh it
    if dead_sec > _OUTBOX_STALL_SEC and outbox.pending_count() > 0:
        outbox.recover_stuck()

The original code wrote the heartbeat BEFORE measuring it, so dead_sec was the
interval between two adjacent ``time.time()`` calls - microseconds on every
pass - and the threshold branch could never be taken, however long the worker
had been stuck.

Both facts matter for the test: the idle wait has to really block (a stub that
ignores ``timeout`` can never produce a long pass) and the threshold has to be
reachable, so it is lowered from the production 120s.

``clean_code.main.time`` is the stdlib ``time`` module itself, so these tests
never patch ``time.time`` - that would break httpx, logging and threading too.
"""

import shutil
import tempfile
import threading
import time
from unittest.mock import patch

from clean_code.main import _outbox_worker
from clean_code.outbox import OutboxQueue

# Production threshold is 120s; shortened so the test does not wait two minutes.
_TEST_STALL_SEC = 0.12
# One idle pass that really blocks for this long must trip the watchdog.
_STALL_WAIT_SEC = 0.30


class _SleepingShutdown:
    """Event stand-in that honours the requested timeout (really sleeps).

    `wait()` really sleeps so the worker's idle branch behaves like the
    production one, but it must also be able to STOP a running worker: the
    test joins the thread before restoring its patches, and a wait that never
    returned would make that join hang or force a leak.
    """

    def __init__(self):
        self._set = False
        self._wake = threading.Event()

    def is_set(self):
        return self._set

    def wait(self, timeout=None):
        # Wake immediately on a stop request, otherwise wait the full slice.
        self._wake.wait(timeout)
        return self._set

    def set(self):
        self._set = True
        self._wake.set()

    def request_stop(self):
        self.set()


class _StallingOutbox(OutboxQueue):
    """Empty queue whose idle wait blocks on one specific pass."""

    STALL_ON_PASS = 1

    def __init__(self, outbox_dir):
        super().__init__(outbox_dir=outbox_dir)
        self.recoveries = 0
        self.idle_passes = 0

    def dequeue(self):
        return (None, None, None, None)

    def pending_count(self):
        return 1

    def recover_stuck(self):
        self.recoveries += 1
        return (0, 0)

    def next_retry_wait_sec(self):
        self.idle_passes += 1
        if self.idle_passes == self.STALL_ON_PASS:
            return _STALL_WAIT_SEC
        return 0.01


class _HealthyOutbox(_StallingOutbox):
    """Never stalls: every idle wait stays below the threshold."""

    STALL_ON_PASS = -1


def _run_worker(outbox, stall_sec, duration):
    """Run the worker for `duration` seconds, then STOP it before the patches
    are restored.

    Restoring the patches while the worker thread is still running raced the
    worker against its own configuration: it could read the REAL
    `_OUTBOX_STALL_SEC` (unpatched) mid-pass and take a branch the test never
    intended, which made the "no recovery" assertion flaky under load. The
    worker is therefore signalled to stop and joined INSIDE the patch context,
    so every line it executes runs under the configuration the test set up.
    """
    shutdown = _SleepingShutdown()
    with patch("clean_code.main._shutdown_requested", shutdown), patch(
        "clean_code.main._OUTBOX_STALL_SEC", stall_sec
    ), patch("clean_code.outbox.STUCK_ITEM_CHECK_INTERVAL", 1e9):
        worker = threading.Thread(
            target=_outbox_worker,
            args=(
                outbox,
                "http://127.0.0.1:8024/dicomweb",
                30,
                None,
                None,
                100,
                1024 * 1024,
                0.01,
                None,
            ),
            daemon=True,
        )
        worker.start()
        time.sleep(duration)
        shutdown.request_stop()
        worker.join(timeout=10)
        assert not worker.is_alive(), "the watchdog worker did not stop"


class TestWatchdogStallBranch:

    def setup_method(self):
        self.tmp = tempfile.mkdtemp(prefix="outbox_wd_")

    def teardown_method(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_stalled_pass_triggers_recovery(self):
        """A pass that blocks longer than the threshold runs recover_stuck().

        With the heartbeat written before the measurement, dead_sec is ~0 on
        every pass and this never happens.
        """
        outbox = _StallingOutbox(self.tmp)
        try:
            _run_worker(outbox, _TEST_STALL_SEC, duration=1.0)
            assert outbox.recoveries > 0, (
                "a pass that blocked for %.2fs exceeded the %.2fs threshold "
                "but the watchdog did not fire; it is measuring the heartbeat "
                "it has just written" % (_STALL_WAIT_SEC, _TEST_STALL_SEC)
            )
        finally:
            try:
                outbox.close()
            except Exception:
                pass

    def test_healthy_idle_never_recovers(self):
        """Passes that all finish below the threshold must not fire."""
        outbox = _HealthyOutbox(self.tmp)
        try:
            _run_worker(outbox, _TEST_STALL_SEC, duration=0.5)
            assert outbox.idle_passes >= 3, (
                "the idle branch barely ran (%d passes)" % outbox.idle_passes
            )
            assert outbox.recoveries == 0, (
                "watchdog fired although no pass exceeded the threshold"
            )
        finally:
            try:
                outbox.close()
            except Exception:
                pass
