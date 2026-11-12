# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1211 convergence batch 7 regression tests, one per finding (F1..F14).

Each test fails when its fix is reverted: the assertion targets the behaviour
the finding described, not the shape of the code.
"""

import os
import threading
import time

MR = "1.2.840.10008.5.1.4.1.1.4"


# ---------------------------------------------------------------------------
# F1 - the request head is bounded by ONE monotonic deadline
# ---------------------------------------------------------------------------
def test_a_trickling_head_answers_408(monkeypatch):
    """A peer trickling one header byte per window must exhaust the budget:
    `settimeout` bounds ONE recv, so a single up-front timeout gives every
    byte a fresh window and never answers."""
    from clean_code import http_server as hs

    state = {"now": 1000.0}
    monkeypatch.setattr(hs.time, "monotonic", lambda: state["now"])

    class _Drip:
        def __init__(self):
            self._request_line = iter(b"GET /health HTTP/1.1\r\n")
            self._drip = iter(b"Host: " + b"x" * 50 + b"\r\n")

        def read(self, n=-1):
            try:
                return bytes([next(self._request_line)])
            except StopIteration:
                pass
            state["now"] += 1.0
            try:
                return bytes([next(self._drip)])
            except StopIteration:
                return b"\r\n"

    h = hs.BridgeHTTPHandler.__new__(hs.BridgeHTTPHandler)
    h.rfile = _Drip()
    h.connection = None
    h.command = ""
    h.request_version = "HTTP/1.1"
    h.errors = []
    h.send_error = lambda code, msg=None: h.errors.append(code)
    h.log_message = lambda *a, **k: None
    h.send_response = lambda *a, **k: None
    h.send_header = lambda *a, **k: None
    h.end_headers = lambda: None
    h.wfile = type("W", (), {"write": lambda *a, **k: None, "flush": lambda *a, **k: None})()

    h.handle_one_request()
    assert 408 in h.errors, h.errors


# ---------------------------------------------------------------------------
# F2 - a broken health check must zero the gauge, not keep the last value
# ---------------------------------------------------------------------------
def test_a_broken_health_check_zeros_the_gauge(monkeypatch):
    from clean_code import http_server as hs

    class _Boom:
        def is_healthy(self):
            raise RuntimeError("broken checker")

    monkeypatch.setattr(hs.BridgeHTTPHandler, "_health_checker_ref", _Boom())
    hs.server_health.set(1)

    h = hs.BridgeHTTPHandler.__new__(hs.BridgeHTTPHandler)
    h.path = "/metrics"
    h.wfile = type("W", (), {"write": lambda *a, **k: None})()
    h.send_response = lambda *a, **k: None
    h.send_header = lambda *a, **k: None
    h.end_headers = lambda: None
    h.log_message = lambda *a, **k: None
    h.do_GET()

    assert hs.server_health._value.get() == 0, (
        "a raising checker must not keep reporting the last good value"
    )


# ---------------------------------------------------------------------------
# F3 - with a database, the report is withheld until the verdict is persisted
# ---------------------------------------------------------------------------
def test_the_report_is_withheld_until_the_verdict_is_persisted(tmp_path, monkeypatch):
    from tests.test_commit_tracker_persistence import SOP_LIST, _Db, _tracker

    tracker = _tracker(_Db(str(tmp_path / "c.db")))
    tracker.register("1.2.3.91", "MODALITY", "10.0.0.5", 104, SOP_LIST)
    job = tracker._jobs["1.2.3.91"]
    job.deadline = 0.0  # resolve on the first tick

    reports = []
    tracker._report = lambda *a, **k: reports.append(a)

    real_persist = tracker._persist
    state = {"n": 0}

    def _flaky(j):
        state["n"] += 1
        if state["n"] == 1:
            raise OSError("disk full")
        return real_persist(j)

    monkeypatch.setattr(tracker, "_persist", _flaky)

    tracker._tick()
    assert reports == [], "no report may leave before the verdict is on disk"
    tracker._tick()
    assert len(reports) == 1, "the report must follow the successful persist"
    tracker.close()


# ---------------------------------------------------------------------------
# F4 - a failed DELETE leaves the abandoned entry for the next sweep
# ---------------------------------------------------------------------------
def test_a_failed_delete_keeps_the_abandoned_entry(tmp_path):
    from tests.test_commit_tracker_persistence import SOP_LIST, _Db, _tracker

    tracker = _tracker(_Db(str(tmp_path / "c.db")))
    tracker.register("1.2.3.92", "MODALITY", "10.0.0.5", 104, SOP_LIST)
    job = tracker._jobs.pop("1.2.3.92")
    job.abandoned = True
    job.abandoned_at = 0.0
    tracker._abandoned["1.2.3.92"] = job

    class _Boom:
        def locked(self):
            raise OSError("cannot delete")

    tracker._db = _Boom()
    assert tracker.purge_abandoned(max_age_sec=-1.0) == 0
    assert "1.2.3.92" in tracker._abandoned, (
        "a failed DELETE must leave the entry for the next sweep"
    )
    tracker.close()


# ---------------------------------------------------------------------------
# F5 - the cleanup sweep must not unlink a replacement published mid-sweep
# ---------------------------------------------------------------------------
def test_cleanup_keeps_a_replacement_published_during_the_sweep(tmp_path, monkeypatch):
    import clean_code.sop_index as si

    idx = si.SopIndexManager(str(tmp_path / "sop_index"))
    uid = "1.2.840.10008.5.1.4.1.1.4.911"
    idx.insert_sop(uid, "1.2.3", "1.2.3.4")
    path = idx._path(uid)
    old = time.time() - 7200
    os.utime(path, (old, old))

    real_getmtime = si.os.path.getmtime
    reached = threading.Event()
    proceed = threading.Event()

    def _getmtime(p):
        if p == path:
            # The sweep decides on the mtime it reads NOW; the race is the
            # unlink that follows. Return the SAME stale snapshot after the
            # replacement has been published, so the unlink (without the
            # guard) deletes the FRESH file it did not inspect.
            stale = real_getmtime(p)
            reached.set()
            proceed.wait(10.0)
            return stale
        return real_getmtime(p)

    monkeypatch.setattr(si.os.path, "getmtime", _getmtime)

    inserted = threading.Event()

    def _insert():
        idx.insert_sop(uid, "1.2.9", "1.2.9.9")
        inserted.set()

    sweep = threading.Thread(
        target=lambda: idx.cleanup(max_age_seconds=3600), daemon=True
    )
    sweep.start()
    try:
        assert reached.wait(5.0), "the sweep never read the entry mtime"
        insert_thread = threading.Thread(target=_insert, daemon=True)
        insert_thread.start()
        # Without the guard the insert lands while the sweep is parked between
        # its mtime read and its unlink; with it the insert is parked behind
        # the sweep's guard and finishes after the sweep unlinks the OLD file.
        insert_thread.join(timeout=1.0)
        proceed.set()
        sweep.join(5.0)
        insert_thread.join(5.0)
        assert inserted.is_set(), "the replacement insert never completed"
    finally:
        proceed.set()
        sweep.join(5.0)

    entry = idx.lookup_sop(uid)
    assert entry is not None, "cleanup deleted the replacement it raced with"
    assert entry["study_uid"] == "1.2.9", "the fresh entry must win"


# ---------------------------------------------------------------------------
# F6 - an inline comment on a UID line must not leak into the previous UID
# ---------------------------------------------------------------------------
def test_an_inline_comment_on_a_uid_line_does_not_reopen_the_previous_class(tmp_path):
    from clean_code.registry import _is_sop_accepted, _parse_simple_yaml

    path = tmp_path / "sop_classes.yaml"
    path.write_text(
        "sop_classes:\n"
        "  1.2.840.10008.5.1.4.1.1.2:\n"
        "    accept: false\n"
        "  1.2.840.10008.5.1.4.1.1.4:  # MR\n"
        "    accept: true\n",
        encoding="utf-8",
    )
    registry = _parse_simple_yaml(str(path))["sop_classes"]
    assert _is_sop_accepted(registry, "1.2.840.10008.5.1.4.1.1.2") is False, (
        "the MR class's `accept: true` must not land on the previous UID"
    )
    assert _is_sop_accepted(registry, "1.2.840.10008.5.1.4.1.1.4") is True


# ---------------------------------------------------------------------------
# F7 - mark_verified must read+update+write as one locked step
# ---------------------------------------------------------------------------
def test_mark_verified_takes_the_guard_once(tmp_path, monkeypatch):
    import clean_code.sop_index as si

    idx = si.SopIndexManager(str(tmp_path / "sop_index"))
    uid = "1.2.840.10008.5.1.4.1.1.4.912"
    idx.insert_sop(uid, "1.2.3", "1.2.3.4")

    acquisitions = []
    real_guard_for = idx._guard_for

    def _counting_guard(path):
        real = real_guard_for(path)

        class _Guarded:
            def __enter__(self):
                acquisitions.append(1)
                real.acquire()
                return real

            def __exit__(self, *a):
                real.release()

        return _Guarded()

    monkeypatch.setattr(idx, "_guard_for", _counting_guard)

    idx.mark_verified(uid)
    assert len(acquisitions) == 1, (
        "mark_verified must hold ONE guard across read+update+write, "
        "not read-then-release-then-write"
    )


# ---------------------------------------------------------------------------
# F8 - a failed instance counts undelivered exactly once
# ---------------------------------------------------------------------------
def test_a_failed_instance_counts_undelivered_once(tmp_path, monkeypatch):
    import json

    import clean_code.metrics as m
    from clean_code.main import _outbox_worker
    from clean_code.outbox import OutboxQueue

    from tests.test_outbox import _make_dataset

    q = OutboxQueue(str(tmp_path / "outbox"), max_retries=2)
    uid = "1.2.840.10008.5.1.4.1.1.4.915"
    q.enqueue(_make_dataset(uid))

    # Park the item one retry from the limit so the next mark_failed parks it.
    meta_path = q._meta_path(q._pending, uid)
    with open(meta_path, encoding="utf-8") as f:
        meta = json.load(f)
    meta["retry_count"] = q.MAX_RETRIES - 1
    with open(meta_path, "w", encoding="utf-8") as f:
        json.dump(meta, f)

    real_mark_failed = q.mark_failed

    def _mark_failed_leaves_meta(u, error):
        real_mark_failed(u, error)
        # Simulate the sidecar-move failure path: a meta still in pending at
        # the retry limit is what the old unconditional second move keyed on.
        if u == uid:
            q._atomic_write(
                q._meta_path(q._pending, uid),
                json.dumps(
                    {
                        "created_at": time.time(),
                        "retry_count": q.MAX_RETRIES,
                        "next_retry_at": 0,
                        "last_error": "boom",
                    }
                ),
            )

    monkeypatch.setattr(q, "mark_failed", _mark_failed_leaves_meta)

    counter = m.outbox_undelivered_total
    before = counter._value.get()

    shutdown = threading.Event()

    def _fake_batch(payloads, clarus_url, http_timeout, client, tls=None):
        return ([(u, 0xC000, 500, "boom") for u, _, _ in payloads], None)

    import unittest.mock as _um

    with _um.patch("clean_code.main._shutdown_requested", shutdown), _um.patch(
        "clean_code.http_helpers.stow_batch_instances", side_effect=_fake_batch
    ):
        t = threading.Thread(
            target=_outbox_worker,
            args=(
                q,
                "http://127.0.0.1:8024/dicomweb",
                30,
                None,
                None,
                100,
                1024 * 1024,
                0.1,
            ),
            daemon=True,
        )
        t.start()
        deadline = time.time() + 10
        while not q._is_undelivered(uid) and time.time() < deadline:
            time.sleep(0.05)
        shutdown.set()
        t.join(timeout=10)

    assert q._is_undelivered(uid)
    assert counter._value.get() - before == 1, (
        "the undelivered counter must rise once, not twice"
    )


# ---------------------------------------------------------------------------
# F9 - pending_bytes walks the directory once per second, not per call
# ---------------------------------------------------------------------------
def test_pending_bytes_walks_once_within_the_ttl(tmp_path, monkeypatch):
    import clean_code.outbox as ob

    q = ob.OutboxQueue(str(tmp_path / "outbox"))
    blob = q._dcm_path(q._pending, "1.2.840.10008.5.1.4.1.1.4.913")
    with open(blob, "wb") as f:
        f.write(b"\x00" * 16)

    listdir_calls = [0]
    real_listdir = ob.os.listdir

    def _listdir(p):
        listdir_calls[0] += 1
        return real_listdir(p)

    monkeypatch.setattr(ob.os, "listdir", _listdir)

    assert q.pending_bytes() == 16
    assert q.pending_bytes() == 16
    assert listdir_calls[0] == 1, "the second read must be served by the TTL cache"


# ---------------------------------------------------------------------------
# F10 - the startup sweep recovers fresh in_progress items
# ---------------------------------------------------------------------------
def test_a_forced_sweep_recovers_a_fresh_in_progress_item(tmp_path):
    from clean_code.outbox import OutboxQueue

    from tests.test_outbox import _make_dataset

    q = OutboxQueue(str(tmp_path / "outbox"))
    uid = "1.2.840.10008.5.1.4.1.1.4.914"
    q.enqueue(_make_dataset(uid))
    claimed, _ds, _blob, _raw = q.dequeue()
    assert claimed == uid
    assert os.path.isfile(q._dcm_path(q._in_progress, uid))

    # A normal sweep must NOT steal a fresh item...
    recovered, _u = q.recover_stuck(force=False)
    assert recovered == 0
    assert os.path.isfile(q._dcm_path(q._in_progress, uid))

    # ...but the startup sweep must.
    recovered, _u = q.recover_stuck(force=True)
    assert recovered == 1
    assert os.path.isfile(q._dcm_path(q._pending, uid))
    assert not os.path.isfile(q._dcm_path(q._in_progress, uid))


# ---------------------------------------------------------------------------
# F11 - the resumed live row uses max, not sum, for total and failed
# ---------------------------------------------------------------------------
def test_the_resumed_live_row_does_not_sum_total_or_failed():
    from http.server import HTTPServer

    from clean_code.handlers.cmove import _stream_and_send_study

    from tests.test_spill import (
        CT,
        EVRLE,
        SOP,
        STUDY,
        _body,
        _FakeClarus,
        _final,
        _part_bytes,
        _status,
    )

    sop1, sop2, sop3 = SOP + ".1", SOP + ".2", SOP + ".3"
    _FakeClarus.body = (
        _part_bytes(_body(256 * 1024, sop_uid=sop1))
        + _part_bytes(_body(256 * 1024, sop_uid=sop2))
        + _part_bytes(_body(256 * 1024, sop_uid=sop3))
        + _final()
    )

    class _RecordingMoveJobs:
        def __init__(self):
            self.progress = []

        def update_job_progress(self, job_id, completed, failed, total):
            self.progress.append((completed, failed, total))

        def is_job_cancelled(self, job_id):
            return False

    recorder = _RecordingMoveJobs()

    class _Assoc:
        is_established = True

        def __init__(self):
            self._calls = 0

        def send_c_store(self, path):
            self._calls += 1
            return _status(0x0000 if self._calls == 1 else 0xA700)

    assoc = _Assoc()

    httpd = HTTPServer(("127.0.0.1", 0), _FakeClarus)
    port = httpd.server_address[1]
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    try:
        _stream_and_send_study(
            assoc=assoc,
            study_uid=STUDY,
            clarus_url=f"http://127.0.0.1:{port}/dicomweb",
            http_timeout=10,
            move_jobs=recorder,
            job_id=7,
            accepted_contexts_map={CT: EVRLE},
            server_ts_list=[EVRLE],
            sop_registry={CT: {"accept": True}},
            _skip_ids={sop1},
            _resumed=True,
            _prior_completed=5,
            _prior_failed=2,
            _prior_total=5,
        )
    finally:
        httpd.shutdown()
        httpd.server_close()

    assert recorder.progress, "no live progress was written"
    completed, failed, total = recorder.progress[-1]
    assert total == 5, f"total must be max(pass, prior), got {total}"
    assert failed == 2, f"failed must be max(pass, prior), got {failed}"
    assert completed == 6, completed


# ---------------------------------------------------------------------------
# F12 - outgoing associations bound the raw TCP connect
# ---------------------------------------------------------------------------
def _fake_ae_capture(monkeypatch, cm):
    made = []

    class _FakeAssoc:
        is_established = True
        accepted_contexts = []

        def release(self):
            pass

        def abort(self):
            pass

    class _FakeAE:
        def __init__(self, ae_title=""):
            self._timeouts = {}
            made.append(self)

        def __setattr__(self, name, value):
            if name != "_timeouts":
                self._timeouts[name] = value
            object.__setattr__(self, name, value)

        def add_requested_context(self, *a, **k):
            pass

        def associate(self, *a, **k):
            return _FakeAssoc()

    monkeypatch.setattr(cm, "AE", _FakeAE)
    return made


def test_the_move_association_bounds_the_tcp_connect(monkeypatch):
    import clean_code.handlers.cmove as cm

    made = _fake_ae_capture(monkeypatch, cm)
    cm._associate_for_move("DEST", "BRIDGE", {MR: {"accept": True}}, "10.0.0.1", 11112)
    assert made, "no AE was constructed"
    t = made[0]._timeouts
    for key in ("network_timeout", "acse_timeout", "dimse_timeout", "connection_timeout"):
        assert key in t, f"AE missing {key}"
    assert t["connection_timeout"] == t["acse_timeout"], (
        "the connect bound must match the ACSE timeout"
    )


def test_the_commit_fallback_bounds_the_tcp_connect(monkeypatch):
    import clean_code.handlers.cmove as cm

    made = _fake_ae_capture(monkeypatch, cm)
    cm._send_storage_commitment("DEST", "10.0.0.1", 11112, [(MR, "1.2.3")], "BRIDGE", assoc=None)
    assert made, "no AE was constructed for the fallback association"
    assert made[0]._timeouts.get("connection_timeout") == 5, (
        "the fallback connect must be bounded to 5s, symmetric with nevent.py"
    )


def test_the_forward_association_bounds_the_tcp_connect(monkeypatch):
    """F12 sibling: the forwarder's outgoing association has the same
    unbounded-connect defect as the C-MOVE one."""
    import clean_code.main as m

    made = _fake_ae_capture(monkeypatch, m)
    m._forward_open_assoc(
        "BRIDGE",
        "DEST",
        "10.0.0.1",
        104,
        [(MR, "1.2.840.10008.1.2.1")],
        30,
        30,
        15,
    )
    assert made, "no AE was constructed for the forward association"
    t = made[0]._timeouts
    assert "connection_timeout" in t, "the forward connect must be bounded"
    assert t["connection_timeout"] == t["acse_timeout"]


# ---------------------------------------------------------------------------
# F13 - the pick skips a busy peer and takes the next job
# ---------------------------------------------------------------------------
def test_the_pick_skips_a_busy_peer_and_takes_the_next_job(tmp_path):
    import clean_code.handlers.cmove as cm
    from clean_code.move_jobs import MoveJobManager

    from tests.test_0911_rabbit_batch1 import _TestDb

    mgr = MoveJobManager(shared_db=_TestDb(str(tmp_path / "jobs.db")), max_workers=2)
    mgr.create_job("1.2.3.1", "PEER_X", "10.0.0.9", 104)
    j2 = mgr.create_job("1.2.3.2", "PEER_Y", "10.0.0.10", 104)

    entered = threading.Event()
    release = threading.Event()

    def _holder():
        cm._job_gate_enter("10.0.0.9", 104, "PEER_X")
        entered.set()
        release.wait(5.0)
        cm._job_gate_exit("10.0.0.9", 104, "PEER_X")

    holder = threading.Thread(target=_holder, daemon=True)
    holder.start()
    try:
        assert entered.wait(5.0)
        job = mgr.pick_pending_job(
            timeout=0.5,
            skip=lambda h, p, ae: bool(h)
            and bool(p)
            and cm._peer_is_busy(h, int(p), ae),
        )
        assert job is not None
        assert job["id"] == j2, "the busy peer's job must be skipped, not parked"
        assert job["move_dest_ae"] == "PEER_Y"
    finally:
        release.set()
        holder.join(5.0)


# ---------------------------------------------------------------------------
# F14 - a mid-stream raise still names the failed SOPs in the final response
# ---------------------------------------------------------------------------
def test_a_midstream_raise_still_names_the_failed_sops(monkeypatch):
    import clean_code.handlers.cmove as cm

    from tests.test_0911_rabbit_batch1 import _noop_counter, _stub_move_jobs

    sop_all = ["1.2.3.4.5.6.7.8.9.10.1", "1.2.3.4.5.6.7.8.9.10.2"]

    def fake_associate(*a, **k):
        return type("A", (), {"is_established": True, "accepted_contexts": []})(), {}

    def fake_preflight(*a, **k):
        return {}

    def fake_sops(study, url, *a, **k):
        return list(sop_all)

    def fake_caps(*a, **k):
        return []

    def fake_stream(*a, **k):
        raise ValueError("part without Content-Length")

    ops = {}
    stub = _stub_move_jobs(ops)
    monkeypatch.setattr(cm, "_associate_for_move", fake_associate)
    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", fake_preflight)
    monkeypatch.setattr(cm, "_study_sop_uids", fake_sops)
    monkeypatch.setattr(cm, "_get_clarus_transfer_syntaxes", fake_caps)
    monkeypatch.setattr(cm, "_stream_and_send_study", fake_stream)
    monkeypatch.setattr(cm, "move_jobs_counter", _noop_counter())

    cm._execute_cmove_job(
        101,
        "2.25.111122223333",
        "SCP",
        stub,
        "http://server/dicomweb",
        5,
        "BRIDGE",
        target_host="127.0.0.1",
        target_port=11112,
        sop_registry={MR: {"accept": True}},
    )

    assert ops.get("failed_sops") == sop_all, (
        "the final (0008,0058) must name the undelivered instances"
    )
    assert ops["finished"][1] is False, "a study with losses must not finish as success"
