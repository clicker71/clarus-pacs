#!/usr/bin/env python3
"""Local end-to-end C-MOVE check: real bridge code vs a local clarus.

Starts a local pynetdicom SCP, runs the actual _execute_cmove_job against
<clarus_url> for <study_uid>, prints the association outcome (multi-context
proposal), the job counters and the per-instance delivery TS.

Usage:
    local_cmove_check.py <clarus_url> <study_uid> [move_dest_ae]
Example:
    local_cmove_check.py http://127.0.0.1:8024/dicomweb 999.999.3859744
"""

import json
import struct
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

from pydicom.uid import ExplicitVRLittleEndian, ImplicitVRLittleEndian
from pynetdicom import AE, evt

sys.path.insert(0, __file__[: __file__.rfind("clbridge")] + "clbridge")
from clean_code.handlers.cmove import _execute_cmove_job  # noqa: E402

EVRLE = ExplicitVRLittleEndian
IVRLE = ImplicitVRLittleEndian
TS_470 = "1.2.840.10008.1.2.4.70"
TS_480 = "1.2.840.10008.1.2.4.80"

STUDY = "999.999.3859744"
SERIES = "999.999.94827453"
SOP_CT = "2.25.9990001112224444"

# Classes the local corpus actually uses (registry shape is
# {uid: {"accept": True}}; non-image classes get the prefix heuristic).
SOP_REGISTRY = {
    "1.2.840.10008.5.1.4.1.1.2": {"accept": True},     # CT
    "1.2.840.10008.5.1.4.1.1.6.1": {"accept": True},   # US
    "1.2.840.10008.5.1.4.1.1.7": {"accept": True},     # SC
}


def _ui(value):
    v = value.encode("ascii")
    return v + b"\x00" if len(v) % 2 else v


def _meta_trio(sc, si, ts):
    out = b""
    for elem, value in ((0x0002, sc), (0x0003, si), (0x0010, ts)):
        vv = _ui(value)
        out += struct.pack("<HH2sH", 0x0002, elem, b"UI", len(vv)) + vv
    return out


def _img_bytes(sop_uid, ts):
    d = b""
    for group, elem, value in (
        (0x0008, 0x0016, "1.2.840.10008.5.1.4.1.1.2"),
        (0x0008, 0x0018, sop_uid),
        (0x0020, 0x000E, SERIES),
    ):
        vv = _ui(value)
        d += struct.pack("<HH", group, elem) + b"UI" + struct.pack("<H", len(vv)) + vv
    d += struct.pack("<HH", 0x0028, 0x0010) + b"US" + struct.pack("<H", 2) + struct.pack("<H", 4)
    d += struct.pack("<HH", 0x0028, 0x0011) + b"US" + struct.pack("<H", 2) + struct.pack("<H", 4)
    d += struct.pack("<HH", 0x7FE0, 0x0010) + b"OW" + b"\x00\x00" + struct.pack("<I", 4) + b"\xaa" * 4
    return b"\x00" * 128 + b"DICM" + _meta_trio("1.2.840.10008.5.1.4.1.1.2", sop_uid, ts) + d


