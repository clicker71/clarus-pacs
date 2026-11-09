# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""3008 canonical transcode file-meta writer - wire regression test.

Real pynetdicom C-STORE over the loopback: the bytes produced by
_ensure_file_meta_bytes must be ACCEPTED by a Storage SCP. This is the
exact 30.08 incident - pynetdicom refused cltranscode-stored files whose
meta group was missing MediaStorageSOPClassUID / MediaStorageSOPInstanceUID
and the bridge C-MOVE died with 0xB000.
"""

import socket
import struct
import tempfile
import threading
import time

from clean_code.handlers.cmove import _ensure_file_meta_bytes
from pydicom import dcmread
from pynetdicom import AE, evt
from pynetdicom.sop_class import CTImageStorage

SC = "1.2.840.10008.5.1.4.1.1.2"  # CT, 25 chars - odd
SI = "1.2.3.4.5.6.7.8.9.10"
TS = "1.2.840.10008.1.2.1"  # Explicit VR Little Endian


def _ui(value):
    v = value.encode("ascii")
    return v + b"\x00" if len(v) % 2 else v


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _anon_style_bytes():
    """Anon-fixture shape: meta = TS-only (NULL inside), explicit dataset."""
    v = _ui(TS)
    meta = struct.pack("<HH2sH", 0x0002, 0x0010, b"UI", len(v)) + v
    d = b""
    for elem, value in ((0x0016, SC), (0x0018, SI)):
        vv = _ui(value)
        d += struct.pack("<HH", 0x0008, elem) + b"UI" + struct.pack("<H", len(vv)) + vv
    d += (
        struct.pack("<HH", 0x0028, 0x0010)
        + b"US"
        + struct.pack("<H", 2)
        + struct.pack("<H", 4)
    )
    d += (
        struct.pack("<HH", 0x0028, 0x0011)
        + b"US"
        + struct.pack("<H", 2)
        + struct.pack("<H", 4)
    )
    d += (
        struct.pack("<HH", 0x0028, 0x0004)
        + b"CS"
        + struct.pack("<H", 12)
        + b"MONOCHROME2\x00"
    )
    d += (
        struct.pack("<HH", 0x7FE0, 0x0010)
        + b"OW"
        + b"\x00\x00"
        + struct.pack("<I", 4)
        + b"\xaa" * 4
    )
    return b"\x00" * 128 + b"DICM" + meta + d


class _StorageScp:
    def __init__(self):
        self.ae = AE(ae_title="TEST_SCP")
        self.ae.add_supported_context(CTImageStorage, [TS])
        self.port = _free_port()
        self.received = {}
        self.ready = threading.Event()
        self.server = self.ae.start_server(
            ("127.0.0.1", self.port),
            block=False,
            evt_handlers=[(evt.EVT_C_STORE, self.on_store)],
        )

    def on_store(self, event):
        try:
            ds = event.dataset
            ds.file_meta = event.file_meta
            self.received["sop_class"] = str(ds.SOPClassUID)
            self.received["sop_instance"] = str(ds.SOPInstanceUID)
            self.received["ts"] = str(ds.file_meta.TransferSyntaxUID)
            self.received["bytes"] = event.file_meta  # parsed = accepted
        finally:
            self.ready.set()
        return 0x0000

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


def test_cstore_accepts_canonical_meta_trio():
    scp = _StorageScp()
    time.sleep(0.3)  # acceptor binds before we associate
    try:
        body = _ensure_file_meta_bytes(_anon_style_bytes(), "", "", TS)
        with tempfile.NamedTemporaryFile(suffix=".dcm", delete=False) as f:
            f.write(body)
            tmp_path = f.name

        ae = AE(ae_title="TEST_SCU")
        ae.add_requested_context(CTImageStorage, TS)
        assoc = ae.associate("127.0.0.1", scp.port)
        assert assoc.is_established
        status = assoc.send_c_store(tmp_path)
        assert status is not None, "C-STORE must return a status"
        assert status.Status == 0x0000, "SCP must accept the file"

        assert scp.ready.wait(timeout=5)
        assert scp.received["sop_class"] == SC
        assert scp.received["sop_instance"] == SI
        assert scp.received["ts"] == TS

        # And the same bytes parse cleanly with pydicom.
        ds = dcmread(tmp_path, stop_before_pixels=True)
        assert str(ds.SOPClassUID) == SC
        assert str(ds.SOPInstanceUID) == SI
        assert str(ds.file_meta.TransferSyntaxUID) == TS
    finally:
        scp.shutdown()
