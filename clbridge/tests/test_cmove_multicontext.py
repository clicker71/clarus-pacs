# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0609 C-MOVE multi-context negotiation - writer tests.

Covers the Stage-1 bridge-only changes (spec
prompts/0609_writer_cmove_multicontext.md, analysis REPORT section 12):
- canonical TS order + the 128-context budget trim (PS3.8 7.1.1.13;
  PS3.8 9.3.2.2 per-context acceptance; PS3.7 C.2.2.1.1);
- R1 pre-flight from the WADO metadata resource (PS3.18 10.4.4), failure
  degrades to None -> default proposal (never fails the job);
- R4 spec-12.1 delivery: stored TS proposed + rejected at negotiation while
  a deliverable negotiated TS exists -> refetch ON THE MAIN association (no
  fallback A-ASSOCIATE).
EVRLE-first / set-valued map wire behaviour is covered in
test_bridge_ts_order.py (set membership) and test_cmove.py (set-valued map);
this file adds the pre-flight and negotiation-result delivery specifics.
"""

import json
import socket
import struct
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

from clean_code.handlers.cmove import (
    _associate_for_move,
    _preflight_study_ts_by_class,
    _trim_optional_ts,
)
from clean_code.protocol import canonical_ts_sort_key
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    JPEGLosslessSV1,
)
from pynetdicom import AE, evt

EVRLE = ExplicitVRLittleEndian
IVRLE = ImplicitVRLittleEndian
TS_470 = JPEGLosslessSV1  # "1.2.840.10008.1.2.4.70"
TS_480 = "1.2.840.10008.1.2.4.80"
RLE = "1.2.840.10008.1.2.5"

CT = "1.2.840.10008.5.1.4.1.1.2"

STUDY = "2.25.1111222233339999"
SERIES = "2.25.5555666677778888"
SOP_CT = "2.25.9990001112224444"
SOP_CT2 = "2.25.9990001112225555"

SOP_REGISTRY = {CT: {"accept": True}}


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
    d += (
        struct.pack("<HH", 0x7FE0, 0x0010)
        + b"OW"
        + b"\x00\x00"
        + struct.pack("<I", 4)
        + b"\xaa" * 4
    )
    return b"\x00" * 128 + b"DICM" + _meta_trio(CT, sop_uid, ts) + d


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


# ---------------------------------------------------------------------------
# canonical TS sort (deterministic proposer order)
# ---------------------------------------------------------------------------
def test_canonical_sort_uncompressed_first():
    ordered = sorted([TS_480, TS_470, RLE, EVRLE, IVRLE], key=canonical_ts_sort_key)
    assert ordered[0] == EVRLE
    assert ordered[1] == IVRLE
    assert RLE in ordered[:3] or ordered[2] == RLE
    # JPEG-LS (4.80) sorts before JPEG lossless SV1 (4.70) only by our table - the
    # useful invariant is that unknown UIDs sort to the END (never dropped early).
    unknown = "1.2.840.10008.1.2.4.999"
    assert canonical_ts_sort_key(unknown) >= canonical_ts_sort_key(TS_470)


# ---------------------------------------------------------------------------
# 128-context budget trim (defensive; R2)
# ---------------------------------------------------------------------------
def test_budget_trim_keeps_baseline_drops_stored_then_fixed():
    # 1 class -> 2 mandatory baselines + SC = 3 fixed; a large optional set
    # must be trimmed to fit within _MAX_CONTEXTS (128).
    # optional_tokens: [(sop_class_uid, ts, kind), ...]
    cls = "1.2.840.10008.5.1.4.1.1.7"
    toks = [(cls, t, "stored") for t in (TS_470, RLE, TS_480)]
    kept = _trim_optional_ts(num_classes=1, has_sc=True, optional_tokens=toks)
    # budget = 128 - 1 - 2 = 125 optional allowed; only 3 -> nothing dropped
    assert set(kept) == set(toks)

    # Simulate an over-large set via a big class count so the trim has to drop.
    many = [(cls, f"1.2.840.10008.1.2.4.{100 + i}", "stored") for i in range(130)]
    kept2 = _trim_optional_ts(num_classes=1, has_sc=True, optional_tokens=many)
    assert len(kept2) <= 125, "must respect the 128-context budget"
    # unknown TS all have equal (max) canonical rank; the trim is still total
    assert len(kept2) <= 125


def test_budget_trim_is_per_class():
    """An overfull class must not cost ANOTHER class a transfer syntax.

    The budget is shared, but the victim list is not: a token is identified by
    (class, transfer syntax), so dropping a syntax from the class that
    overflowed leaves every other class intact.
    """
    hungry = "1.2.840.10008.5.1.4.1.1.7"
    quiet = "1.2.840.10008.5.1.4.1.1.2"
    toks = [(hungry, f"1.2.840.10008.1.2.4.{100 + i}", "stored") for i in range(130)]
    toks.append((quiet, TS_470, "stored"))
    kept = _trim_optional_ts(num_classes=2, has_sc=True, optional_tokens=toks)
    assert (
        quiet,
        TS_470,
        "stored",
    ) in kept, "the trim removed a transfer syntax from a class that had one token"
    assert len(kept) <= 128 - 1 - 4


# ---------------------------------------------------------------------------
# R1 pre-flight (mock HTTP; failure degrades to None)
# ---------------------------------------------------------------------------
class _FakeResp:
    """Minimal requests/httpx-like response with .json() and raise_for_status."""

    def __init__(self, payload, ok=True):
        self._payload = payload
        self._ok = ok
        self.status_code = 200 if ok else 500

    def json(self):
        return self._payload

    def raise_for_status(self):
        if not self._ok:
            raise RuntimeError("HTTP 500")


def test_preflight_metadata_path_map(monkeypatch):
    """R1 happy path: QIDO series + per-series metadata (00020010) yields
    dict[sop_class -> set[ts]]."""
    import clean_code.handlers.cmove as cm

    cm._preflight_cache.clear()
    series_rows = [{"0020000E": {"vr": "UI", "Value": [SERIES]}}]
    meta_rows = [
        {
            "00080016": {"vr": "UI", "Value": [CT]},
            "00020010": {"vr": "UI", "Value": [TS_470]},
        },
        {
            "00080016": {"vr": "UI", "Value": [CT]},
            "00020010": {"vr": "UI", "Value": [EVRLE]},
        },
    ]
    monkeypatch.setattr(cm, "_qido_request", lambda *a, **k: series_rows)

    def _fake_get(url, **kwargs):
        return _FakeResp(meta_rows)

    monkeypatch.setattr(cm.requests, "get", _fake_get)
    out = _preflight_study_ts_by_class(STUDY, "http://127.0.0.1/dicomweb")
    assert out == {CT: {TS_470, EVRLE}}


def test_preflight_metadata_multipart_related(monkeypatch):
    """0910 pre-flight multipart (PS3.18 10.4.4): a server that answers the
    metadata resource with the mandatory multipart/related media type (the
    field shape from the orchestrator's live finding, 09.09) must be parsed.
    The single part carries JSON identical to a plain application/dicom+json
    body, so the same class/TS map must be produced."""
    import clean_code.handlers.cmove as cm

    cm._preflight_cache.clear()
    series_rows = [{"0020000E": {"vr": "UI", "Value": [SERIES]}}]
    meta_rows = [
        {
            "00080016": {"vr": "UI", "Value": [CT]},
            "00020010": {"vr": "UI", "Value": [TS_470]},
        },
    ]
    monkeypatch.setattr(cm, "_qido_request", lambda *a, **k: series_rows)
    boundary = "18d3aa_9b7c"
    payload = (
        f"--{boundary}\r\n"
        "Content-Type: application/dicom+json\r\n"
        "\r\n" + json.dumps(meta_rows) + f"\r\n--{boundary}--\r\n"
    ).encode("utf-8")

    class _MultipartResp(_FakeResp):
        def __init__(self, body, ctype):
            super().__init__(None)
            self._body = body
            self.headers = {"Content-Type": ctype}

        @property
        def content(self):
            return self._body

        def json(self):
            raise ValueError("raw json of a multipart body must not be used")

    def _fake_get(url, **kwargs):
        ctype = (
            'multipart/related; type="application/dicom+json"; '
            f'boundary="{boundary}"'
        )
        return _MultipartResp(payload, ctype)

    monkeypatch.setattr(cm.requests, "get", _fake_get)
    out = _preflight_study_ts_by_class(STUDY, "http://127.0.0.1/dicomweb")
    assert out == {CT: {TS_470}}


def test_preflight_metadata_multipart_object_per_part(monkeypatch):
    """0909 hotfix regression (field, live clarus 8024): the origin server
    answers the PS3.18 10.4.4 metadata resource as multipart/related with ONE
    JSON OBJECT per instance per part (NOT a single JSON array). The old
    parser read only parts[0] and required a JSON array, discarding every
    part -> "0 class(es), 0 stored TS total" -> no stored-TS contexts were
    proposed and 4.70 instances could not be delivered. Now every part's
    payload is parsed and a bare dict counts as one instance."""

    import clean_code.handlers.cmove as cm

    cm._preflight_cache.clear()
    series_rows = [{"0020000E": {"vr": "UI", "Value": [SERIES]}}]
    monkeypatch.setattr(cm, "_qido_request", lambda *a, **k: series_rows)

    boundary = "18d3b30b89f0452a"
    inst_a = {
        "00080016": {"vr": "UI", "Value": [CT]},
        "00020010": {"vr": "UI", "Value": [TS_470]},
    }
    inst_b = {
        "00080016": {"vr": "UI", "Value": [CT]},
        "00020010": {"vr": "UI", "Value": [EVRLE]},
    }
    # Two parts, each a bare JSON OBJECT (one instance), exactly like 8024.
    payload = (
        f"--{boundary}\r\n"
        "Content-Type: application/dicom+json\r\n"
        "\r\n" + json.dumps(inst_a) + f"\r\n--{boundary}\r\n"
        "Content-Type: application/dicom+json\r\n"
        "\r\n" + json.dumps(inst_b) + f"\r\n--{boundary}--\r\n"
    ).encode("utf-8")

    class _MultipartResp(_FakeResp):
        def __init__(self, body):
            super().__init__(None)
            self._body = body
            self.headers = {"Content-Type": "application/dicom+json"}

        @property
        def content(self):
            return self._body

        def json(self):
            raise ValueError("raw json of a multipart body must not be used")

    def _fake_get(url, **kwargs):
        ctype = (
            'multipart/related; type="application/dicom+json"; '
            f'boundary="{boundary}"'
        )
        resp = _MultipartResp(payload)
        resp.headers = {"Content-Type": ctype}
        return resp

    monkeypatch.setattr(cm.requests, "get", _fake_get)
    out = _preflight_study_ts_by_class(STUDY, "http://127.0.0.1/dicomweb")
    assert out == {CT: {TS_470, EVRLE}}


def test_preflight_metadata_404_returns_none(monkeypatch):
    """R1 failure: metadata 404 -> the whole pre-flight returns None (the
    caller then degrades to the default proposal - never fails the job)."""
    import clean_code.handlers.cmove as cm

    cm._preflight_cache.clear()
    series_rows = [{"0020000E": {"vr": "UI", "Value": [SERIES]}}]
    monkeypatch.setattr(cm, "_qido_request", lambda *a, **k: series_rows)

    def _fake_get(url, **kwargs):
        raise RuntimeError("404")

    monkeypatch.setattr(cm.requests, "get", _fake_get)
    out = _preflight_study_ts_by_class(
        STUDY, "http://127.0.0.1/dicomweb", http_timeout=0.2
    )
    assert out is None


# ---------------------------------------------------------------------------
# Wire: R4 spec-12.1 - pre-flight present + stored TS rejected + server can
# produce -> refetch ON THE MAIN association, NO fallback A-ASSOCIATE.
# ---------------------------------------------------------------------------
class _Clarus(BaseHTTPRequestHandler):
    parts = []
    part_ts = []
    instances = {}
    caps = [EVRLE]
    series = [{"0020000E": {"vr": "UI", "Value": [SERIES]}}]
    metadata = [
        {
            "00080016": {"vr": "UI", "Value": [CT]},
            "00020010": {"vr": "UI", "Value": [TS_470]},
        }
    ]
    requests = []

    def log_message(self, *a):
        pass

    def do_GET(self):
        type(self).requests.append(f"GET {self.path}")
        if self.path == "/dicomweb":
            body = json.dumps(
                {"transferCapabilities": [{"transferSyntaxUid": list(self.caps)}]}
            ).encode()
            self._send(200, "application/json", body)
            return
        # QIDO series for the study (pre-flight step 1)
        if self.path == f"/dicomweb/studies/{STUDY}/series":
            body = json.dumps(self.series).encode()
            self._send(200, "application/dicom+json", body)
            return
        sup = f"/dicomweb/studies/{STUDY}/series/{SERIES}"
        if self.path == f"{sup}/metadata":
            body = json.dumps(self.metadata).encode()
            self._send(200, "application/dicom+json", body)
            return
        if "/instances/" in self.path:
            sop = self.path.rsplit("/", 1)[-1].split("?")[0]
            body = self.instances.get((STUDY, SERIES, sop), b"")
            self._send(200, "application/dicom", body)
            return
        if self.path == f"/dicomweb/studies/{STUDY}":
            boundary = "mb42"
            out = b""
            for i, part in enumerate(self.parts):
                out += (f"--{boundary}\r\n").encode()
                out += (
                    f"Content-Type: application/dicom; transfer-syntax={self.part_ts[i]}\r\n"
                    f"Content-Length: {len(part)}\r\n\r\n"
                ).encode()
                out += part
            out += f"\r\n--{boundary}--\r\n".encode()
            self._send(
                200,
                f'multipart/related; type="application/dicom"; boundary={boundary}',
                out,
            )
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

    @classmethod
    def reset(cls):
        cls.parts = []
        cls.part_ts = []
        cls.instances = {}
        cls.caps = [EVRLE]
        cls.requests = []


class _Scp:
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
            (str(ds.SOPInstanceUID), str(ds.file_meta.TransferSyntaxUID))
        )
        return 0x0000

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


class _Jobs:
    def __init__(self):
        self.failed = []
        self.success = []
        self.rec_failure = 0  # record_scp_failure calls (eviction counter)
        self.rec_success = 0  # record_scp_success calls
        self.failed_sops = {}  # job_id -> [failed sop uids]

    def is_job_cancelled(self, job_id):
        return False

    def update_job_progress(self, *a):
        pass

    def set_job_failed_sops(self, job_id, uids):
        self.failed_sops[job_id] = list(uids)

    def get_job_failed_sops(self, job_id):
        return list(self.failed_sops.get(job_id, []))

    def finish_job(self, job_id, success=False, error_message=None):
        if success:
            self.success.append(job_id)
        else:
            self.failed.append(error_message)

    def record_scp_success(self, *a, **k):
        self.rec_success += 1

    def record_scp_failure(self, *a, **k):
        self.rec_failure += 1

    def reload_peers_from_file(self, *a, **k):
        pass


def _run(job_id, scp, httpd, parts, part_ts, caps, instances):
    _Clarus.reset()
    _Clarus.caps = list(caps)
    _Clarus.parts = list(parts)
    _Clarus.part_ts = list(part_ts)
    _Clarus.instances.update(instances)
    from clean_code.handlers.cmove import _execute_cmove_job

    jobs = _Jobs()
    _execute_cmove_job(
        job_id=job_id,
        study_uid=STUDY,
        move_dest_ae="TEST_SCP",
        move_jobs=jobs,
        clarus_url=f"http://127.0.0.1:{httpd}/dicomweb",
        http_timeout=10,
        bridge_ae_title="CLARUS_BRIDGE",
        target_host="127.0.0.1",
        target_port=scp.port,
        sop_registry=SOP_REGISTRY,
        storage_commit_after_move=False,
        outbound_assoc_attempts=3,
    )
    return jobs


def _log_records():
    import logging

    recs = []

    class H(logging.Handler):
        def emit(self, r):
            recs.append(r.getMessage())

    h = H()
    h.setLevel(logging.DEBUG)
    root = logging.getLogger()
    root.addHandler(h)
    # pynetdicom module setup may push the root logger to WARNING; force INFO
    # for the capture window so INFO lines (pre-flight, contexts, refetch main)
    # are collected.
    prev_level = root.level
    if prev_level and prev_level > logging.INFO:
        root.setLevel(logging.INFO)
    return root, h, recs, prev_level


def test_preflight_rejected_stored_ts_refetch_on_main_no_fallback():
    """R4 / spec 12.1 rule 2: the pre-flight reports the study stores 4.70 so
    the bridge PROPOSES a 4.70 context; the SCP supports only EVRLE so 4.70 is
    rejected at negotiation. The server CAN produce EVRLE (caps=[EVRLE]); the
    instance is REFETCHED on the MAIN association (no fallback A-ASSOCIATE -
    the viewer already said no to 4.70)."""
    scp = _Scp([(CT, [EVRLE])])
    httpd = _free_port()
    srv = HTTPServer(("127.0.0.1", httpd), _Clarus)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        time.sleep(0.3)
        stored = _img_bytes(SOP_CT, TS_470)
        transcoded = _img_bytes(SOP_CT, EVRLE)
        _log_root, _log_h, recs, _prev = _log_records()
        _run(
            1,
            scp,
            httpd,
            [stored],
            [TS_470],
            caps=[EVRLE],
            instances={(STUDY, SERIES, SOP_CT): transcoded},
        )
        joined = "\n".join(recs)
        # only the main association is opened; no second/probe association
        assert (
            "TS-fallback association" not in joined
        ), f"{joined}\nHTTP:{_Clarus.requests}"
        assert (
            "refetching on the MAIN association" in joined
        ), f"{joined}\nHTTP:{_Clarus.requests}"
        assert "refetched=1" in joined, joined
        assert "transcoded=1" in joined, joined
        assert "passthrough=0" in joined, joined
        assert len(scp.received) == 1, scp.received
        assert scp.received[0][1] == EVRLE
    finally:
        try:
            _log_root.setLevel(_prev)
        except Exception:
            pass
        try:
            _log_root.removeHandler(_log_h)
        except Exception:
            pass
        scp.shutdown()
        srv.shutdown()
        srv.server_close()


# ---------------------------------------------------------------------------
# 0709 status fixes: D3 eviction - a partial job must NOT count against the
# peer (a single bad minute must not evict an alive peer).
# ---------------------------------------------------------------------------
def test_partial_job_does_not_touch_eviction_counter():
    """A C-MOVE job that delivered SOME slices and failed one (partial) must
    neither call record_scp_success nor record_scp_failure - in particular it
    must NOT bump the consecutive-failure eviction counter, which is reserved
    for MAIN-association failures (D3)."""
    scp = _Scp([(CT, [EVRLE])])  # native-only peer (EVRLE accepted)
    httpd = _free_port()
    srv = HTTPServer(("127.0.0.1", httpd), _Clarus)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        time.sleep(0.3)
        ok = _img_bytes(SOP_CT, EVRLE)  # delivered on the main association
        bad = _img_bytes(SOP_CT2, TS_470)  # not covered; no server transcode
        jobs = _run(
            1,
            scp,
            httpd,
            [ok, bad],
            [EVRLE, TS_470],
            caps=[],  # server: no transcode
            instances={},
        )
        # one slice delivered, one failed -> partial; job not 'success'
        assert len(scp.received) == 1, scp.received
        assert jobs.success == [], jobs.success
        # D3: partial outcome must not call the eviction counter.
        assert jobs.rec_failure == 0, jobs.rec_failure
        assert jobs.rec_success == 0, jobs.rec_success
        # the failed slice is captured for the (0008,0058) Failed SOP UID list
        assert SOP_CT2 in jobs.failed_sops.get(1, []), jobs.failed_sops
    finally:
        scp.shutdown()
        srv.shutdown()
        srv.server_close()


# ---------------------------------------------------------------------------
# 0609 field fix (jobs #3611-#3623): registry-wide proposal overflow
# ---------------------------------------------------------------------------
def _big_registry(n=80):
    """n fake image storage classes (no leading-zero UID components, PS3.5)."""
    return {
        f"1.2.840.10008.5.1.4.1.1.{i}": {"accept": True} for i in range(100, 100 + n)
    }


def test_preflight_classes_drive_proposal_not_registry():
    """With pre-flight the proposal covers ONLY the study's classes. The
    registry is 80 classes: the pre-fix code proposed 2 baselines per
    registry class and blew the 128-context budget (pynetdicom ValueError).
    With the fix: 1 study class -> a handful of contexts, association
    establishes and only that class is accepted."""
    scp = _Scp([(CT, [EVRLE, IVRLE, TS_470])])
    try:
        assoc, accepted = _associate_for_move(
            move_dest_ae="TEST_SCP",
            bridge_ae_title="CLARUS_BRIDGE",
            sop_registry=_big_registry(),
            target_host="127.0.0.1",
            target_port=scp.port,
            outbound_assoc_attempts=2,
            study_ts_by_class={CT: {TS_470}},
        )
        assert assoc.is_established
        assert set(accepted.keys()) == {CT}, accepted
        assoc.release()
    finally:
        scp.shutdown()


def test_no_preflight_big_registry_budget_drops_whole_classes():
    """Without pre-flight the registry-wide proposal must be trimmed to the
    128-context budget by dropping WHOLE classes (never a class's own
    EVRLE/IVRLE baseline pair). 80 classes x 2 baselines = 160 would
    overflow; the fix keeps at most 63 classes (127 contexts incl. the
    reserved Storage Commitment slot)."""
    reg = _big_registry()
    scp = _Scp([(uid, [EVRLE, IVRLE]) for uid in reg])
    try:
        assoc, accepted = _associate_for_move(
            move_dest_ae="TEST_SCP",
            bridge_ae_title="CLARUS_BRIDGE",
            sop_registry=reg,
            target_host="127.0.0.1",
            target_port=scp.port,
            outbound_assoc_attempts=2,
            study_ts_by_class=None,
        )
        assert assoc.is_established
        assert len(accepted) <= 63, f"{len(accepted)} classes accepted"
        assoc.release()
    finally:
        scp.shutdown()


# ---------------------------------------------------------------------------
# TTL cache of successful pre-flights (0609 field: repeated C-MOVE of the
# same study must not re-walk the PS3.18 10.4.4 metadata)
# ---------------------------------------------------------------------------
def test_preflight_cache_hit_skips_http(monkeypatch):
    import clean_code.handlers.cmove as cm

    cm._preflight_cache.clear()
    series_rows = [{"0020000E": {"vr": "UI", "Value": [SERIES]}}]
    meta_rows = [
        {
            "00080016": {"vr": "UI", "Value": [CT]},
            "00020010": {"vr": "UI", "Value": [TS_470]},
        }
    ]
    calls = {"get": 0}
    monkeypatch.setattr(cm, "_qido_request", lambda *a, **k: series_rows)

    def _fake_get(url, **kwargs):
        calls["get"] += 1
        return _FakeResp(meta_rows)

    monkeypatch.setattr(cm.requests, "get", _fake_get)
    out1 = _preflight_study_ts_by_class(STUDY, "http://x/dicomweb")
    out2 = _preflight_study_ts_by_class(STUDY, "http://x/dicomweb")
    assert out1 == {CT: {TS_470}}
    assert out2 == out1
    assert calls["get"] == 1, f"metadata fetched {calls['get']} times"


def test_preflight_cache_expiry_refetches(monkeypatch):
    import clean_code.handlers.cmove as cm

    cm._preflight_cache.clear()
    series_rows = [{"0020000E": {"vr": "UI", "Value": [SERIES]}}]
    meta_rows = [
        {
            "00080016": {"vr": "UI", "Value": [CT]},
            "00020010": {"vr": "UI", "Value": [TS_470]},
        }
    ]
    calls = {"get": 0}
    monkeypatch.setattr(cm, "_qido_request", lambda *a, **k: series_rows)

    def _fake_get(url, **kwargs):
        calls["get"] += 1
        return _FakeResp(meta_rows)

    monkeypatch.setattr(cm.requests, "get", _fake_get)
    _preflight_study_ts_by_class(STUDY, "http://x/dicomweb")
    for key in list(cm._preflight_cache.keys()):
        _, val = cm._preflight_cache[key]
        cm._preflight_cache[key] = (0.0, val)  # expire
    _preflight_study_ts_by_class(STUDY, "http://x/dicomweb")
    assert calls["get"] == 2


def test_preflight_failure_not_cached(monkeypatch):
    import clean_code.handlers.cmove as cm

    cm._preflight_cache.clear()
    series_rows = [{"0020000E": {"vr": "UI", "Value": [SERIES]}}]
    calls = {"get": 0}
    monkeypatch.setattr(cm, "_qido_request", lambda *a, **k: series_rows)

    def _fake_get(url, **kwargs):
        calls["get"] += 1
        raise RuntimeError("404")

    monkeypatch.setattr(cm.requests, "get", _fake_get)
    assert _preflight_study_ts_by_class(STUDY, "http://x/dicomweb") is None
    assert _preflight_study_ts_by_class(STUDY, "http://x/dicomweb") is None
    assert calls["get"] == 2
    assert cm._preflight_cache == {}