class _MockClarus(BaseHTTPRequestHandler):
    """Minimal DICOMweb server for the mock mode (CAPS + QIDO + metadata +
    study multipart WADO)."""

    part = _img_bytes(SOP_CT, EVRLE)
    part_ts = EVRLE
    caps = [EVRLE, IVRLE, TS_470, TS_480]

    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path == "/dicomweb":
            body = json.dumps({"transferCapabilities": [
                {"transferSyntaxUid": list(self.caps)}
            ]}).encode()
            self._send(200, "application/json", body)
            return
        if self.path == f"/dicomweb/studies/{STUDY}/series":
            body = json.dumps([
                {"0020000E": {"vr": "UI", "Value": [SERIES]}}
            ]).encode()
            self._send(200, "application/dicom+json", body)
            return
        if self.path == f"/dicomweb/studies/{STUDY}/series/{SERIES}/metadata":
            body = json.dumps([{
                "00080016": {"vr": "UI", "Value": ["1.2.840.10008.5.1.4.1.1.2"]},
                "00020010": {"vr": "UI", "Value": [EVRLE]},
            }]).encode()
            self._send(200, "application/dicom+json", body)
            return
        if self.path == f"/dicomweb/studies/{STUDY}":
            boundary = "mb42"
            part = self.part
            out = (f"--{boundary}\r\n").encode()
            out += (f"Content-Type: application/dicom; transfer-syntax={self.part_ts}\r\n"
                    f"Content-Length: {len(part)}\r\n\r\n").encode()
            out += part
            out += f"\r\n--{boundary}--\r\n".encode()
            self._send(200, f'multipart/related; type="application/dicom"; '
                            f'boundary={boundary}', out)
            return
        self.send_response(404)
        self.end_headers()

    def _send(self, code, ctype, body):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)


class _Jobs:
    def __init__(self):
        self.success = []
        self.failed = []

    def is_job_cancelled(self, job_id):
        return False

    def update_job_progress(self, *a):
        pass

    def finish_job(self, job_id, success=False, error_message=None):
        (self.success if success else self.failed).append((job_id, error_message))

    def record_scp_success(self, *a, **k):
        pass

    def record_scp_failure(self, *a, **k):
        pass

    def reload_peers_from_file(self, *a, **k):
        pass


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    clarus_url = sys.argv[1]
    study_uid = sys.argv[2]
    move_dest_ae = sys.argv[3] if len(sys.argv) > 3 else "CHECK_SCP"

    mock_srv = None
    if clarus_url == "mock":
        mock_srv = HTTPServer(("127.0.0.1", 0), _MockClarus)
        threading.Thread(target=mock_srv.serve_forever, daemon=True).start()
        clarus_url = f"http://127.0.0.1:{mock_srv.server_address[1]}/dicomweb"
        study_uid = STUDY

    # Local SCP: accepts every registry class with all four TS (multi-context
    # acceptor), records each C-STORE.
    ae = AE(ae_title=move_dest_ae)
    for sop_class in SOP_REGISTRY:
        ae.add_supported_context(sop_class, [EVRLE, IVRLE, TS_480, TS_470])
    received = []

    def on_store(event):
        ds = event.dataset
        ds.file_meta = event.file_meta
        received.append((str(ds.SOPInstanceUID),
                         str(ds.file_meta.TransferSyntaxUID)))
        return 0x0000

    server = ae.start_server(("127.0.0.1", 0), block=False,
                             evt_handlers=[(evt.EVT_C_STORE, on_store)])
    port = server.server_address[1]
    print(f"SCP {move_dest_ae} on 127.0.0.1:{port}; "
          f"clarus={clarus_url}; study={study_uid}")

    jobs = _Jobs()
    t0 = time.time()
    try:
        _execute_cmove_job(
            job_id=1,
            study_uid=study_uid,
            move_dest_ae=move_dest_ae,
            move_jobs=jobs,
            clarus_url=clarus_url,
            http_timeout=10,
            bridge_ae_title="CHECK_BRIDGE",
            target_host="127.0.0.1",
            target_port=port,
            sop_registry=SOP_REGISTRY,
            storage_commit_after_move=False,
            outbound_assoc_attempts=3,
        )
    except Exception as exc:
        print(f"JOB FAILED: {type(exc).__name__}: {exc}")
    finally:
        ae.shutdown()
        if mock_srv is not None:
            mock_srv.shutdown()
            mock_srv.server_close()

    dt = time.time() - t0
    print(f"elapsed={dt:.2f}s received={len(received)} "
          f"success={len(jobs.success)} failed={len(jobs.failed)}")
    for sop, ts in received:
        print(f"  C-STORE {sop} TS={ts}")
    if jobs.failed:
        for job_id, err in jobs.failed:
            print(f"  job {job_id} failed: {err}")


if __name__ == "__main__":
    main()
