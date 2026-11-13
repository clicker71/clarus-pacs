# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""3008 stored-first passthrough - bridge Rule A wire tests (fake SCP+fake HTTP).

0909 Rule A (no probe/second association; analysis/0909_architect_microdicom_
probe_refetch_resume_REPORT.md sec. 2.2): delivery is decided on the ONE main
association.
1. SCP accepts the stored TS: ONE study-level GET WITHOUT ?transferSyntax=,
   C-STORE bytes carry the stored TS, counter passthrough=1.
2. SCP accepts only EVRLE, stored TS not proposed/accepted: refetch that ONE
   instance on the MAIN association ?transferSyntax=EVRLE (no second assoc),
   counter refetched=1 transcoded=1.
3. Stored rejected AND server cannot produce an accepted TS: honest per-
   instance failure with reason (R3), no refetch, one association only.
4. Passthrough mode (server reports no transcode): single study-level fetch.
5. Non-image SOP (SR): single fetch, C-STORE as-is.
6. Stored 4.70 accepted on the main 4.70 context (field scenario): passthrough
   refetched=0 (no second association).
7. S2: SC not negotiated -> commit push skipped, no separate SC association.
Rule A replaces the 31.08 fallback-first scenarios (6-11 of the old file) and
the probes_timeout observability (removed with the probe itself).
"""

import io
import json
import logging
import socket
import struct
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

from clean_code.handlers.cmove import _execute_cmove_job
from pydicom import dcmread
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    JPEGBaseline8Bit,
    JPEGLosslessSV1,
    JPEGLSLossless,
)
from pynetdicom import AE, evt

EVRLE = ExplicitVRLittleEndian
IVRLE = ImplicitVRLittleEndian
JPG = JPEGBaseline8Bit

# Transfer syntaxes whose Pixel Data is encapsulated (PS3.5 A.4). EVRLE/IVRLE
# carry native Pixel Data with an explicit length, so the fixture must encode
# the two forms differently - that distinction is the whole point of the
# passthrough assertions.
_COMPRESSED_TS = frozenset({JPEGBaseline8Bit, JPEGLosslessSV1, JPEGLSLossless})

CT = "1.2.840.10008.5.1.4.1.1.2"
MR = "1.2.840.10008.5.1.4.1.1.4"
SR = "1.2.840.10008.5.1.4.1.1.88.11"  # Basic Text SR

STUDY = "2.25.1111222233334444"
SERIES = "2.25.5555666677778888"
SOP_CT = "2.25.9990001112223331"
SOP_CT2 = "2.25.9990001112223333"
SOP_SR = "2.25.9990001112223332"


def _ui(value):
    v = value.encode("ascii")
    return v + b"\x00" if len(v) % 2 else v


# A deterministic "compressed" bit stream: a SOI marker, filler that is NOT
# plain grey, and the EOI marker. Nothing in the bridge decodes it, so its
# only job is to be recognisable byte-for-byte after the round trip - which is
# what proves the stored payload survived instead of being re-encoded.
_COMPRESSED_FRAME = b"\xff\xd8" + bytes(range(0x20, 0x20 + 62)) + b"\xff\xd9"


def _fragment(sop_uid, ts):
    """`_COMPRESSED_FRAME` with the SOP UID folded in, so two instances of the
    same study cannot be confused."""
    tail = sop_uid.encode("ascii")
    body = _COMPRESSED_FRAME + tail
    return body + b"\x00" if len(body) % 2 else body


def _encapsulated_pixel_data(fragment):
    """PS3.5 A.4 encapsulated (7FE0,0010) OB, undefined length.

    Layout: an empty Basic Offset Table Item (FFFE,E000 with length 0), one
    Fragment Item (FFFE,E000) carrying the encoded bit stream, then the
    Sequence Delimitation Item (FFFE,E0DD) with length 0. The Value Length of
    the element itself is the Undefined Length sentinel.
    """
    bot = struct.pack("<HHI", 0xFFFE, 0xE000, 0)  # empty basic offset table
    item = struct.pack("<HHI", 0xFFFE, 0xE000, len(fragment)) + fragment
    delim = struct.pack("<HHI", 0xFFFE, 0xE0DD, 0)
    return (
        struct.pack("<HH", 0x7FE0, 0x0010)
        + b"OB"
        + b"\x00\x00"
        + struct.pack("<I", 0xFFFFFFFF)
        + bot
        + item
        + delim
    )


def _native_pixel_data(payload):
    """PS3.5 A.4 native (7FE0,0010) OW, defined length."""
    return (
        struct.pack("<HH", 0x7FE0, 0x0010)
        + b"OW"
        + b"\x00\x00"
        + struct.pack("<I", len(payload))
        + payload
    )


def _meta_trio_bytes(sc, si, ts):
    out = b""
    for elem, value in ((0x0002, sc), (0x0003, si), (0x0010, ts)):
        vv = _ui(value)
        out += struct.pack("<HH2sH", 0x0002, elem, b"UI", len(vv)) + vv
    return out


def _img_bytes(sop_uid, ts):
    """CT instance with canonical trio meta, stored under `ts`.

    A compressed transfer syntax REQUIRES encapsulated Pixel Data (PS3.5 A.4:
    VR OB, Undefined Length, fragment Items). A defined-length OW blob under a
    compressed TS is not encapsulated at all, so a test built on one could
    never show that the compressed bytes survived delivery.
    """
    d = b""
    for group, elem, value in (
        (0x0008, 0x0016, CT),
        (0x0008, 0x0018, sop_uid),
        (0x0020, 0x000E, SERIES),
    ):
        vv = _ui(value)
        d += struct.pack("<HH", group, elem) + b"UI" + struct.pack("<H", len(vv)) + vv
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
    if ts in _COMPRESSED_TS:
        d += _encapsulated_pixel_data(_fragment(sop_uid, ts))
    else:
        d += _native_pixel_data(b"\xaa" * 4)
    return b"\x00" * 128 + b"DICM" + _meta_trio_bytes(CT, sop_uid, ts) + d


def _sr_bytes(sop_uid):
    d = b""
    for group, elem, value in (
        (0x0008, 0x0016, SR),
        (0x0008, 0x0018, sop_uid),
        (0x0020, 0x000E, SERIES),
    ):
        vv = _ui(value)
        d += struct.pack("<HH", group, elem) + b"UI" + struct.pack("<H", len(vv)) + vv
    return b"\x00" * 128 + b"DICM" + _meta_trio_bytes(SR, sop_uid, EVRLE) + d


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _pixel_data_fragment(ds):
    """The first encapsulated Fragment Item payload of (7FE0,0010).

    pydicom hands an encapsulated element back as the raw Item stream (Basic
    Offset Table Item, the Fragment Items, then the Sequence Delimitation
    Item). Returning the first fragment's payload lets a test compare the
    bytes that arrived against the bytes that were sent - the only way to show
    the stored compressed stream survived instead of being re-encoded.

    A native (defined-length) element yields its value bytes unchanged.
    """
    try:
        element = ds[0x7FE0, 0x0010]
    except KeyError:
        return b""
    value = element.value
    stream = bytes((value[1] if isinstance(value, tuple) else value) or b"")
    if len(stream) < 8 or stream[:4] != b"\xfe\xff\x00\xe0":
        # Native format: no Item stream to walk.
        return stream
    # Walk the Item stream: the first Item with a non-zero length is the
    # fragment (PS3.5 7.5: items carry no VR, 8-byte header).
    pos = 0
    while pos + 8 <= len(stream):
        _group, _elem, length = struct.unpack_from("<HHI", stream, pos)
        if length:
            return stream[pos + 8 : pos + 8 + length]
        pos += 8
    return b""


class _FakeClarus(BaseHTTPRequestHandler):
    """Minimal DICOMweb server: capabilities, study multipart, instance GET."""

    parts = {}  # study_uid -> [part_bytes]
    part_ts = {}  # study_uid -> [ts per part]
    instance = {}  # (study, series, sop) -> transcoded EVRLE bytes
    requests = []  # recorded "METHOD path"
    caps_ts = [EVRLE]

    def log_message(self, *a):
        pass

    def do_GET(self):
        type(self).requests.append(f"GET {self.path}")
        base = f"/dicomweb/studies/{STUDY}"
        if self.path == "/dicomweb":
            body = json.dumps(
                {"transferCapabilities": [{"transferSyntaxUid": list(self.caps_ts)}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        # Instance-level GET MUST be matched before the study-level branch
        # (its path also starts with the study prefix).
        if "/instances/" in self.path:
            sop = self.path.rsplit("/", 1)[-1].split("?")[0]
            body = self.instance.get((STUDY, SERIES, sop), b"")
            self.send_response(200)
            self.send_header("Content-Type", "application/dicom")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path == base or self.path.startswith(base + "/"):
            boundary = "fakeboundary42"
            parts = self.parts.get(STUDY, [])
            tss = self.part_ts.get(STUDY, [])
            body = b""
            for i, part in enumerate(parts):
                body += (
                    f"--{boundary}\r\n" if i == 0 else f"\r\n--{boundary}\r\n"
                ).encode()
                body += (
                    f"Content-Type: application/dicom; transfer-syntax={tss[i]}\r\n"
                    f"Content-Length: {len(part)}\r\n\r\n"
                ).encode()
                body += part
            body += f"\r\n--{boundary}--\r\n".encode()
            self.send_response(200)
            self.send_header(
                "Content-Type",
                f'multipart/related; type="application/dicom"; boundary={boundary}',
            )
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404)
        self.end_headers()

    @classmethod
    def reset(cls):
        cls.parts = {}
        cls.part_ts = {}
        cls.instance = {}
        cls.requests = []
        cls.caps_ts = [EVRLE]


class _FakeServer:
    def __init__(self):
        self.port = _free_port()
        self.httpd = HTTPServer(("127.0.0.1", self.port), _FakeClarus)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    @property
    def clarus_url(self):
        return f"http://127.0.0.1:{self.port}/dicomweb"

    def shutdown(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class _StorageScp:
    def __init__(self, contexts):
        self.ae = AE(ae_title="TEST_SCP")
        for sop_class, ts_list in contexts:
            self.ae.add_supported_context(sop_class, ts_list)
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
                str(getattr(ds, "SOPClassUID", "")),
                str(getattr(ds, "SOPInstanceUID", "")),
                str(getattr(ds.file_meta, "TransferSyntaxUID", "")),
                _pixel_data_fragment(ds),
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


SOP_REGISTRY = {
    CT: {"accept": True},
    MR: {"accept": True},
    SR: {"accept": True, "non_image": True},
}


def _run_move(scp, server, study_parts, part_ts, caps, instance_map=None, commit=False):
    _FakeClarus.reset()
    _FakeClarus.caps_ts = list(caps)
    _FakeClarus.parts[STUDY] = list(study_parts)
    _FakeClarus.part_ts[STUDY] = list(part_ts)
    _FakeClarus.instance.update(instance_map or {})

    handler = _LogCapture()
    root = logging.getLogger()
    root.addHandler(handler)
    old_level = root.level
    root.setLevel(logging.INFO)
    try:
        _execute_cmove_job(
            job_id=1,
            study_uid=STUDY,
            move_dest_ae="TEST_SCP",
            move_jobs=_FakeMoveJobs(),
            clarus_url=server.clarus_url,
            http_timeout=10,
            bridge_ae_title="CLARUS_BRIDGE",
            target_host="127.0.0.1",
            target_port=scp.port,
            sop_registry=SOP_REGISTRY,
            storage_commit_after_move=commit,
        )
    finally:
        root.removeHandler(handler)
        root.setLevel(old_level)
    return handler.records


def _counters(records):
    for msg in records:
        if "passthrough=" in msg:
            return msg
    return ""


def _study_stream_gets():
    """The TRUE study-level multipart streaming GETs (the R1 pre-flight issues
    its own /series + /metadata HTTP GETs which must not count as study
    streams). Path == exactly the study base (no /series, /metadata,
    /instances suffix)."""
    base = f"GET /dicomweb/studies/{STUDY}"
    out = []
    for r in _FakeClarus.requests:
        if r.startswith(base):
            rest = r[len(base) :]
            if rest == "":
                out.append(r)
    return out


def test_stored_ts_accepted_by_main_association():
    """Stored EVRLE accepted on main: one study GET, as-is (multi-context:
    the EVRLE context is accepted; no fallback association)."""
    scp = _StorageScp([(CT, [EVRLE])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored = _img_bytes(SOP_CT, EVRLE)
        records = _run_move(scp, server, [stored], [EVRLE], caps=[EVRLE])
        study_gets = _study_stream_gets()
        assert len(study_gets) == 1, _FakeClarus.requests
        assert "transferSyntax" not in study_gets[0], study_gets
        inst_gets = [r for r in _FakeClarus.requests if "/instances/" in r]
        assert inst_gets == [], inst_gets
        assert len(scp.received) == 1
        assert scp.received[0][2] == EVRLE, "stored TS must be sent as-is"
        assert "passthrough=1" in _counters(records), records
    finally:
        scp.shutdown()
        server.shutdown()


def test_the_compressed_fixture_is_encapsulated_not_native():
    """PS3.5 A.4: a compressed transfer syntax REQUIRES encapsulated Pixel Data.

    The fixture is the evidence in every compressed-wire test above, so its
    framing is itself asserted here: VR OB, Undefined Length, an Item stream
    that starts with a Basic Offset Table Item, one Fragment Item carrying the
    encoded bit stream and a Sequence Delimitation Item. A defined-length OW
    blob under a compressed TS would mean those tests only proved that the
    transfer-syntax UID survived.
    """
    for ts in (JPG, JPEGLosslessSV1, JPEGLSLossless):
        raw = _img_bytes(SOP_CT, ts)
        # Locate the Pixel Data element with pydicom rather than a byte scan:
        # the tag bytes also occur inside the SOP UID text and inside the
        # fragment itself. `file_offset` is where its header starts.
        parsed = dcmread(io.BytesIO(raw))
        # file_tell is the offset of the element's VALUE (the Item stream);
        # the 12-byte long-VR header starts immediately before it.
        pos = parsed[(0x7FE0, 0x0010)].file_tell - 12
        assert pos > 0, ts
        vr = raw[pos + 4 : pos + 6]
        assert vr == b"OB", (ts, vr)
        length = struct.unpack_from("<I", raw, pos + 8)[0]
        assert length == 0xFFFFFFFF, (ts, hex(length))

        stream = raw[parsed[(0x7FE0, 0x0010)].file_tell :]
        # Item stream: Basic Offset Table Item (empty), Fragment Item, then the
        # Sequence Delimitation Item. Each Item header is 8 bytes (PS3.5 7.5:
        # no VR), so the fragment payload starts at 16 and the delimiter
        # follows it at 16 + frag_len.
        group, elem, bot_len = struct.unpack_from("<HHI", stream, 0)
        assert (group, elem, bot_len) == (0xFFFE, 0xE000, 0), (ts, group, elem, bot_len)
        group, elem, frag_len = struct.unpack_from("<HHI", stream, 8)
        assert (group, elem) == (0xFFFE, 0xE000), (ts, group, elem)
        assert frag_len > 0, ts
        fragment = stream[16 : 16 + frag_len]
        assert fragment == _fragment(
            SOP_CT, ts
        ), "the fragment payload must be the encoded bytes"
        delim_at = 16 + frag_len
        group, elem, delim_len = struct.unpack_from("<HHI", stream, delim_at)
        assert (group, elem, delim_len) == (0xFFFE, 0xE0DD, 0), (
            ts,
            group,
            elem,
            delim_len,
        )
        assert len(stream) == delim_at + 8, (ts, len(stream), delim_at + 8)

    # The native path keeps the explicit-length OW form (A.4).
    native = _img_bytes(SOP_CT, EVRLE)
    pos = native.find(struct.pack("<HH", 0x7FE0, 0x0010))
    assert native[pos + 4 : pos + 6] == b"OW"
    assert struct.unpack_from("<I", native, pos + 8)[0] == 4


def test_stored_rejected_refetch_single_instance_evrle():
    """Rule A (0909): SCP accepts only EVRLE, stored JPEG Baseline. The bridge
    opens ONLY the ONE main association, and because the server can produce an
    accepted TS (EVRLE in caps) it refetches the instance ON THE MAIN to EVRLE.
    NO second/probe association is ever opened (refetched=1, one main assoc)."""
    scp = _StorageScp([(CT, [EVRLE])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored = _img_bytes(SOP_CT, JPG)
        transcoded = _img_bytes(SOP_CT, EVRLE)
        records = _run_move(
            scp,
            server,
            [stored],
            [JPG],
            caps=[EVRLE],
            instance_map={(STUDY, SERIES, SOP_CT): transcoded},
        )
        inst_gets = [r for r in _FakeClarus.requests if "/instances/" in r]
        assert len(inst_gets) == 1, inst_gets
        assert inst_gets[0].endswith(
            f"/studies/{STUDY}/series/{SERIES}/instances/{SOP_CT}"
            "?transferSyntax=1.2.840.10008.1.2.1"
        ), inst_gets
        assert len(scp.received) == 1
        assert scp.received[0][2] == EVRLE
        joined = "\n".join(records)
        # Exactly ONE association (the main); NO second/probe association.
        assert joined.count("Requesting Association") == 1, joined
        assert "refetching on the MAIN association" in joined, joined
        assert "TS-fallback association" not in joined, joined
        msg = _counters(records)
        assert "refetched=1" in msg, msg
        assert "transcoded=1" in msg, msg
    finally:
        scp.shutdown()
        server.shutdown()


def test_main_ivrle_no_refetch_failed_with_reason():
    """Rule A: the only accepted/main TS is IVRLE and the stored TS (JPG) is
    rejected AND the server cannot produce the accepted TS (caps=[EVRLE] but
    the main accepted is IVRLE, so no IVRLE in production): NO refetch, honest
    per-instance failure with a logged reason (R3). NO second association."""
    scp = _StorageScp([(CT, [IVRLE])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored = _img_bytes(SOP_CT, JPG)
        records = _run_move(scp, server, [stored], [JPG], caps=[EVRLE])
        inst_gets = [r for r in _FakeClarus.requests if "/instances/" in r]
        assert inst_gets == [], "no refetch when the accepted TS is not producible"
        assert scp.received == []
        joined = "\n".join(records)
        assert "cannot produce an accepted TS" in joined, joined
        assert "TS-fallback association" not in joined, joined
    finally:
        scp.shutdown()
        server.shutdown()


def test_passthrough_mode_single_fetch_as_is():
    """No server transcode capabilities: single study-level fetch, as-is."""
    scp = _StorageScp([(CT, [EVRLE])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored = _img_bytes(SOP_CT, EVRLE)
        records = _run_move(scp, server, [stored], [EVRLE], caps=[])
        study_gets = _study_stream_gets()
        assert len(study_gets) == 1, _FakeClarus.requests
        assert len(scp.received) == 1
        assert scp.received[0][2] == EVRLE
        assert "passthrough=1" in _counters(records), records
    finally:
        scp.shutdown()
        server.shutdown()


def test_non_image_sr_single_fetch_as_is():
    """Non-image SOP: single fetch, C-STORE as-is on the main association."""
    scp = _StorageScp([(SR, [EVRLE, IVRLE])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored = _sr_bytes(SOP_SR)
        records = _run_move(scp, server, [stored], [EVRLE], caps=[EVRLE])
        inst_gets = [r for r in _FakeClarus.requests if "/instances/" in r]
        assert inst_gets == []
        assert len(scp.received) == 1
        assert scp.received[0][0] == SR
        assert scp.received[0][2] == EVRLE
        assert "passthrough=1" in _counters(records), records
    finally:
        scp.shutdown()
        server.shutdown()


def test_native_only_scp_two_instances_refetch_on_main():
    """Rule A: native-only SCP + a TWO-instance study whose stored TS (4.80)
    is not accepted by the SCP. The bridge opens ONLY the ONE main
    association and refetches BOTH instances on it to an accepted & producible
    TS (EVRLE). No probe, no second association, no rejection cache.
    refetched=2 transcoded=2."""
    scp = _StorageScp([(CT, [EVRLE])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored1 = _img_bytes(SOP_CT, JPEGLSLossless)
        stored2 = _img_bytes(SOP_CT2, JPEGLSLossless)
        evrle1 = _img_bytes(SOP_CT, EVRLE)
        evrle2 = _img_bytes(SOP_CT2, EVRLE)
        records = _run_move(
            scp,
            server,
            [stored1, stored2],
            [JPEGLSLossless, JPEGLSLossless],
            caps=[EVRLE],
            instance_map={
                (STUDY, SERIES, SOP_CT): evrle1,
                (STUDY, SERIES, SOP_CT2): evrle2,
            },
        )
        inst_gets = [r for r in _FakeClarus.requests if "/instances/" in r]
        assert len(inst_gets) == 2, inst_gets
        assert all(
            g.endswith("?transferSyntax=1.2.840.10008.1.2.1") for g in inst_gets
        ), inst_gets
        joined = "\n".join(records)
        # Exactly ONE association (the main) for the whole job - no probe.
        assert joined.count("Requesting Association") == 1, joined
        assert "refetching on the MAIN association" in joined, joined
        assert "TS-fallback association" not in joined, joined
        assert len(scp.received) == 2, scp.received
        assert {r[1] for r in scp.received} == {SOP_CT, SOP_CT2}, scp.received
        assert all(r[2] == EVRLE for r in scp.received), scp.received
        msg = _counters(records)
        assert "refetched=2" in msg, msg
        assert "transcoded=2" in msg, msg
    finally:
        scp.shutdown()
        server.shutdown()


def test_native_only_scp_accepts_fallback_passthrough_470():
    """0609 multi-context accept case (field scenario 4.70): the SCP that
    supports JPEG-LS 4.70 now accepts the SEPARATE 4.70 presentation context
    on the main association - stored 4.70 rides the MAIN association as-is,
    NO separate fallback association is opened (no-transcode server too).
    The pre-flight-less default proposal [EVRLE, IVRLE, 4.80, 4.70] makes the
    stored TS an explicit main context, so the old dedicated-fallback path is
    unnecessary. Job #16 passthrough semantics preserved: refetched=0."""
    scp = _StorageScp([(CT, [EVRLE, JPEGLosslessSV1])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored = _img_bytes(SOP_CT, JPEGLosslessSV1)
        records = _run_move(scp, server, [stored], [JPEGLosslessSV1], caps=[])
        inst_gets = [r for r in _FakeClarus.requests if "/instances/" in r]
        assert inst_gets == [], inst_gets
        assert len(scp.received) == 1
        assert scp.received[0][2] == JPEGLosslessSV1
        # PS3.5 A.4: the encapsulated fragment on the wire is byte-identical
        # to the one that was stored - the compressed stream was not decoded
        # and re-encoded on the way through.
        assert scp.received[0][3] == _fragment(
            SOP_CT, JPEGLosslessSV1
        ), "the stored compressed fragment must arrive unchanged"
        joined = "\n".join(records)
        # Accepted on the main (a 4.70 context was proposed and accepted) -
        # passthrough on the ONE association.
        assert "refetching on the MAIN association" not in joined, joined
        assert joined.count("Requesting Association") == 1, joined
        msg = _counters(records)
        assert "passthrough=1" in msg, msg
        assert "refetched=0" in msg, msg
    finally:
        scp.shutdown()
        server.shutdown()


def test_main_covers_stored_ts_no_fallback_association():
    """0109 Weasis regression: the main context ALREADY covers the stored
    TS (acceptor picked 4.80 for CT) - the instance rides the MAIN
    association as-is on the ONE association. No refetch, no second assoc."""
    scp = _StorageScp([(CT, [JPEGLSLossless, EVRLE])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored = _img_bytes(SOP_CT, JPEGLSLossless)
        records = _run_move(scp, server, [stored], [JPEGLSLossless], caps=[EVRLE])
        inst_gets = [r for r in _FakeClarus.requests if "/instances/" in r]
        assert inst_gets == [], inst_gets
        assert len(scp.received) == 1
        assert scp.received[0][2] == JPEGLSLossless
        assert scp.received[0][3] == _fragment(
            SOP_CT, JPEGLSLossless
        ), "4.80 rides the main context; its fragment must be untouched"
        joined = "\n".join(records)
        # Exactly ONE association (main); no refetch.
        assert joined.count("Requesting Association") == 1, joined
        assert "refetching on the MAIN association" not in joined, joined
        msg = _counters(records)
        assert "passthrough=1" in msg, msg
    finally:
        scp.shutdown()
        server.shutdown()


def test_sc_not_negotiated_skips_commit_push():
    """S2 (31.08): the viewer's main association negotiated no Storage
    Commitment SOP class -> the separate SC-only association is NOT opened
    (MicroDicom burned ACSE timeouts there, field Jobs #8/#9)."""
    scp = _StorageScp([(CT, [EVRLE])])
    server = _FakeServer()
    time.sleep(0.3)
    try:
        stored = _img_bytes(SOP_CT, EVRLE)
        records = _run_move(scp, server, [stored], [EVRLE], caps=[EVRLE], commit=True)
        joined = "\n".join(records)
        assert (
            "not negotiated in the main association - skipping SC push" in joined
        ), joined
        assert "[C-MOVE-COMMIT] Sending Storage Commitment" not in joined, joined
        assert "passthrough=1" in _counters(records), records
    finally:
        scp.shutdown()
        server.shutdown()
