# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""W3 (0209) spill-to-file - parser and send-loop unit tests.

The multipart parser keeps the first spill_threshold bytes of a part in
memory and streams the excess into a temp file (SpilledPart). The send
loop appends that file to the C-STORE temp file in chunks and unlinks it
on every exit branch (success, refusal, cancel, shutdown).
"""

import glob
import os
import shutil
import struct
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from unittest.mock import MagicMock, patch

import pytest
from clean_code.handlers.cmove import _stream_and_send_study
from clean_code.streaming import (
    SpilledPart,
    _stream_multipart_instances,
)
from pydicom import Dataset

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

BOUNDARY = "spilltest42"
EVRLE = "1.2.840.10008.1.2.1"
CT = "1.2.840.10008.5.1.4.1.1.2"
STUDY = "2.25.1111222233334444"
SOP = "2.25.9990001112223331"

# Deterministic pixel filler: a repeating 251-byte block (all bytes non-zero).
_PIX = bytes(range(1, 252))


def _ui(value):
    v = value.encode("ascii")
    return v + b"\x00" if len(v) % 2 else v


def _body(size, sop_uid=SOP, ts=EVRLE):
    """DICOM-like body of exactly `size` bytes with a canonical trio meta."""
    out = b""
    for elem, value in ((0x0002, CT), (0x0003, sop_uid), (0x0010, ts)):
        vv = _ui(value)
        out += struct.pack("<HH2sH", 0x0002, elem, b"UI", len(vv)) + vv
    for group, elem, value in ((0x0008, 0x0016, CT), (0x0008, 0x0018, sop_uid)):
        vv = _ui(value)
        out += struct.pack("<HH", group, elem) + b"UI" + struct.pack("<H", len(vv)) + vv
    out += struct.pack("<HH", 0x7FE0, 0x0010) + b"OW" + b"\x00\x00"
    pixels = size - (132 + len(out) + 4)
    assert pixels >= 0 and pixels % 2 == 0, (size, 132 + len(out) + 4)
    out += struct.pack("<I", pixels)
    out += (_PIX * (pixels // 251 + 1))[:pixels]
    return b"\x00" * 128 + b"DICM" + out


def _part_bytes(body):
    return (
        f"--{BOUNDARY}\r\n"
        f"Content-Type: application/dicom; transfer-syntax={EVRLE}\r\n"
        f"Content-Length: {len(body)}\r\n\r\n"
    ).encode("ascii") + body


def _final():
    return f"\r\n--{BOUNDARY}--\r\n".encode("ascii")


def _spill_files():
    return glob.glob(os.path.join(tempfile.gettempdir(), "clbridge_spill_*"))


class _MockResponse:
    """Mock requests.Response for _stream_multipart_instances tests."""

    def __init__(self, chunks, boundary=BOUNDARY):
        self._chunks = chunks
        self.headers = {
            "Content-Type": (
                f'multipart/related; type="application/dicom"; boundary={boundary}'
            )
        }

    def iter_content(self, chunk_size=65536):
        yield from self._chunks


# ---------------------------------------------------------------------------
# Parser: spill decision, byte-exact reassembly, cleanup
# ---------------------------------------------------------------------------


def test_small_part_stays_bytes():
    """Parts within the threshold are yielded as plain bytes (unchanged)."""
    body = _body(200 * 1024)
    resp = _MockResponse([_part_bytes(body) + _final()])
    results = list(_stream_multipart_instances(resp))
    assert len(results) == 1
    part, idx, ts = results[0]
    assert isinstance(part, bytes)
    assert part == body


def test_part_at_threshold_stays_bytes():
    """Content-Length == spill_threshold is NOT spilled (strict >)."""
    threshold = 256 * 1024
    body = _body(threshold)
    resp = _MockResponse([_part_bytes(body) + _final()])
    results = list(_stream_multipart_instances(resp, spill_threshold=threshold))
    assert len(results) == 1
    assert isinstance(results[0][0], bytes)


def test_large_part_spills_and_reassembles_byte_exact():
    """Head capped at the threshold, excess on disk, reassembly exact."""
    body = _body(2 * 1024 * 1024)
    threshold = 512 * 1024
    resp = _MockResponse([_part_bytes(body) + _final()])
    results = list(_stream_multipart_instances(resp, spill_threshold=threshold))
    assert len(results) == 1
    part, idx, ts = results[0]
    assert isinstance(part, SpilledPart)
    assert part.head == body[:threshold]
    assert ts == EVRLE
    try:
        assert os.path.getsize(part.path) == len(body) - threshold
        with open(part.path, "rb") as f:
            assert f.read() == body[threshold:]
    finally:
        if os.path.exists(part.path):
            os.unlink(part.path)


def test_spill_file_cleaned_when_stream_truncated():
    """A truncated oversized part yields nothing and leaves no temp file."""
    body = _body(2 * 1024 * 1024)
    threshold = 512 * 1024
    before = set(_spill_files())
    wire = _part_bytes(body)
    resp = _MockResponse([wire[: len(wire) // 2]])
    results = list(_stream_multipart_instances(resp, spill_threshold=threshold))
    assert results == []
    assert set(_spill_files()) - before == set()


def test_spilled_invalid_dicom_part_is_skipped_and_unlinked():
    """An oversized part without DICM magic is skipped and its spill file
    is dropped - no consumer ever sees it, so nothing leaks."""
    body = (
        b"\x00" * 128
        + b"XXXX"
        + (_PIX * (2 * 1024 * 1024 // 251 + 1))[: 2 * 1024 * 1024]
    )
    threshold = 512 * 1024
    before = set(_spill_files())
    resp = _MockResponse([_part_bytes(body) + _final()])
    results = list(_stream_multipart_instances(resp, spill_threshold=threshold))
    assert results == []
    assert set(_spill_files()) - before == set()


def test_tripwire_fires_for_oversized_part_without_spill():
    """Tripwire stays the guard when spill is disabled (threshold above the
    part size): a >100 MB part raises instead of growing unbounded."""
    body_size = 120 * 1024 * 1024
    headers = (
        f"--{BOUNDARY}\r\n"
        "Content-Type: application/dicom\r\n"
        f"Content-Length: {body_size}\r\n\r\n"
    ).encode("ascii")
    chunk = b"\x01" * (8 * 1024 * 1024)
    resp = _MockResponse([headers + chunk] + [chunk] * 13)  # 104 MB buffered
    with pytest.raises(ValueError, match="Multipart buffer exceeded"):
        list(
            _stream_multipart_instances(
                resp,
                spill_threshold=150 * 1024 * 1024,
            )
        )


# ---------------------------------------------------------------------------
# Send loop: SpilledPart handling, chunked append, unlink branches
# ---------------------------------------------------------------------------

SOP_REGISTRY = {CT: {"accept": True}}


class _FakeMoveJobs:
    def is_job_cancelled(self, job_id):
        return False

    def update_job_progress(self, job_id, completed, failed, total):
        pass


def _status(code):
    ds = Dataset()
    ds.Status = code
    return ds


class _FakeClarus(BaseHTTPRequestHandler):
    """Serves ONE study multipart body for any GET (stream loop only)."""

    body = b""
    boundary = BOUNDARY

    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header(
            "Content-Type",
            f'multipart/related; type="application/dicom"; boundary={self.boundary}',
        )
        self.send_header("Content-Length", str(len(self.body)))
        self.end_headers()
        self.wfile.write(self.body)


def _run_send(assoc, body, threshold):
    _FakeClarus.body = _part_bytes(body) + _final()
    httpd = HTTPServer(("127.0.0.1", 0), _FakeClarus)
    port = httpd.server_address[1]
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    try:
        return _stream_and_send_study(
            assoc=assoc,
            study_uid=STUDY,
            clarus_url=f"http://127.0.0.1:{port}/dicomweb",
            http_timeout=10,
            move_jobs=_FakeMoveJobs(),
            job_id=1,
            accepted_contexts_map={CT: EVRLE},
            server_ts_list=[EVRLE],
            sop_registry=SOP_REGISTRY,
            spill_threshold=threshold,
        )
    finally:
        httpd.shutdown()
        httpd.server_close()


def test_send_loop_streams_spilled_part_in_chunks_and_unlinks_on_success():
    """A SpilledPart is appended to the send file in 1 MiB chunks and the
    spill file is unlinked after a successful C-STORE."""
    body = _body(2 * 1024 * 1024)
    threshold = 256 * 1024
    sent = []
    assoc = MagicMock()

    def _fake_send(path):
        with open(path, "rb") as f:
            sent.append(f.read())
        return _status(0x0000)

    assoc.send_c_store.side_effect = _fake_send
    before = set(_spill_files())
    with patch(
        "clean_code.handlers.cmove.shutil.copyfileobj", wraps=shutil.copyfileobj
    ) as copy_mock:
        completed, failed, total, sent_sops, _failed_sops, cancelled, no_common = (
            _run_send(
                assoc,
                body,
                threshold,
            )
        )
    assert (completed, failed, total) == (1, 0, 1)
    assert (cancelled, no_common) == (False, False)
    assert sent_sops == [(CT, SOP)]
    assert sent == [body]  # head + spill reassembled byte-exact
    assert copy_mock.call_count == 1
    assert copy_mock.call_args.args[2] == 1024 * 1024  # chunked copy
    assert set(_spill_files()) - before == set()


def test_send_loop_unlinks_spill_file_on_refusal():
    """The spill file is unlinked when the viewer refuses the C-STORE."""
    body = _body(2 * 1024 * 1024)
    threshold = 256 * 1024
    assoc = MagicMock()
    assoc.send_c_store.return_value = _status(0xA700)  # Out of Resources
    before = set(_spill_files())
    completed, failed, total, _, _, _, _ = _run_send(assoc, body, threshold)
    assert (completed, failed, total) == (0, 1, 1)
    assert set(_spill_files()) - before == set()
