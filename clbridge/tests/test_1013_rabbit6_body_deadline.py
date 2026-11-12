# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1013 rabbit pass 6, B1 - the /print body read has no overall deadline.

`BridgeHTTPHandler.timeout` bounds ONE socket operation. A peer that announces
a 100 MB Content-Length and then sends a byte just inside every timeout window
keeps every `recv` succeeding while `rfile.read(Content-Length)` never returns,
so one ThreadingHTTPServer thread is held indefinitely - the exact scenario the
module docstring says the timeout prevents.
"""

import json

from clean_code import http_server as hs


class _TrickleRfile:
    """One byte per call, never short of data: `read(n)` cannot complete."""

    def __init__(self):
        self.calls = 0

    def read(self, n=-1):
        self.calls += 1
        return b"x"


class _TruncatedRfile:
    """Announces more than it delivers, then reports EOF."""

    def __init__(self, data=b""):
        self._data = data

    def read(self, n=-1):
        data, self._data = self._data, b""
        return data


class _ShortRfile:
    """A well-behaved body: everything in one call."""

    def __init__(self, data):
        self._data = data

    def read(self, n=-1):
        data, self._data = self._data[:n], self._data[n:]
        return data


def _handler(rfile, content_length, captured, advance=None):
    from clean_code.http_server import BridgeHTTPHandler

    h = BridgeHTTPHandler.__new__(BridgeHTTPHandler)
    h.path = "/print"
    h.headers = {
        "Content-Type": "application/json",
        "Content-Length": str(content_length),
    }
    h.rfile = rfile
    h.printer_registry = {}
    h.driver_registry = {}
    h.bridge_ae_title = "TEST"
    h.send_error = lambda code, msg=None: captured.update(
        code=code, msg=msg or "", via="send_error"
    )
    h._json_error = lambda code, msg: captured.update(code=code, msg=msg)
    h.send_response = lambda *a, **k: None
    h.send_header = lambda *a, **k: None
    h.end_headers = lambda: None
    h.log_message = lambda *a, **k: None
    return h


def _clock(monkeypatch, step):
    """A monotonic clock that advances `step` seconds on each read of rfile.

    Returned as a bound-style object: `read` counts calls and moves the clock,
    so every individual read succeeds and only the deadline can stop the loop.
    """
    state = {"now": 1000.0}

    class _Trickle:
        def read(self, n=-1):
            state["now"] += step
            return b"x"

    monkeypatch.setattr(hs.time, "monotonic", lambda: state["now"])
    return _Trickle()


class TestB1BodyDeadline:
    def test_a_trickling_body_hits_the_deadline_and_answers_408(self, monkeypatch):
        """Every single recv succeeds; only the deadline stops the handler."""
        captured = {}
        h = _handler(_clock(monkeypatch, 1.0), 100 * 1024 * 1024, captured)
        h.do_POST()
        assert captured["code"] == 408, captured
        assert "timed out" in captured["msg"], captured

    def test_a_body_that_ends_early_answers_400(self):
        """A peer that promised a length and left answers 400, not a hang."""
        captured = {}
        h = _handler(_TruncatedRfile(b"{}"), 4096, captured)
        h.do_POST()
        assert captured["code"] == 400, captured
        assert "truncated" in captured["msg"].lower(), captured

    def test_the_deadline_is_a_module_constant_above_the_socket_timeout(self):
        """Named, and strictly larger than one socket operation - otherwise a
        legitimate large body is cut off by the total bound."""
        assert hs.BODY_TOTAL_TIMEOUT_SEC == 120
        assert hs.BODY_TOTAL_TIMEOUT_SEC > hs.HTTP_SOCKET_TIMEOUT_SEC

    def test_a_normal_short_body_still_answers(self, monkeypatch):
        """Regression guard: the deadline must not change the happy path."""
        captured = {}
        payload = json.dumps({"printer_name": "P1", "images": []}).encode()
        h = _handler(_ShortRfile(payload), len(payload), captured)
        h.do_POST()
        # Reaches the registry lookup and answers "not found", i.e. the body
        # was read and parsed, not rejected by the reader.
        assert captured["code"] == 404, captured
        assert "P1" in captured["msg"], captured

    def test_an_empty_body_leaves_the_deadline_unused(self, monkeypatch):
        """Content-Length: 0 is a valid body: no read, no deadline, and the
        JSON parse of b"" decides the answer."""
        captured = {}
        h = _handler(_TruncatedRfile(b""), 0, captured)
        h.do_POST()
        assert captured["code"] == 400, captured
        assert "Invalid JSON" in captured["msg"], captured
