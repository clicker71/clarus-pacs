# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Stage 5 tests: STORE-FORWARD config + worker pause gate.

Covers:
  - [outbox] store_forward parses (true/false/default).
  - _outbox_worker pauses while the server is down (health_checker unhealthy):
    no STOW/HTTP calls, no retry burn; on recovery it drains normally.
"""

import os
import shutil
import tempfile
import threading
import time
from unittest.mock import patch

from clean_code.config import ConfParser
from clean_code.main import _outbox_worker
from pydicom import Dataset

# ---------------------------------------------------------------------------
# Config parse
# ---------------------------------------------------------------------------


class TestStoreForwardConfig:

    def _write_conf(self, text):
        tmp = tempfile.mkdtemp(prefix="conf_sf_")
        path = os.path.join(tmp, "clbridge.conf")
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        return path, tmp

    def test_parses_true(self):
        path, tmp = self._write_conf("[outbox]\nstore_forward = true\n")
        try:
            cfg = ConfParser(path)
            assert cfg.get_bool("outbox", "store_forward", default=False) is True
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def test_parses_false(self):
        path, tmp = self._write_conf("[outbox]\nstore_forward = false\n")
        try:
            cfg = ConfParser(path)
            assert cfg.get_bool("outbox", "store_forward", default=False) is False
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def test_default_false_when_absent(self):
        path, tmp = self._write_conf("[outbox]\nmax_retries = 3\n")
        try:
            cfg = ConfParser(path)
            assert cfg.get_bool("outbox", "store_forward", default=False) is False
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------------------
# Worker pause gate (store_forward)
# ---------------------------------------------------------------------------


class _FlipHealth:
    """HealthChecker stand-in; is_healthy() reflects a mutable flag."""

    def __init__(self):
        self.ok = False
        self.calls = 0

    def is_healthy(self):
        self.calls += 1
        return self.ok


class _FakeShutdown:
    """Return-immediately Event stand-in (so the test needs no real 5s waits)."""

    def __init__(self):
        self._set = False

    def is_set(self):
        return self._set

    def wait(self, timeout=None):
        return self._set

    def set(self):
        self._set = True


def _make_dataset(sop_uid):
    from pydicom.dataset import FileMetaDataset
    from pydicom.uid import ImplicitVRLittleEndian

    ds = Dataset()
    ds.SOPInstanceUID = sop_uid
    ds.PatientName = "Test^Patient"
    ds.StudyInstanceUID = "1.2.840.12345"
    ds.SeriesInstanceUID = "1.2.840.12345.1"
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.file_meta.MediaStorageSOPInstanceUID = sop_uid
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    return ds


class TestWorkerPauseGate:

    def _make_outbox(self):
        tmp = tempfile.mkdtemp(prefix="outbox_sf_")
        from clean_code.outbox import OutboxQueue

        return OutboxQueue(outbox_dir=tmp), tmp

    def test_pauses_while_server_down_no_stow(self):
        """Unhealthy server -> worker makes 0 STOW calls, keeps the SOP pending.

        On recovery the worker drains normally (single mock handles both the
        paused phase - never called - and the drained phase after health flips).
        """
        outbox, tmp = self._make_outbox()
        try:
            outbox.enqueue(_make_dataset("1.2.3.4.5.6.sf1"))
            shutdown = _FakeShutdown()
            health = _FlipHealth()  # ok=False -> paused
            delivered = []
            delivered_event = threading.Event()

            def _fake_batch(payloads, clarus_url, http_timeout, client, tls=None):
                # First delivery: confirm the worker is now past the pause gate.
                health.ok = True
                delivered.extend(u for u, _, _ in payloads)
                delivered_event.set()
                return ([(u, 0x0000, 200, "OK") for u, _, _ in payloads], None)

            with patch("clean_code.main._shutdown_requested", shutdown), patch(
                "clean_code.http_helpers.stow_batch_instances", side_effect=_fake_batch
            ):
                t = threading.Thread(
                    target=_outbox_worker,
                    args=(
                        outbox,
                        "http://127.0.0.1:8024/dicomweb",
                        30,
                        None,
                        None,
                        100,
                        1024 * 1024,
                        0.1,
                        health,
                    ),
                    daemon=True,
                )
                t.start()
                # Let the paused worker spin a bit (fake shutdown returns
                # immediately; the pause gate blocks all delivery).
                time.sleep(0.2)
                assert delivered == [], "worker must not STOW while server down"
                assert outbox.pending_count() == 1, "SOP must stay pending"

                # Recover the server and confirm the worker drains it.
                health.ok = True
                assert delivered_event.wait(timeout=5), "worker never delivered"
                shutdown.set()
                t.join(timeout=5)
            assert delivered == ["1.2.3.4.5.6.sf1"]
            assert outbox.pending_count() == 0, "SOP delivered after recovery"
        finally:
            try:
                outbox.close()
            except Exception:
                pass
            shutil.rmtree(tmp, ignore_errors=True)

    def test_no_health_checker_drains_normally(self):
        """health_checker=None -> worker does not pause (back-compat)."""
        outbox, tmp = self._make_outbox()
        try:
            outbox.enqueue(_make_dataset("1.2.3.4.5.6.sf2"))
            shutdown = _FakeShutdown()
            done = threading.Event()

            def _fake_batch(payloads, clarus_url, http_timeout, client, tls=None):
                done.set()
                return ([(uid, 0x0000, 200, "OK") for uid, _, _ in payloads], None)

            with patch("clean_code.main._shutdown_requested", shutdown), patch(
                "clean_code.http_helpers.stow_batch_instances", side_effect=_fake_batch
            ):
                t = threading.Thread(
                    target=_outbox_worker,
                    args=(
                        outbox,
                        "http://127.0.0.1:8024/dicomweb",
                        30,
                        None,
                        None,
                        100,
                        1024 * 1024,
                        0.1,
                        None,
                    ),  # no health checker
                    daemon=True,
                )
                t.start()
                assert done.wait(timeout=5), "worker should drain without health"
                shutdown.set()
                t.join(timeout=5)
            assert outbox.pending_count() == 0
        finally:
            try:
                outbox.close()
            except Exception:
                pass
            shutil.rmtree(tmp, ignore_errors=True)
