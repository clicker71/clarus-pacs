# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Batch-7 lifecycle, transport and quoting regressions.

Each test pins one defect from the same change; reverting the fix turns it red.
The platform-dependent ones say how they run on this host.
"""

import ast
import os
import socket
import sys
import threading
from unittest.mock import MagicMock, patch

HERE = os.path.dirname(__file__)
CLEAN_CODE = os.path.join(HERE, "..", "clean_code")


def _main_source():
    return open(os.path.join(CLEAN_CODE, "main.py"), encoding="utf-8").read()


# ---------------------------------------------------------------------------
# T4 - the pre-bind probe uses the listener's own reuse semantics
# ---------------------------------------------------------------------------


class TestPreBindReuse:
    def test_the_probe_sets_the_platform_specific_flag(self):
        """A probe without SO_REUSEADDR reports a port in TIME_WAIT as taken,
        which turns an ordinary restart into a minute-long wait.

        Platform note: the assertion follows `sys.platform` of the host that
        runs the test. On POSIX it requires SO_REUSEADDR; on Windows, where
        SO_REUSEADDR would let a second socket steal a LIVE port, it requires
        SO_EXCLUSIVEADDRUSE instead.
        """
        from clean_code.main import _check_port_available

        calls = []

        class _Sock:
            def __init__(self, *a, **k):
                pass

            def setsockopt(self, level, option, value):
                calls.append((level, option, value))

            def bind(self, address):
                calls.append(("bind", address))

            def close(self):
                pass

        with patch("socket.socket", _Sock):
            _check_port_available(0, "127.0.0.1", "test")

        options = [c[1] for c in calls if isinstance(c[0], int)]
        if sys.platform == "win32":
            exclusive = getattr(socket, "SO_EXCLUSIVEADDRUSE", None)
            if exclusive is not None:
                assert (
                    exclusive in options
                ), f"expected SO_EXCLUSIVEADDRUSE, got {options}"
                assert (
                    socket.SO_REUSEADDR not in options
                ), "SO_REUSEADDR on Windows lets a second instance steal a live port"
        else:
            assert (
                socket.SO_REUSEADDR in options
            ), f"expected SO_REUSEADDR, got {options}"


# ---------------------------------------------------------------------------
# m3 - peers_file and printers_yaml are config-relative
# ---------------------------------------------------------------------------


class TestConfigRelativePaths:
    def test_both_paths_are_resolved_against_the_config_directory(self):
        source = _main_source()
        tree = ast.parse(source)
        main_fn = next(
            n
            for n in ast.walk(tree)
            if isinstance(n, ast.FunctionDef) and n.name == "main"
        )
        keys = None
        for node in ast.walk(main_fn):
            if isinstance(node, ast.Assign) and any(
                getattr(t, "id", None) == "_path_keys" for t in node.targets
            ):
                keys = [e.value for e in node.value.elts]
        assert keys is not None, "_path_keys not found"
        assert "peers_file" in keys, (
            "a relative peers_file resolved from the CWD leaves the C-MOVE peer "
            "lookup empty"
        )
        assert "printers_yaml" in keys, "the print registry has the same problem"


# ---------------------------------------------------------------------------
# m5 - the outbox workers are joined before their queues are closed
# ---------------------------------------------------------------------------


class TestShutdownOrder:
    def test_workers_are_joined_before_the_drain(self):
        source = _main_source()
        tree = ast.parse(source)
        main_fn = next(
            n
            for n in ast.walk(tree)
            if isinstance(n, ast.FunctionDef) and n.name == "main"
        )
        order = []
        for node in ast.walk(main_fn):
            if isinstance(node, ast.For) and "outbox_workers" in ast.dump(node.iter):
                order.append((node.lineno, "join_workers"))
            if isinstance(node, ast.Call):
                name = getattr(node.func, "id", None) or getattr(
                    node.func, "attr", None
                )
                if name in ("_drain_outbox", "close"):
                    order.append((node.lineno, name))
        order.sort()
        names = [n for _, n in order]
        assert "join_workers" in names, f"no worker join loop found: {names}"
        assert names.index("join_workers") < names.index(
            "_drain_outbox"
        ), f"a worker must finish its tail batch before the queue closes: {names}"

    def test_both_queues_are_closed(self):
        source = _main_source()
        # forward_outbox was never closed, so its workdir stayed behind.
        assert "forward_outbox.close()" in source


# ---------------------------------------------------------------------------
# m1/m2 - N-EVENT-REPORT delivery and the logger-level window
# ---------------------------------------------------------------------------


class _FakeAssoc:
    def __init__(self, status):
        self._status = status
        self.is_established = True
        self.accepted_contexts = [MagicMock(abstract_syntax="1.2.840.10008.1.20.1")]

    def send_n_event_report(self, *a, **k):
        return self._status, None

    def release(self):
        pass

    def abort(self):
        pass


def _counter_reading(counter):
    """A counter's current value, through the child the module holds."""
    return counter._value.get()


