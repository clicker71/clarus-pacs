# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""0609 field regression: study 8352 (MR, stored TS 4.70 JPEG-LS SV1).

340/340 C-STORE success in MicroDicom AND Weasis, yet neither viewer
displays the study; Weasis SC answers 0x0118 for all 340 instances.
Server-side bytes verified valid.

This test answers the wire question: does the bridge's CURRENT send path
corrupt a REAL 4.70 file (bytes/fragments/meta) when sent over
(a) the MAIN multi-context association (post-fix topology) and
(b) a dedicated single-TS association (pre-fix fallback topology)?

Requires the real field file: d:\\Clarus\\tmp\\8352_1.dcm (first instance
of study 8352, extracted on the Pi from the multipart fetch). Skipped when
the fixture is absent.
"""

import io
import socket
import time

import pytest
from clean_code.handlers.cmove import _ensure_file_meta_bytes
from pydicom import dcmread, dcmwrite
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    JPEGLosslessSV1,
)
from pynetdicom import AE, evt

FIXTURE = r"D:\Clarus\tmp\8352_1.dcm"

EVRLE = ExplicitVRLittleEndian
IVRLE = ImplicitVRLittleEndian
TS_470 = JPEGLosslessSV1
MR = "1.2.840.10008.5.1.4.1.1.4"


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class _Recorder:
    """Minimal C-STORE SCP recording the received dataset + file meta."""

    def __init__(self, supported_ts):
        self.ae = AE(ae_title="REC_SCP")
        self.ae.add_supported_context(MR, supported_ts)
        self.port = _free_port()
        self.events = []
        self.server = self.ae.start_server(
            ("127.0.0.1", self.port),
            block=False,
            evt_handlers=[(evt.EVT_C_STORE, self.on_store)],
        )
        time.sleep(0.3)

    def on_store(self, event):
        ds = event.dataset
        fm = event.file_meta
        ds.file_meta = fm
        buf = io.BytesIO()
        dcmwrite(buf, ds, enforce_file_format=True)
        buf.seek(0)
        self.events.append((buf.getvalue(), ds, fm))
        return 0x0000

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


def _assoc_main(recorder):
    """Post-fix topology: 3 single-TS contexts on ONE main association
    (mirrors MicroDicom contexts_req=3 acc=3)."""
    ae = AE(ae_title="CLARUS_BRIDGE")
    for ts in (EVRLE, IVRLE, TS_470):
        ae.add_requested_context(MR, ts)
    return ae.associate("127.0.0.1", recorder.port, ae_title="REC_SCP")


def _assoc_fallback(recorder):
    """Pre-fix topology: dedicated association, single TS 4.70."""
    ae = AE(ae_title="CLARUS_BRIDGE")
    ae.add_requested_context(MR, TS_470)
    return ae.associate("127.0.0.1", recorder.port, ae_title="REC_SCP")


def _check_received(raw, ds, fm, src):
    """Field-level contract between the received dataset and the source."""
    assert str(fm.MediaStorageSOPClassUID) == MR, fm
    assert str(fm.MediaStorageSOPInstanceUID) == str(src.SOPInstanceUID)
    assert str(fm.TransferSyntaxUID) == TS_470, fm
    assert str(ds.StudyInstanceUID) == str(src.StudyInstanceUID)
    assert str(ds.SeriesInstanceUID) == str(src.SeriesInstanceUID)
    assert str(ds.SOPClassUID) == MR
    assert str(ds.SOPInstanceUID) == str(src.SOPInstanceUID)
    assert getattr(ds, "Rows", None) == src.Rows
    assert getattr(ds, "Columns", None) == src.Columns
    assert str(ds.PhotometricInterpretation) == str(src.PhotometricInterpretation)
    # Encapsulated fragments must survive byte-for-byte
    assert bytes(ds.PixelData) == bytes(
        src.PixelData
    ), "PixelData fragments changed on the wire"
    # Lossless round-trip: decoded arrays must be identical
    import numpy as np

    back = dcmread(io.BytesIO(raw)).pixel_array
    assert np.array_equal(back, src.pixel_array), "decoded pixels differ"


@pytest.mark.skipif(
    not __import__("os").path.exists(FIXTURE),
    reason="field fixture tmp/8352_1.dcm not present",
)
def test_8352_real_4_70_survives_both_send_topologies():
    src = dcmread(FIXTURE)
    assert str(src.file_meta.TransferSyntaxUID) == TS_470
    assert str(src.SOPClassUID) == MR

    # EXACT bridge pre-processing: canonical meta trio + tmp file
    raw = open(FIXTURE, "rb").read()
    sop_uid = str(src.SOPInstanceUID)
    processed = _ensure_file_meta_bytes(raw, MR, sop_uid, TS_470)

    src_ds = dcmread(io.BytesIO(processed))

    received = {}
    for name, make_assoc in (
        ("main_multicontext", _assoc_main),
        ("fallback_single_ts", _assoc_fallback),
    ):
        rec = _Recorder([EVRLE, IVRLE, TS_470])
        try:
            assoc = make_assoc(rec)
            try:
                assert assoc.is_established, f"{name}: association failed"
                tmp_path = tempfile_named_tmp(processed)
                try:
                    status = assoc.send_c_store(tmp_path)
                finally:
                    __import__("os").unlink(tmp_path)
                assert (
                    status.Status == 0x0000
                ), f"{name}: C-STORE status {status.Status}"
            finally:
                assoc.release()
            assert len(rec.events) == 1, rec.events
            _check_received(*rec.events[0], src_ds)
            received[name] = rec.events[0][0]
        finally:
            rec.shutdown()

    # The two topologies must deliver IDENTICAL files to the receiver
    assert (
        received["main_multicontext"] == received["fallback_single_ts"]
    ), "main vs fallback delivery differ!"


def tempfile_named_tmp(data):
    import tempfile

    f = tempfile.NamedTemporaryFile(suffix=".dcm", delete=False)
    f.write(data)
    f.close()
    return f.name


if __name__ == "__main__":
    # Quick self-test of the harness pieces when run directly
    import sys

    sys.exit(pytest.main([__file__, "-xvs"]))
