# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1211 convergence batch 8 regression tests, one per finding (R1..R19).

Each test fails when its fix is reverted: the assertion targets the behaviour
the finding described, not the shape of the code.
"""

import os
import struct
import threading
import time

from pydicom.dataelem import DataElement
from pydicom.dataset import Dataset

MR = "1.2.840.10008.5.1.4.1.1.4"
CT = "1.2.840.10008.5.1.4.1.1.2"


class _InlinePrefetcher:
    """Stand-in for PartPrefetcher that runs the generator in the calling
    thread, so the reader's exception surfaces at the `for part` site - the
    same place the real prefetcher raises it."""

    def __init__(self, source, *a, **kw):
        self._source = source

    def __iter__(self):
        return iter(self._source)

    def start(self):
        return self

    def stop(self):
        pass


# ---------------------------------------------------------------------------
# R1 - the head deadline holds INSIDE readline
# ---------------------------------------------------------------------------
def test_a_byte_per_window_head_answers_408(monkeypatch):
    """A buffered `readline` loops internally, so arming the socket once per
    call still gave every internal recv a fresh window. Batch 8 assembles the
    line from read(1) calls with the deadline checked between them."""
    from clean_code import http_server as hs

    state = {"now": 1000.0}
    monkeypatch.setattr(hs.time, "monotonic", lambda: state["now"])

    class _Drip:
        """One byte per fake second, never a newline: only the deadline ends it."""

        def __init__(self):
            self.reads = 0

        def read(self, n=-1):
            self.reads += 1
            state["now"] += 1.0
            return b"X"

    drip = _Drip()
    h = hs.BridgeHTTPHandler.__new__(hs.BridgeHTTPHandler)
    h.rfile = drip
    h.connection = None
    h.command = None
    h.request_version = "HTTP/1.1"
    h.default_request_version = "HTTP/1.0"
    h.errors = []
    h.send_error = lambda code, msg=None: h.errors.append(code)
    h.log_message = lambda *a, **k: None
    h.send_response = lambda *a, **k: None
    h.send_header = lambda *a, **k: None
    h.end_headers = lambda: None
    h.wfile = type(
        "W", (), {"write": lambda *a, **k: None, "flush": lambda *a, **k: None}
    )()

    h.handle_one_request()

    assert 408 in h.errors, h.errors
    # Bounded: the read loop must stop at the budget, not read for ever.
    assert drip.reads <= hs.HEAD_TOTAL_TIMEOUT_SEC + 2, drip.reads


# ---------------------------------------------------------------------------
# R2 - multi-valued series / SOP UIDs are not collapsed into one string
# ---------------------------------------------------------------------------
def test_a_multi_valued_series_uid_never_becomes_a_python_list_string():
    """`str(MultiValue)` renders "['1.2.3', '1.2.4']", which then became a
    WADO-RS path segment and turned a legal SERIES C-MOVE into a 404."""
    from clean_code.handlers.cmove import _parse_c_move_rq

    from tests.conftest import make_fake_move_event

    event = make_fake_move_event()
    event.identifier.QueryRetrieveLevel = "SERIES"
    event.identifier.StudyInstanceUID = "1.2.3"
    # Bypass the single-value check: the wire CAN carry several values for a
    # Type 1 single-value key, and that is the shape the fix must survive.
    event.identifier._dict[0x0020000E] = DataElement(
        0x0020000E,
        "UI",
        __import__("pydicom").multival.MultiValue(str, ["1.2.3.4", "1.2.3.5"]),
    )
    _study, _dest, level, series_uid, _sop = _parse_c_move_rq(event)

    assert level == "SERIES"
    assert "[" not in series_uid and "'" not in series_uid, series_uid
    assert series_uid == "1.2.3.4", series_uid


def test_a_list_shaped_uid_value_is_unwrapped_not_refused():
    """A single rendered list still scopes ONE series, so its UID is recovered;
    what must never happen is the bracket-and-quote text reaching the URL."""
    from clean_code.handlers.cmove import _parse_c_move_rq

    from tests.conftest import make_fake_move_event

    event = make_fake_move_event()
    event.identifier.QueryRetrieveLevel = "SERIES"
    event.identifier.StudyInstanceUID = "1.2.3"
    event.identifier.SeriesInstanceUID = "['1.2.3.4']"
    _study, _dest, _level, series_uid, _sop = _parse_c_move_rq(event)

    assert series_uid == "1.2.3.4", series_uid


def test_an_ambiguous_container_uid_is_refused():
    """A quoted value keeps its ambiguity after trimming, so it is refused
    rather than guessed."""
    from clean_code.handlers.cmove import _parse_c_move_rq

    from tests.conftest import make_fake_move_event

    event = make_fake_move_event()
    event.identifier.QueryRetrieveLevel = "SERIES"
    event.identifier.StudyInstanceUID = "1.2.3"
    event.identifier.SeriesInstanceUID = "['1.2.3.4', '1.2.3.5']"
    try:
        _parse_c_move_rq(event)
    except ValueError as exc:
        assert "SeriesInstanceUID" in str(exc), exc
    else:  # pragma: no cover - the assertion below reports it
        raise AssertionError("an ambiguous container was accepted into the URL")


# ---------------------------------------------------------------------------
# R3 - a stream-read failure keeps the delivered prefix
# ---------------------------------------------------------------------------
def test_a_stream_read_failure_reports_the_delivered_prefix(monkeypatch):
    """`for part in prefetch` raising outside the per-instance try cost the
    driver this pass's `sent_sop_uids`, so the resume re-sent everything and a
    pass without a resume answered 0xC000 for a study that had moved."""
    import clean_code.handlers.cmove as cm

    from tests.test_spill import (
        BOUNDARY,
        EVRLE,
        STUDY,
        _body,
        _final,
        _part_bytes,
        _status,
    )
    from tests.test_spill import (
        CT as SPILL_CT,
    )

    sop1 = "2.25.9990001112223331"
    sop2 = "2.25.9990001112223332"
    # Part 1 arrives in full and is delivered; part 2 is announced and then the
    # socket dies, so the failure lands in the middle of the part loop - the
    # case where the driver already has a non-empty delivered list.
    body = _part_bytes(_body(64 * 1024, sop_uid=sop1))
    tail = _part_bytes(_body(64 * 1024, sop_uid=sop2)) + _final()

    class _TruncatingResponse:
        """Yields the whole first part, then raises the way a broken socket
        does. `iter_content` is consumed lazily, so the first part reaches the
        caller before the error - which is exactly the case that used to lose
        the delivered list."""

        headers = {
            "Content-Type": f'multipart/related; type="application/dicom"; boundary={BOUNDARY}'
        }

        def __init__(self):
            self.closed = False

        def iter_content(self, chunk_size=65536):
            yield body
            # Part 2's header is on the wire, its body never arrives.
            yield tail[:40]
            raise ConnectionError("socket reset mid-response")

        def close(self):
            self.closed = True

        def raise_for_status(self):
            pass

    sent = []

    class _Assoc:
        is_established = True

        def send_c_store(self, path):
            sent.append(path)
            return _status(0x0000)

    class _Jobs:
        def update_job_progress(self, *a):
            pass

        def is_job_cancelled(self, job_id):
            return False

    monkeypatch.setattr(cm.requests, "get", lambda *a, **kw: _TruncatingResponse())
    monkeypatch.setattr(cm, "PartPrefetcher", _InlinePrefetcher)

    result = cm._stream_and_send_study(
        assoc=_Assoc(),
        study_uid=STUDY,
        clarus_url="http://127.0.0.1:9/dicomweb",
        http_timeout=5,
        move_jobs=_Jobs(),
        job_id=1,
        accepted_contexts_map={SPILL_CT: EVRLE},
        server_ts_list=[EVRLE],
        sop_registry={SPILL_CT: {"accept": True}},
        _report_lost=True,
    )

    completed, _failed, _total, sent_sops, _fs, _cancelled, _nc, lost = result
    assert completed == 1, "the instance delivered before the failure is kept"
    assert [u for _c, u in sent_sops] == [sop1], sent_sops
    assert lost is True, "the pass must report itself lost, not swallow the error"


# ---------------------------------------------------------------------------
# R4 - the registry fill-in runs only on a PARTIAL pre-flight
# ---------------------------------------------------------------------------
def test_a_complete_preflight_does_not_add_the_whole_registry(monkeypatch):
    """Adding every registry class to a complete pre-flight ate the
    128-context budget, and the trim then dropped the study's OWN stored
    syntaxes - a compressed study proposed only EVRLE/IVRLE."""
    import clean_code.handlers.cmove as cm

    built = {}

    class _AE:
        def __init__(self, ae_title=""):
            self.contexts = []
            built["ae"] = self

        def add_requested_context(self, sop, ts=None):
            self.contexts.append((sop, ts))

        def associate(self, *a, **k):
            return type("A", (), {"is_established": True, "accepted_contexts": []})()

    monkeypatch.setattr(cm, "AE", _AE)

    # 70 registry classes, all accepted; the study holds ONE of them
    # compressed with JPEG Lossless.
    registry = {f"1.2.3.4.5.{i}": {"accept": True} for i in range(70)}
    registry[CT] = {"accept": True}
    study = {CT: {"1.2.840.10008.1.2.4.70"}}

    cm._associate_for_move(
        "DEST",
        "BRIDGE",
        registry,
        "10.0.0.1",
        11112,
        study_ts_by_class=study,
        partial_preflight=False,
    )

    proposed = {sop for sop, _ts in built["ae"].contexts}
    # Storage Commitment is always proposed on the main association.
    proposed.discard("1.2.840.10008.1.20.1")
    assert proposed == {CT}, (
        "a complete pre-flight must propose ONLY the study's classes, "
        f"got {len(proposed)} classes"
    )
    ts_for_ct = {ts for sop, ts in built["ae"].contexts if sop == CT}
    assert (
        "1.2.840.10008.1.2.4.70" in ts_for_ct
    ), "the study's stored syntax must survive the budget trim"


def test_a_partial_preflight_still_fills_in_the_registry(monkeypatch):
    """The fill-in exists for a walk that MISSED series: those classes come
    from the registry so the move is not silently incomplete."""
    import clean_code.handlers.cmove as cm

    built = {}

    class _AE:
        def __init__(self, ae_title=""):
            self.contexts = []
            built["ae"] = self

        def add_requested_context(self, sop, ts=None):
            self.contexts.append((sop, ts))

        def associate(self, *a, **k):
            return type("A", (), {"is_established": True, "accepted_contexts": []})()

    monkeypatch.setattr(cm, "AE", _AE)
    registry = {CT: {"accept": True}, "1.2.840.10008.5.1.4.1.1.4": {"accept": True}}

    cm._associate_for_move(
        "DEST",
        "BRIDGE",
        registry,
        "10.0.0.1",
        11112,
        study_ts_by_class={CT: {"1.2.840.10008.1.2.1"}},
        partial_preflight=True,
    )

    proposed = {sop for sop, _ts in built["ae"].contexts}
    assert MR in proposed, "a partial walk still needs the registry fill-in"


# ---------------------------------------------------------------------------
# R5 - the MWL start-date matching key is not dropped
# ---------------------------------------------------------------------------
def test_the_mwl_start_date_is_translated_not_dropped():
    """(0040,0002) is a REQUIRED MWL matching key (PS3.4 Table K.6-1). Without
    it the search returned workitems scheduled for other days."""
    from clean_code.protocol import _build_mwl_params
    from pydicom import Dataset

    ds = Dataset()
    sps = Dataset()
    sps.ScheduledStationAETitle = "STATION01"
    sps.ScheduledProcedureStepStartDate = "20260802"
    ds.ScheduledProcedureStepSequence = [sps]

    params = _build_mwl_params(ds)
    assert "00404005" in params, params
    lo, _, hi = params["00404005"].partition("-")
    assert lo.startswith("20260802") and hi.startswith("20260802"), params


def test_an_mwl_date_range_keeps_both_bounds():
    from clean_code.protocol import _build_mwl_params
    from pydicom import Dataset

    ds = Dataset()
    sps = Dataset()
    sps.ScheduledStationAETitle = "STATION01"
    sps.ScheduledProcedureStepStartDate = "20260801-20260831"
    ds.ScheduledProcedureStepSequence = [sps]

    value = _build_mwl_params(ds)["00404005"]
    assert value.startswith("20260801000000"), value
    assert value.endswith("20260831235959"), value


# ---------------------------------------------------------------------------
# R6 - UTF-8 without a charset label is not "repaired" into cp1251 mojibake
# ---------------------------------------------------------------------------
def test_utf8_text_without_a_charset_is_left_readable():
    from clean_code.handlers.cstore import _fix_cp1251_krakozyabry
    from pydicom import Dataset

    name = "Петров"
    ds = Dataset()
    # pydicom read the UTF-8 bytes as Latin-1: the string carries the mojibake
    # form while its byte content is still valid UTF-8.
    ds.PatientName = name.encode("utf-8").decode("latin-1")

    _fix_cp1251_krakozyabry(ds)

    assert (
        str(ds.PatientName) == name
    ), "valid UTF-8 must be recovered, not re-decoded as cp1251"


def test_real_cp1251_text_is_still_repaired():
    """The cp1251 path must keep working: the strict-UTF-8 probe is a
    discriminator, not a replacement."""
    from clean_code.handlers.cstore import _fix_cp1251_krakozyabry
    from pydicom import Dataset

    name = "Петров"
    ds = Dataset()
    ds.PatientName = name.encode("cp1251").decode("latin-1")

    _fix_cp1251_krakozyabry(ds)

    assert str(ds.PatientName) == name


# ---------------------------------------------------------------------------
# R9 / R10 - the audit entry carries no unregistered tag and keeps falsy olds
# ---------------------------------------------------------------------------
def test_the_audit_entry_has_no_0400_0566():
    """(0400,0566) is not in the PS3.6 registry and group 0400 is even, so it
    cannot be private either (PS3.5 7.8.1)."""
    from clean_code.validation import record_modification
    from pydicom import Dataset

    ds = Dataset()
    ds.PatientID = "RAW"
    record_modification(ds, (0x0010, 0x0020), "OLD", modifying_system="B")

    item = ds.OriginalAttributesSequence[0]
    assert (0x0400, 0x0566) not in item
    assert (0x0400, 0x0563) in item, "ModifyingSystem is the 'who' that remains"


def test_a_falsy_old_value_is_recorded_verbatim():
    """PS3.3 C.12.1.1.9.1: the item holds the PREVIOUS value. `old_value or ""`
    replaced 0, 0.0 and False with an empty string."""
    from clean_code.validation import record_modification
    from pydicom import Dataset

    for old in (0, 0.0, False):
        ds = Dataset()
        ds.add_new((0x0028, 0x0002), "US", 1)
        record_modification(ds, (0x0028, 0x0002), old, modifying_system="B")
        recorded = (
            ds.OriginalAttributesSequence[0]
            .ModifiedAttributesSequence[0][(0x0028, 0x0002)]
            .value
        )
        assert recorded == old, (old, recorded)


# ---------------------------------------------------------------------------
# R11 - a frozen verdict stops the verification walk
# ---------------------------------------------------------------------------
def test_a_frozen_verdict_is_not_verified_again(tmp_path):
    from clean_code.commit_tracker import CommitTracker

    calls = []

    def _verify(sop_class, sop_uid):
        calls.append(sop_uid)
        return True

    tracker = CommitTracker(
        verify=_verify,
        report=lambda *a, **k: True,
        interval=0.1,
        deadline_sec=300.0,
    )
    tracker.register("1.2.3.99", "MOD", "10.0.0.5", 104, [(MR, "1.2.3.4")])
    job = tracker._jobs["1.2.3.99"]
    job.frozen_verdict = ([(MR, "1.2.3.4")], [], 1)  # already decided
    job.deadline = time.time() + 3600

    tracker._tick()

    assert calls == [], "a frozen verdict must not be verified again"


# ---------------------------------------------------------------------------
# R12 - an unreadable PID file does not abort startup
# ---------------------------------------------------------------------------
def test_an_unreadable_pid_file_does_not_raise(tmp_path, monkeypatch):
    import clean_code.main as m

    path = tmp_path / "clbridge.pid"
    path.write_text(str(os.getpid()), encoding="utf-8")

    real_open = open

    def _open(p, *a, **k):
        if str(p) == str(path):
            raise PermissionError("denied")
        return real_open(p, *a, **k)

    monkeypatch.setattr("builtins.open", _open)
    # Must not raise: the service has to start.
    m._write_pid_file(str(path))


# ---------------------------------------------------------------------------
# R13 - expose_port is range-checked like every other port
# ---------------------------------------------------------------------------
def test_an_out_of_range_expose_port_is_a_config_error(tmp_path):
    import pytest
    from clean_code.config import ConfParser
    from clean_code.main import _validate_config

    cfg_path = tmp_path / "clbridge.conf"
    cfg_path.write_text(
        "[server]\nport = 8104\nexpose_port = 70000\n"
        "[dicomweb]\nclarus_url = http://127.0.0.1:8024/dicomweb\n",
        encoding="utf-8",
    )
    with pytest.raises(ValueError) as exc:
        _validate_config(ConfParser(str(cfg_path)))
    assert "expose_port" in str(exc.value), exc.value


# ---------------------------------------------------------------------------
# R14 - a broken registry entry drops that entry, not the operator's choices
# ---------------------------------------------------------------------------
def test_a_mistyped_entry_keeps_the_disabled_class_disabled(tmp_path):
    from clean_code.registry import _is_sop_accepted, _load_sop_registry

    path = tmp_path / "sop_classes.yaml"
    path.write_text(
        "sop_classes:\n"
        "  1.2.840.10008.5.1.4.1.1.2:\n"
        "    accept: false\n"
        "  1.2.840.10008.5.1.4.1.1.4:\n"
        "    accept: yes\n",
        encoding="utf-8",
    )
    registry = _load_sop_registry(str(path))
    assert (
        _is_sop_accepted(registry, "1.2.840.10008.5.1.4.1.1.2") is False
    ), "one mistyped entry must not re-enable a class the operator switched off"


# ---------------------------------------------------------------------------
# R15 - an association loss does not burn a retry on the rest of the batch
# ---------------------------------------------------------------------------
def test_an_association_loss_does_not_charge_the_rest_of_the_batch(monkeypatch):
    import clean_code.main as m

    charged = []
    requeued = []

    class _Outbox:
        def mark_delivered(self, uid):
            pass

        def mark_failed(self, uid, error):
            charged.append(uid)

        def requeue(self, uid, reason):
            requeued.append(uid)

        def read_mods_sidecar(self, path):
            return None

    class _Ctx:
        abstract_syntax = CT
        transfer_syntax = ["1.2.840.10008.1.2.1"]

    class _Assoc:
        """Item 1 comes back with no RSP and the association is then dead; the
        rest of the batch must be requeued WITHOUT a retry burn."""

        accepted_contexts = [_Ctx()]

        def __init__(self):
            self.is_established = True
            self.sends = 0

        def send_c_store(self, path):
            self.sends += 1
            self.is_established = False  # the peer dropped the association
            return Dataset()  # empty: no Status at all

        def release(self):
            pass

    items = [
        ("1.2.3.1", CT, "1.2.840.10008.1.2.1", "/tmp/a.dcm"),
        ("1.2.3.2", CT, "1.2.840.10008.1.2.1", "/tmp/b.dcm"),
    ]

    monkeypatch.setattr(m, "_forward_open_assoc", lambda *a, **k: _Assoc())
    monkeypatch.setattr(
        m,
        "_forward_blob_size",
        lambda p: 0,
    )

    # Item 1 answers with NO RSP and the association is reported dead, so the
    # rest of the batch is requeued by the caller WITHOUT a retry burn.
    ok = m._forward_process_batch(
        _Outbox(), items, "DEST", "BRIDGE", "10.0.0.1", 104, 30, 30, 15
    )
    assert ok is False
    assert charged == [], f"a lost association must not burn retries: {charged}"


# ---------------------------------------------------------------------------
# R16 - a timeout before the request line still answers 408
# ---------------------------------------------------------------------------
def test_a_stall_on_the_first_byte_answers_408(monkeypatch):
    """`send_error` reads `self.requestline`; it did not exist yet."""
    from clean_code import http_server as hs

    class _Stall:
        def read(self, n=-1):
            raise hs._HeadDeadlineExceeded()

    h = hs.BridgeHTTPHandler.__new__(hs.BridgeHTTPHandler)
    h.rfile = _Stall()
    h.connection = None
    h.command = None
    h.request_version = "HTTP/1.1"
    h.default_request_version = "HTTP/1.0"
    h.errors = []
    h.send_error = lambda code, msg=None: h.errors.append(code)
    h.log_message = lambda *a, **k: None

    h.handle_one_request()

    assert h.errors == [408], h.errors


# ---------------------------------------------------------------------------
# R17 - a reloaded C-STORE limit cannot be released through the wrong object
# ---------------------------------------------------------------------------
def test_a_reloaded_semaphore_is_not_over_released():
    from clean_code.handlers import cstore

    original = cstore._cstore_semaphore
    original_max = cstore._MAX_CONCURRENT_CSTORE
    try:
        cstore.set_max_concurrent_cstore(1)
        old_sem = cstore._cstore_semaphore
        # The handler took the OLD semaphore...
        assert old_sem.acquire(blocking=False)
        # ...and the limit is reloaded while it is in flight.
        cstore.set_max_concurrent_cstore(1)
        new_sem = cstore._cstore_semaphore
        assert new_sem is not old_sem
        # Releasing the NEW one would raise its counter above the limit.
        before = new_sem._value
        old_sem.release()
        assert (
            new_sem._value == before
        ), "the release must reach the semaphore that was acquired"
    finally:
        cstore._cstore_semaphore = original
        cstore._MAX_CONCURRENT_CSTORE = original_max


# ---------------------------------------------------------------------------
# R18 - dequeue reuses its listing instead of re-stat'ing the whole backlog
# ---------------------------------------------------------------------------
def test_dequeue_does_not_rescan_the_directory_for_every_claim(tmp_path, monkeypatch):
    import clean_code.outbox as ob

    from tests.test_outbox import _make_dataset

    q = ob.OutboxQueue(str(tmp_path / "outbox"))
    for i in range(3):
        q.enqueue(_make_dataset(f"1.2.840.10008.5.1.4.1.1.4.{i}"))

    listings = [0]
    real_listdir = ob.os.listdir

    def _listdir(p):
        if str(p) == q._pending:
            listings[0] += 1
        return real_listdir(p)

    monkeypatch.setattr(ob.os, "listdir", _listdir)

    claimed = [q.dequeue() for _ in range(3)]

    assert all(c[0] for c in claimed), claimed
    # One listing for the whole drain: the cache is only invalidated when an
    # enqueue publishes (here: before the loop) or the listing runs dry.
    assert listings[0] == 1, f"dequeue re-listed the directory {listings[0]} times"


# ---------------------------------------------------------------------------
# R19 - stop() does not sit out the join when the reader is blocked in the read
# ---------------------------------------------------------------------------
def test_stop_returns_promptly_when_the_reader_is_blocked_in_the_source():
    from clean_code.prefetch import PartPrefetcher

    release = threading.Event()
    closed = threading.Event()

    def _source():
        # Blocked inside the source, as `iter_content` is on a silent peer.
        release.wait(30.0)
        yield b"never", 0

    def _on_stop():
        closed.set()
        release.set()

    prefetch = PartPrefetcher(_source(), on_stop=_on_stop).start()
    try:
        time.sleep(0.05)  # let the reader reach the blocking wait
        started = time.monotonic()
        prefetch.stop()
        elapsed = time.monotonic() - started
        assert closed.is_set(), "stop must invoke the on_stop callback"
        assert elapsed < 2.0, f"stop() blocked for {elapsed:.1f}s"
    finally:
        release.set()


# ---------------------------------------------------------------------------
# R7 - the MWL e2e test demands a terminal Success
# ---------------------------------------------------------------------------
def test_the_mwl_e2e_test_asserts_a_terminal_success():
    """A bridge that answered one valid Pending and then 0xA700 used to pass
    every assertion in the e2e test, because the final status was ignored."""
    path = os.path.join(os.path.dirname(__file__), "test_mwl_e2e.py")
    with open(path, encoding="utf-8") as f:
        src = f.read()
    assert (
        "statuses[-1] == 0x0000" in src
    ), "the e2e MWL test must require a terminal Success status"


# ---------------------------------------------------------------------------
# R8 - the file-meta writer emits every PS3.10 Table 7.1-1 Type 1 element
# ---------------------------------------------------------------------------
def test_the_file_meta_group_carries_every_mandatory_element():
    from clean_code.handlers.cmove import _ensure_file_meta_bytes, _walk_meta

    body = (
        struct.pack("<HH", 0x0008, 0x0016)
        + b"UI"
        + struct.pack("<H", 26)
        + CT.encode("ascii")
        + b"\x00"
    )
    out = _ensure_file_meta_bytes(b"\x00" * 128 + b"DICM" + body, "", "", MR)
    _, elems, _meta_end = _walk_meta(out)
    tags = [e for e, _v in elems]
    # PS3.10 7.1 Table 7.1-1 Type 1: every one of the six, no exceptions.
    for tag in (0x0000, 0x0001, 0x0002, 0x0003, 0x0010, 0x0012):
        assert tag in tags, (tag, tags)
