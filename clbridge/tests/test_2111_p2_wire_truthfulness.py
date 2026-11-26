# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P2 wire truthfulness - one test per finding, each fails when reverted.

The rule under test is the same in all five: the bridge must never report
success when the answer is wrong, unreadable or truncated.
"""

import json
import os
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest
from clean_code import http_helpers as hh
from clean_code.streaming import (
    InvalidDicomPartError,
    MultipartFramingError,
    _stream_multipart_instances,
)

BOUNDARY = "p2truth11"
URL = "http://origin.test/dicomweb/studies"
DICM = b"\x00" * 128 + b"DICM"


def _dicom(size=200, seed=0):
    """A body with the PS3.10 7.1 prefix at offset 128."""
    assert size >= 132
    body = bytearray(size)
    body[128:132] = b"DICM"
    for i in range(132, size):
        body[i] = (i + seed) % 251 + 1
    return bytes(body)


def _part(body, ts="1.2.840.10008.1.2.1"):
    return (
        f"--{BOUNDARY}\r\n"
        f"Content-Type: application/dicom; transfer-syntax={ts}\r\n"
        f"Content-Length: {len(body)}\r\n\r\n"
    ).encode("ascii") + body


def _close():
    return f"\r\n--{BOUNDARY}--\r\n".encode("ascii")


class _Resp:
    """Minimal requests.Response stand-in for the multipart parser."""

    def __init__(self, chunks):
        self._chunks = chunks
        self.headers = {
            "Content-Type": (
                f'multipart/related; type="application/dicom"; boundary={BOUNDARY}'
            )
        }

    def iter_content(self, chunk_size=65536):
        yield from self._chunks


# ---------------------------------------------------------------------------
# streaming.py:297 - an invalid DICOM part is a transfer error
# ---------------------------------------------------------------------------
def test_an_invalid_part_is_a_transfer_error_not_a_silent_skip():
    """A valid part A followed by a body without DICM used to be reported as
    Success for A alone - the origin said it sent B, and B went nowhere."""
    good = _dicom(400, seed=1)
    bad = b"\x00" * 400  # no magic
    wire = (
        _part(good)
        + f"\r\n--{BOUNDARY}\r\n".encode()
        + _part(bad)[len(f"--{BOUNDARY}\r\n") :]
        + _close()
    )
    out = []
    with pytest.raises(InvalidDicomPartError):
        for item in _stream_multipart_instances(_Resp([wire])):
            out.append(item)
    # A was genuinely delivered before the defect surfaced: the caller keeps it
    # and resumes the remainder, which is why this is not swallowed silently.
    assert len(out) == 1
    assert out[0][0] == good


def test_a_valid_stream_still_completes_without_error():
    """The guard must not fire on a well-formed stream (falsification pair)."""
    a, b = _dicom(300, seed=1), _dicom(500, seed=2)
    wire = (
        _part(a)
        + f"\r\n--{BOUNDARY}\r\n".encode()
        + _part(b)[len(f"--{BOUNDARY}\r\n") :]
        + _close()
    )
    assert [p for p, _i, _t in _stream_multipart_instances(_Resp([wire]))] == [a, b]


# ---------------------------------------------------------------------------
# streaming.py:323 - EOF before the closing delimiter is a framing failure
# ---------------------------------------------------------------------------
def test_eof_before_the_closing_delimiter_is_a_framing_error():
    """The origin sent A and stopped halfway through B. PS3.18 8.7.2 requires
    the closing delimiter; without it the answer is truncated, not short."""
    a = _dicom(400, seed=1)
    b = _dicom(4000, seed=2)
    wire = (
        _part(a)
        + f"\r\n--{BOUNDARY}\r\n".encode()
        + _part(b)[len(f"--{BOUNDARY}\r\n") :]
    )
    wire = wire[: len(wire) - 200]  # cut mid-part B, no closing delimiter
    out = []
    with pytest.raises(MultipartFramingError):
        for item in _stream_multipart_instances(_Resp([wire])):
            out.append(item)
    assert [p for p, _i, _t in out] == [a], "parts already delivered are kept"


def test_a_single_part_with_a_closing_delimiter_is_not_a_framing_error():
    a = _dicom(400, seed=1)
    list(_stream_multipart_instances(_Resp([_part(a) + _close()])))


# ---------------------------------------------------------------------------
# http_helpers.py:227 - non-object rows never reach a converter
# ---------------------------------------------------------------------------
def _qido(monkeypatch, payload, headers=None, limit=None):
    class _Client:
        def get(self, *a, **k):
            return _QidoResponse(payload, headers)

        def close(self):
            pass

    monkeypatch.setattr(hh, "_http_retry", lambda fn, **_kwargs: fn())
    monkeypatch.setattr(hh.httpx, "Client", lambda **k: _Client())
    return hh._qido_request(URL, {}, limit=limit)


class _QidoResponse:
    def __init__(self, payload, headers=None):
        self.status_code = 200
        self.content = payload if isinstance(payload, bytes) else payload.encode()
        self.headers = headers or {}
        self._payload = payload

    def json(self):
        return json.loads(self._payload)

    def raise_for_status(self):
        pass


@pytest.mark.parametrize("payload", ["[null]", '["a", "b"]', "[[]]", "[{}, null]"])
def test_a_non_object_qido_row_is_a_malformed_body(monkeypatch, payload):
    """`[null]` passed the array check and then `.items()` raised outside the
    C-FIND error handler, escaping as a tooling failure instead of 0xA700."""
    with pytest.raises(hh.QidoMalformedBodyError):
        _qido(monkeypatch, payload)


def test_an_empty_array_is_still_a_valid_empty_result(monkeypatch):
    answer = _qido(monkeypatch, "[]")
    assert answer.rows == []
    assert answer == []
    assert answer.more is False


# ---------------------------------------------------------------------------
# http_helpers.py:233 - continuation information survives the request
# ---------------------------------------------------------------------------
def test_a_warning_299_marks_the_page_as_continuing(monkeypatch):
    """An origin may answer one row plus `Warning: 299` even when eleven were
    asked for. Returning only the array lost that, so a C-FIND caller could
    send one Pending row and then claim Success for an incomplete search."""
    body = json.dumps([{"00080018": {"Value": ["1.2.3"]}}])
    answer = _qido(
        monkeypatch,
        body,
        headers={"Warning": '299 origin "did not complete"'},
        limit=11,
    )
    assert len(answer.rows) == 1
    assert answer.more is True


def test_a_short_page_without_the_warning_is_complete(monkeypatch):
    body = json.dumps([{"00080018": {"Value": ["1.2.3"]}}])
    assert _qido(monkeypatch, body, limit=11).more is False


def test_a_warn_code_that_only_starts_with_299_is_not_the_warning(monkeypatch):
    """The 299 warn-code is a whole token; `2990` and a message containing
    `299` are different warnings, and a substring test would misread them."""
    body = json.dumps([{"00080018": {"Value": ["1.2.3"]}}])
    answer = _qido(
        monkeypatch, body, headers={"Warning": '2990 origin "299 rows"'}, limit=11
    )
    assert answer.more is False


def test_c_find_reports_incomplete_when_the_origin_still_has_more(monkeypatch):
    """End to end through the handler and the real transport: a page the
    origin flagged with `Warning: 299`, at the caller's cap, must never answer
    0x0000. The flag is produced by `_qido_continuation`, so neutering that
    helper turns this red too.

    P9: the status is 0xA700, not 0xB001 - PS3.4 Table C.4.1-1 does not define
    0xB001 for Query/Retrieve, so an answer the bridge could not complete is
    the defined "Refused: Out of Resources".
    """
    from clean_code.handlers import cfind

    from tests.conftest import make_fake_find_event

    rows = [{"0020000D": {"Value": ["1.2.3"]}, "00100010": {"Value": ["A"]}}]
    # One row, a limit of two, and the origin warning that it did not finish:
    # the row count alone would say "complete".
    monkeypatch.setattr(
        hh,
        "_http_retry",
        lambda fn, **_kwargs: _QidoResponse(
            json.dumps(rows), {"Warning": '299 origin "did not complete"'}
        ),
    )
    event = make_fake_find_event()
    event.identifier.QueryRetrieveLevel = "STUDY"
    out = list(
        cfind._handle_qido_search(
            event,
            "http://127.0.0.1:1/clarus/dicomweb",
            False,
            1,
            5,
            {},
            "study",
        )
    )
    assert out[-1][0] == 0xA700
    assert [s for s, _ in out[:-1]] == [0xFF00]


# ---------------------------------------------------------------------------
# http_helpers.py:601 - a partial STOW is never acknowledged
# ---------------------------------------------------------------------------
class _StowResp:
    def __init__(self, status_code, text):
        self.status_code = status_code
        self.text = text
        self.content = text.encode()

    def json(self):
        return json.loads(self.text)


def _batch(monkeypatch, resp, payloads):
    monkeypatch.setattr(hh, "_http_retry", lambda fn, **_kwargs: resp)

    class _Client:
        def post(self, *a, **k):
            return resp

    return hh.stow_batch_instances(
        payloads, "http://127.0.0.1:1/dicomweb", client=_Client()
    )


def test_an_unreadable_202_verdict_is_not_an_acknowledgement(monkeypatch):
    """HTTP 202 with a body the bridge cannot read does NOT establish that any
    SOP was stored. Returning None made the worker mark every SOP delivered."""
    resp = _StowResp(202, "<html>proxy</html>")
    results, _backoff = _batch(monkeypatch, resp, [("1.2.3.A", None, b"a")])
    assert len(results) == 1
    uid, status, http_status, msg = results[0]
    assert uid == "1.2.3.A"
    assert status == 0xA700, "an unknown outcome must be retried"
    assert http_status == 202
    assert hh.STOW_UNVERIFIED in msg


def test_a_readable_202_with_a_partial_reject_keeps_the_verdicts(monkeypatch):
    """PS3.18 10.5.2: 202 may carry a Failed SOP Sequence. The accepted SOP
    stays 0x0000 and the rejected one 0xA700, both in payload order.

    The accepted SOP is named in the Referenced SOP Sequence (0008,1199): on a
    202 that sequence IS the origin's statement that the instance was stored
    (10.5.3), and an instance the answer does not account for is unverified
    rather than delivered.
    """
    body = json.dumps(
        {
            "00081199": {"Value": [{"00081155": {"Value": ["1.2.3.A"]}}]},
            "00081198": {
                "Value": [
                    {
                        "00081155": {"Value": ["1.2.3.B"]},
                        "00081197": {"Value": [272]},
                        "00000902": {"Value": ["nope"]},
                    }
                ]
            },
        }
    )
    results, _b = _batch(
        monkeypatch,
        _StowResp(202, body),
        [("1.2.3.A", None, b"a"), ("1.2.3.B", None, b"b")],
    )
    assert [(r[0], r[1]) for r in results] == [("1.2.3.A", 0x0000), ("1.2.3.B", 0xA700)]


def test_batch_verdicts_stay_in_payload_order(monkeypatch):
    """For payloads [A, B] with B failing serialization, the list is
    [A, B] - not [B failure, A success]."""
    body = json.dumps({})
    results, _b = _batch(
        monkeypatch,
        _StowResp(202, body),
        [("1.2.3.A", None, b"a"), ("1.2.3.B", None, b"b")],
    )
    assert [r[0] for r in results] == ["1.2.3.A", "1.2.3.B"]


def test_a_serialize_failure_keeps_the_input_order(monkeypatch):
    """Non-vacuity for the order rule: the failing part is SECOND, so the
    verdict list must place it second rather than hoisting it to the front."""
    sent = {}

    class _Bad:
        SOPInstanceUID = "1.2.3.BAD"

        def __init__(self):
            self.file_meta = type(
                "M", (), {"TransferSyntaxUID": "1.2.840.10008.1.2.1"}
            )()

    class _Good:
        SOPInstanceUID = "1.2.3.GOOD"

        def __init__(self):
            self.file_meta = type(
                "M", (), {"TransferSyntaxUID": "1.2.840.10008.1.2.1"}
            )()

    def _write(fp, ds, **kwargs):
        if getattr(ds, "SOPInstanceUID", "") == "1.2.3.BAD":
            raise ValueError("invalid value for VR US")
        fp.write(b"GOOD-BYTES")

    monkeypatch.setattr(hh, "dcmwrite", _write)
    # The origin states what it stored, so the subject here stays ORDER: a 202
    # with no per-SOP verdict would leave the good instance unverified
    # (PS3.18 10.5.3) and the assertion below would be about the verdict rather
    # than about the position of each part.
    monkeypatch.setattr(
        hh,
        "_http_retry",
        lambda fn, **_kwargs: sent.setdefault(
            "r",
            _StowResp(
                202,
                json.dumps(
                    {"00081199": {"Value": [{"00081155": {"Value": ["1.2.3.GOOD"]}}]}}
                ),
            ),
        ),
    )
    payloads = [("1.2.3.GOOD", _Good(), None), ("1.2.3.BAD", _Bad(), None)]
    results, _b = hh.stow_batch_instances(payloads, "http://127.0.0.1:1/dicomweb")
    assert [r[0] for r in results] == ["1.2.3.GOOD", "1.2.3.BAD"]
    assert [r[1] for r in results] == [0x0000, 0xC000]


# ---------------------------------------------------------------------------
# http_helpers.py - only enumerated Storage Commitment reasons reach the wire
# ---------------------------------------------------------------------------
def test_an_unenumerated_storage_commitment_reason_becomes_0x0110():
    item = {
        "00081150": {"vr": "UI", "Value": ["1.2.840.10008.5.1.4.1.1.2"]},
        "00081155": {"vr": "UI", "Value": ["1.2.3"]},
        "00081197": {"vr": "US", "Value": [65535]},
    }
    assert hh._stc_reason(item) == 0x0110


def test_an_enumerated_decimal_reason_is_preserved():
    """274 is the decimal spelling of 0x0112 and must stay 0x0112."""
    item = {
        "00081150": {"vr": "UI", "Value": ["1.2.840.10008.5.1.4.1.1.2"]},
        "00081155": {"vr": "UI", "Value": ["1.2.3"]},
        "00081197": {"vr": "US", "Value": [274]},
    }
    assert hh._stc_reason(item) == 0x0112


# ---------------------------------------------------------------------------
# The same two defects driven over loopback, not through a mock
# ---------------------------------------------------------------------------
class _TruncatingOrigin(BaseHTTPRequestHandler):
    """Sends one complete part, then stops mid-way through the second."""

    mode = "truncate"

    def log_message(self, *a):
        pass

    def do_GET(self):
        a = _dicom(2048, seed=1)
        b = _dicom(8192, seed=2)
        body = (
            _part(a)
            + f"\r\n--{BOUNDARY}\r\n".encode()
            + _part(b)[len(f"--{BOUNDARY}\r\n") :]
        )
        if self.mode == "invalid":
            bad = b"\x00" * 2048
            body = (
                _part(a)
                + f"\r\n--{BOUNDARY}\r\n".encode()
                + _part(bad)[len(f"--{BOUNDARY}\r\n") :]
                + _close()
            )
        else:
            body = body[: len(body) - 300]
        self.send_response(200)
        self.send_header(
            "Content-Type",
            f'multipart/related; type="application/dicom"; boundary={BOUNDARY}',
        )
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def _origin(mode):
    _TruncatingOrigin.mode = mode
    httpd = HTTPServer(("127.0.0.1", 0), _TruncatingOrigin)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


def test_a_real_truncated_body_raises_over_the_wire():
    import requests

    httpd = _origin("truncate")
    try:
        resp = requests.get(
            f"http://127.0.0.1:{httpd.server_address[1]}/studies",
            headers={"Accept": "multipart/related; type=application/dicom"},
            stream=True,
            timeout=10,
        )
        with pytest.raises(MultipartFramingError):
            list(_stream_multipart_instances(resp))
    finally:
        httpd.shutdown()
        httpd.server_close()


def test_a_real_invalid_part_raises_over_the_wire():
    import requests

    httpd = _origin("invalid")
    try:
        resp = requests.get(
            f"http://127.0.0.1:{httpd.server_address[1]}/studies",
            headers={"Accept": "multipart/related; type=application/dicom"},
            stream=True,
            timeout=10,
        )
        with pytest.raises(InvalidDicomPartError):
            list(_stream_multipart_instances(resp))
    finally:
        httpd.shutdown()
        httpd.server_close()


def test_no_spill_files_leak_from_the_two_errors():
    import glob
    import tempfile

    before = set(glob.glob(os.path.join(tempfile.gettempdir(), "clbridge_spill_*")))
    a = _dicom(400, seed=1)
    wire = _part(a)
    with pytest.raises(MultipartFramingError):
        list(_stream_multipart_instances(_Resp([wire])))
    after = set(glob.glob(os.path.join(tempfile.gettempdir(), "clbridge_spill_*")))
    assert after - before == set()