def _fake_ae_factory(status):
    """An `AE` stand-in whose `associate()` returns an association carrying
    `status` as the N-EVENT-REPORT response."""

    class _AE:
        def __init__(self, *a, **k):
            pass

        def add_requested_context(self, *a, **k):
            pass

        def associate(self, *a, **k):
            return _FakeAssoc(status)

    return _AE


class TestNEVENTEmptyResponse:
    def test_an_unanswered_request_is_not_delivered(self, monkeypatch):
        """PS3.7 9.3 makes the response mandatory.

        A response without a Status is what pynetdicom leaves on an aborted
        association. It must not count as delivered: the delivered counter is
        the observable that separates "we were told" from "we were not", so it
        is what this pins, together with the return value.
        """
        import clean_code.nevent as nevent
        from pydicom import Dataset

        delivered = _counter_reading(nevent.nevent_delivered_total)
        status = Dataset()  # no Status element: an incomplete response
        monkeypatch.setattr(nevent, "AE", _fake_ae_factory(status))
        ok = nevent.send_n_event_report(
            "127.0.0.1",
            11112,
            "MODALITY",
            "1.2.3",
            nevent.EVENT_SUCCESS,
            [("1.2.840.10008.5.1.4.1.1.7", "1.2.3.4")],
            [],
            "CLBRIDGE",
            max_retries=0,
            retry_delay=0,
        )
        assert ok is False, "a response without a Status is not a delivery"
        assert (
            _counter_reading(nevent.nevent_delivered_total) == delivered
        ), "the delivered counter must not move for an unanswered request"

    def test_a_complete_response_is_a_delivery(self, monkeypatch):
        import clean_code.nevent as nevent
        from pydicom import Dataset

        delivered = _counter_reading(nevent.nevent_delivered_total)
        status = Dataset()
        status.Status = 0x0000
        monkeypatch.setattr(nevent, "AE", _fake_ae_factory(status))
        assert nevent.send_n_event_report(
            "127.0.0.1",
            11112,
            "MODALITY",
            "1.2.3",
            nevent.EVENT_SUCCESS,
            [("1.2.840.10008.5.1.4.1.1.7", "1.2.3.4")],
            [],
            "CLBRIDGE",
            max_retries=0,
            retry_delay=0,
        )
        assert _counter_reading(nevent.nevent_delivered_total) == delivered + 1


class TestNeventLevelLock:
    def test_the_restore_is_serialised(self):
        """The save/raise/restore triple is not atomic, so two reporters could
        leave the library silenced for the life of the process."""
        import clean_code.nevent as nevent

        assert hasattr(nevent, "_LEVEL_LOCK"), "the level window has no lock"
        source = open(os.path.join(CLEAN_CODE, "nevent.py"), encoding="utf-8").read()
        tree = ast.parse(source)
        fn = next(
            n
            for n in ast.walk(tree)
            if isinstance(n, ast.FunctionDef) and n.name == "send_n_event_report"
        )
        guarded = any(
            isinstance(n, ast.With)
            and any(
                isinstance(item.context_expr, ast.Name)
                and item.context_expr.id == "_LEVEL_LOCK"
                for item in n.items
            )
            for n in ast.walk(fn)
        )
        assert guarded, "associate() is not called under _LEVEL_LOCK"

    def test_two_reporters_leave_the_level_restored(self):
        import logging

        import clean_code.nevent as nevent

        pn = logging.getLogger("pynetdicom")
        original = pn.level

        def _associate(*a, **k):
            return _FakeAssoc(None)

        with patch.object(nevent, "AE") as ae_cls:
            ae_cls.return_value.associate = _associate
            threads = [
                threading.Thread(
                    target=nevent.send_n_event_report,
                    args=(
                        "127.0.0.1",
                        11112,
                        "MODALITY",
                        "1.2.3",
                        nevent.EVENT_SUCCESS,
                        [("1.2.840.10008.5.1.4.1.1.7", "1.2.3.4")],
                        [],
                        "CLBRIDGE",
                    ),
                    kwargs={"max_retries": 0},
                )
                for _ in range(2)
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
        assert (
            pn.level == original
        ), f"the pynetdicom level stayed at {pn.level} instead of {original}"
