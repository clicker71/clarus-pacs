# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""W3 (0209) spill-to-file - real-HTTP wire tests.

1. A 120 MB part streamed in 64 KB chunks over a real socket: the parser
   accepts it (pre-W3 the 100 MB tripwire aborted it), yields a SpilledPart
   whose in-memory head is capped at the injected threshold and whose excess
   is on disk - byte-exact.
2. Full C-MOVE: the bridge forwards a spilled part to a real SCP and leaves
   no temp files behind.

The spill decision is Content-Length > spill_threshold, so test 2 exercises
the same branch as a 120 MB field part with a smaller body and an injected
threshold - the suite stays fast without moving 120 MB twice.
"""

import glob
import json
import logging
import os
import socket
import struct
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

import requests
from clean_code.handlers.cmove import _execute_cmove_job
from clean_code.streaming import SpilledPart, _stream_multipart_instances
from pydicom.uid import ExplicitVRLittleEndian
from pynetdicom import AE, evt
from pynetdicom.sop_class import CTImageStorage

EVRLE = ExplicitVRLittleEndian
CT = "1.2.840.10008.5.1.4.1.1.2"
STUDY = "2.25.1111222233334444"
SOP = "2.25.9990001112223331"

BOUNDARY = "spillwire77"
PART_SIZE = 120 * 1024 * 1024  # the field-sized part (110 MB XA class)
THRESHOLD = 1024 * 1024  # injected spill threshold

_PIX = bytes(range(1, 252))
_HEAD = b"\x00" * 128 + b"DICM"
_BLOCK = bytes((i * 31 + 7) % 251 + 1 for i in range(251))


def _pattern(offset, n):
    """Deterministic `n` bytes of the streamed part body at `offset`.

    Bytes 0..131 are the preamble + DICM magic; the rest is a repeating
    251-byte block computed from the absolute position, so chunks generated
    independently stay continuous.
    """
    out = bytearray(n)
    if offset < len(_HEAD):
        m = min(len(_HEAD) - offset, n)
        out[:m] = _HEAD[offset : offset + m]
        offset += m
        start = m
    else:
        start = 0
    rest = n - start
    if rest > 0:
        shift = offset % 251
        block = _BLOCK[shift:] + _BLOCK[:shift]
        out[start:] = (block * (rest // 251 + 1))[:rest]
    return bytes(out)


class _StreamHandler(BaseHTTPRequestHandler):
    """Streams one multipart part of PART_SIZE bytes in 64 KB chunks."""

    def log_message(self, *a):
        pass

    def do_GET(self):
        head_hdr = (
            f"--{BOUNDARY}\r\n"
            f"Content-Type: application/dicom; transfer-syntax={EVRLE}\r\n"
            f"Content-Length: {PART_SIZE}\r\n\r\n"
        ).encode("ascii")
        tail = f"\r\n--{BOUNDARY}--\r\n".encode("ascii")
        self.send_response(200)
        self.send_header(
            "Content-Type",
            f'multipart/related; type="application/dicom"; boundary={BOUNDARY}',
        )
        self.send_header("Content-Length", str(len(head_hdr) + PART_SIZE + len(tail)))
        self.end_headers()
        self.wfile.write(head_hdr)
        offset = 0
        while offset < PART_SIZE:
            n = min(65536, PART_SIZE - offset)
            self.wfile.write(_pattern(offset, n))
            offset += n
        self.wfile.write(tail)


def _spill_files():
    return glob.glob(os.path.join(tempfile.gettempdir(), "clbridge_spill_*"))


def test_120mb_part_streamed_in_chunks_spills_to_disk_not_memory():
    """Field scenario: one 120 MB part over a real socket, streamed in 64 KB
    chunks. The parser must accept it (pre-W3 the tripwire aborted at 100 MB)
    and keep only the injected threshold in memory."""
    httpd = HTTPServer(("127.0.0.1", 0), _StreamHandler)
    port = httpd.server_address[1]
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    before = set(_spill_files())
    try:
        url = f"http://127.0.0.1:{port}/dicomweb/studies/{STUDY}"
        resp = requests.get(
            url,
            headers={"Accept": "multipart/related; type=application/dicom"},
            stream=True,
            timeout=60,
        )
        resp.raise_for_status()
        results = list(
            _stream_multipart_instances(
                resp,
                spill_threshold=THRESHOLD,
            )
        )
        assert len(results) == 1
        part, idx, ts = results[0]
        assert idx == 0
        assert ts == EVRLE
        assert isinstance(part, SpilledPart)
        # In-memory head is capped exactly at the threshold - the rest of
        # the 120 MB part never enters RAM in the parser.
        assert len(part.head) == THRESHOLD
        assert part.head == _pattern(0, THRESHOLD)
        try:
            assert os.path.getsize(part.path) == PART_SIZE - THRESHOLD
            with open(part.path, "rb") as f:
                offset = THRESHOLD
                while True:
                    chunk = f.read(65536)
                    if not chunk:
                        break
                    assert chunk == _pattern(offset, len(chunk))
                    offset += len(chunk)
        finally:
            if os.path.exists(part.path):
                os.unlink(part.path)
        assert set(_spill_files()) - before == set()
    finally:
        httpd.shutdown()
        httpd.server_close()


# ---------------------------------------------------------------------------
# Full C-MOVE: forward a spilled part to a real SCP, nothing left behind
# ---------------------------------------------------------------------------

# Preamble(132) + trio(92) + dataset elems(34+30+10+10) + pixel header(12)
IMG_HEADER = 320


def _ui(value):
    v = value.encode("ascii")
    return v + b"\x00" if len(v) % 2 else v


def _img_big(sop_uid, ts, total):
    """Valid CT instance of exactly `total` bytes (canonical trio meta)."""
    out = b""
    for elem, value in ((0x0002, CT), (0x0003, sop_uid), (0x0010, ts)):
        vv = _ui(value)
        out += struct.pack("<HH2sH", 0x0002, elem, b"UI", len(vv)) + vv
    for group, elem, value in ((0x0008, 0x0016, CT), (0x0008, 0x0018, sop_uid)):
        vv = _ui(value)
        out += struct.pack("<HH", group, elem) + b"UI" + struct.pack("<H", len(vv)) + vv
    out += (
        struct.pack("<HH", 0x0028, 0x0010)
        + b"US"
        + struct.pack("<H", 2)
        + struct.pack("<H", 4)
    )
    out += (
        struct.pack("<HH", 0x0028, 0x0011)
        + b"US"
        + struct.pack("<H", 2)
        + struct.pack("<H", 4)
    )
    pixels = total - IMG_HEADER
    assert pixels >= 0 and pixels % 2 == 0, (pixels, IMG_HEADER)
    out += (
        struct.pack("<HH", 0x7FE0, 0x0010)
        + b"OW"
        + b"\x00\x00"
        + struct.pack("<I", pixels)
    )
    out += (_PIX * (pixels // 251 + 1))[:pixels]
    return b"\x00" * 128 + b"DICM" + out


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class _ClarusHandler(BaseHTTPRequestHandler):
    parts = {}
    part_ts = {}
    requests = []

    def log_message(self, *a):
        pass

    def do_GET(self):
        type(self).requests.append(f"GET {self.path}")
        if self.path == "/dicomweb":
            body = json.dumps(
                {"transferCapabilities": [{"transferSyntaxUid": [EVRLE]}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        parts = self.parts.get(STUDY, [])
        tss = self.part_ts.get(STUDY, [])
        body = b""
        for i, part in enumerate(parts):
            body += (
                f"--{BOUNDARY}\r\n" if i == 0 else f"\r\n--{BOUNDARY}\r\n"
            ).encode()
            body += (
                f"Content-Type: application/dicom; transfer-syntax={tss[i]}\r\n"
                f"Content-Length: {len(part)}\r\n\r\n"
            ).encode()
            body += part
        body += f"\r\n--{BOUNDARY}--\r\n".encode()
        self.send_response(200)
        self.send_header(
            "Content-Type",
            f'multipart/related; type="application/dicom"; boundary={BOUNDARY}',
        )
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class _ClarusServer:
    def __init__(self):
        self.port = _free_port()
        self.httpd = HTTPServer(("127.0.0.1", self.port), _ClarusHandler)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    @property
    def clarus_url(self):
        return f"http://127.0.0.1:{self.port}/dicomweb"

    def shutdown(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class _Scp:
    def __init__(self):
        self.ae = AE(ae_title="SPILL_SCP")
        self.ae.add_supported_context(CTImageStorage, [EVRLE])
        self.port = _free_port()
        self.received = []
        self.server = self.ae.start_server(
            ("127.0.0.1", self.port),
            block=False,
            evt_handlers=[(evt.EVT_C_STORE, self.on_store)],
        )

    def on_store(self, event):
        ds = event.dataset
        ds.file_meta = event.file_meta
        self.received.append(
            (
                str(ds.SOPClassUID),
                str(ds.SOPInstanceUID),
                str(ds.file_meta.TransferSyntaxUID),
                len(ds.PixelData),
            )
        )
        return 0x0000

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


class _FakeMoveJobs:
    def is_job_cancelled(self, job_id):
        return False

    def update_job_progress(self, job_id, completed, failed, total):
        pass

    def finish_job(self, job_id, success=False, error_message=None):
        pass

    def record_scp_success(self, ae, host=""):
        pass

    def record_scp_failure(self, ae, host=""):
        pass

    def reload_peers_from_file(self, peers_file):
        pass


class _LogCapture(logging.Handler):
    def __init__(self):
        super().__init__(level=logging.INFO)
        self.records = []

    def emit(self, record):
        self.records.append(record.getMessage())


SOP_REGISTRY = {CT: {"accept": True}}


def _run_move(scp, server, part, spill_threshold):
    _ClarusHandler.requests = []
    _ClarusHandler.parts = {STUDY: [part]}
    _ClarusHandler.part_ts = {STUDY: [EVRLE]}

    handler = _LogCapture()
    root = logging.getLogger()
    root.addHandler(handler)
    old_level = root.level
    root.setLevel(logging.INFO)
    try:
        _execute_cmove_job(
            job_id=1,
            study_uid=STUDY,
            move_dest_ae="SPILL_SCP",
            move_jobs=_FakeMoveJobs(),
            clarus_url=server.clarus_url,
            http_timeout=10,
            bridge_ae_title="CLARUS_BRIDGE",
            target_host="127.0.0.1",
            target_port=scp.port,
            sop_registry=SOP_REGISTRY,
            storage_commit_after_move=False,
            spill_threshold=spill_threshold,
        )
    finally:
        root.removeHandler(handler)
        root.setLevel(old_level)
    return handler.records


def test_full_move_forwards_spilled_part_and_leaves_no_files():
    """The bridge accepts a spilled part, reassembles it on disk and the
    real SCP receives the FULL instance (head + spilled excess). Both temp
    files are gone afterwards."""
    scp = _Scp()
    server = _ClarusServer()
    time.sleep(0.3)
    before = set(_spill_files())
    try:
        part = _img_big(SOP, EVRLE, 4 * 1024 * 1024)
        records = _run_move(scp, server, part, spill_threshold=1024 * 1024)
        assert len(scp.received) == 1, records
        assert scp.received[0][0] == CT
        assert scp.received[0][1] == SOP
        assert scp.received[0][2] == EVRLE
        # Every pixel byte arrived: head + spill were reassembled exactly.
        assert scp.received[0][3] == 4 * 1024 * 1024 - IMG_HEADER
        counters = [r for r in records if "passthrough=" in r]
        assert counters and "passthrough=1" in counters[0], records
        assert set(_spill_files()) - before == set()
    finally:
        scp.shutdown()
        server.shutdown()
