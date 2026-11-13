# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false

"""1211 convergence batch 6 regression tests, one per finding.

Each test fails when its fix is reverted: the assertion targets the behaviour
the finding described, not the shape of the code.
"""

import inspect
import json
import logging
import threading

import pytest


# ---------------------------------------------------------------------------
# R1 - the media type is compared as an essence, not searched as a substring
# ---------------------------------------------------------------------------
class _FakeBody:
    def __init__(self, raw):
        self._raw = raw

    def read(self, n=-1):
        return self._raw


def _post_print(content_type):
    """Run the real do_POST with the transport stubbed out."""
    from clean_code import http_server as hs

    handler = object.__new__(hs.BridgeHTTPHandler)
    handler.headers = {"Content-Type": content_type, "Content-Length": "0"}
    handler.path = "/print"
    handler.errors = []
    handler.command = "POST"
    handler.request_version = "HTTP/1.1"
    handler.wfile = _FakeBody(b"")
    handler.rfile = _FakeBody(b"")
    handler.send_error = lambda code, msg=None: handler.errors.append(("error", code))
    handler._json_error = lambda code, msg=None: handler.errors.append(("json", code))
    handler.log_message = lambda *a, **k: None
    handler.do_POST()
    return handler


def test_a_smuggled_media_type_is_refused():
    """`"application/json" in "text/plain; x=application/json"` is True, and
    text/plain is CORS-safelisted: a cross-origin page could post a print job
    with no preflight at all."""
    handler = _post_print("text/plain; x=application/json")
    assert ("error", 400) in handler.errors


def test_the_essence_with_parameters_is_accepted():
    handler = _post_print("application/json; charset=utf-8")
    assert ("error", 400) not in handler.errors


# ---------------------------------------------------------------------------
# R2 - the request line and headers share one wall-clock budget
# ---------------------------------------------------------------------------
def test_the_request_head_is_read_under_a_total_deadline():
    """`self.timeout` bounds one socket operation and every received byte
    resets it, so a header trickled every 29 s held the single-threaded
    listener - and /health behind it - indefinitely. F1 sharpened this: the
    deadline is read ONCE and re-arms the socket to whatever is LEFT of it on
    every read, travelling through `_HeadBudgetReader`."""
    from clean_code import http_server as hs

    src = inspect.getsource(hs.BridgeHTTPHandler._read_head_with_deadline)
    assert (
        "time.monotonic() + HEAD_TOTAL_TIMEOUT_SEC" in src
    ), "the head must be bounded by one wall-clock budget"
    assert (
        "_HeadBudgetReader" in src
    ), "the budget must travel with the reader so every header line re-arms it"
    # The deadline must be computed before the request line is read.
    assert src.index("time.monotonic() + HEAD_TOTAL_TIMEOUT_SEC") < src.index(
        "readline"
    ), "the deadline must be set before the request line is read"

    one_request = inspect.getsource(hs.BridgeHTTPHandler.handle_one_request)
    assert (
        "_read_head_with_deadline" in one_request
    ), "handle_one_request must route the head through the budget"


def test_a_header_timeout_answers_408():
    from clean_code import http_server as hs

    one_request = inspect.getsource(hs.BridgeHTTPHandler.handle_one_request)
    assert (
        "send_error(408" in one_request
    ), "a head timeout must answer 408, not drop the connection"
    assert hs.HEAD_TOTAL_TIMEOUT_SEC <= hs.HTTP_SOCKET_TIMEOUT_SEC


# ---------------------------------------------------------------------------
# R3 - an open batch is flushed by AGE
# ---------------------------------------------------------------------------
def test_an_open_batch_is_flushed_by_age():
    """The idle flush only fires on a dequeue GAP, so a source trickling one
    instance per `batch_idle_sec - 1` s kept the batch open past
    STUCK_ITEM_TIMEOUT. The reaper then returned the oldest claimed item to
    pending, and the next dequeue sent it a second time."""
    import clean_code.main as m
    from clean_code.outbox import STUCK_ITEM_TIMEOUT

    src = inspect.getsource(m._outbox_worker)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())

    assert (
        "_batch_started_at = time.monotonic()" in code
    ), "the batch must remember when its first item was buffered"
    assert "_batch_max_age_sec" in code, "the age trigger must be declared"
    assert (
        "time.monotonic() - _batch_started_at >= _batch_max_age_sec" in code
    ), "the flush condition must include the age"

    limit = min(60.0, STUCK_ITEM_TIMEOUT / 4)
    assert (
        limit < STUCK_ITEM_TIMEOUT
    ), "the cap must fire well before the reaper can reclaim an item"


# ---------------------------------------------------------------------------
# R4 - one unserializable SOP does not take the whole batch with it
# ---------------------------------------------------------------------------
def test_a_serialization_failure_is_per_sop(monkeypatch):
    """`dcmwrite` sat OUTSIDE the per-SOP guard, so one instance with an
    invalid value for its VR raised before the function returned its
    `(results, retry_after)` tuple: the whole batch had no outcome."""
    import clean_code.http_helpers as hh

    real_dcmwrite = hh.dcmwrite

    class _Bad:
        SOPInstanceUID = "1.2.3.BAD"

        def __init__(self):
            self.file_meta = type(
                "M", (), {"TransferSyntaxUID": "1.2.840.10008.1.2.1"}
            )()

    def _write(fp, ds, **kwargs):
        if getattr(ds, "SOPInstanceUID", "") == "1.2.3.BAD":
            raise ValueError("invalid value for VR US")
        return real_dcmwrite(fp, ds, **kwargs)

    monkeypatch.setattr(hh, "dcmwrite", _write)
    # The transport layer must never be reached: this batch must fail before
    # a POST could even be attempted.
    monkeypatch.setattr(
        hh, "_http_retry", lambda fn: pytest.fail("POST attempted with no part")
    )

    payloads = [
        ("1.2.3.BAD", _Bad(), None),
    ]
    results, retry_after = hh.stow_batch_instances(payloads, "http://127.0.0.1:1")

    assert retry_after is None
    assert [(uid, status) for uid, status, _h, _m in results] == [
        ("1.2.3.BAD", 0xC000)
    ], "the caller must get a per-SOP verdict for the unwritable instance"


def test_a_serialization_failure_keeps_the_other_parts(monkeypatch):
    import clean_code.http_helpers as hh

    class _Good:
        SOPInstanceUID = "1.2.3.GOOD"

        def __init__(self):
            self.file_meta = type(
                "M", (), {"TransferSyntaxUID": "1.2.840.10008.1.2.1"}
            )()

    class _Bad:
        SOPInstanceUID = "1.2.3.BAD"

        def __init__(self):
            self.file_meta = type(
                "M", (), {"TransferSyntaxUID": "1.2.840.10008.1.2.1"}
            )()

    def _write(fp, ds, **kwargs):
        if getattr(ds, "SOPInstanceUID", "") == "1.2.3.BAD":
            raise ValueError("invalid value for VR US")
        fp.write(b"GOOD-BYTES")
        return None

    sent = {}

    class _Resp:
        status_code = 200
        text = "{}"

    def _retry(fn):
        sent["called"] = True
        return _Resp()

    monkeypatch.setattr(hh, "dcmwrite", _write)
    monkeypatch.setattr(hh, "_http_retry", _retry)

    results, _retry_after = hh.stow_batch_instances(
        [("1.2.3.BAD", _Bad(), None), ("1.2.3.GOOD", _Good(), None)],
        "http://127.0.0.1:1",
    )

    assert sent.get("called") is True, "the good part must still be posted"
    by_uid = {uid: status for uid, status, _h, _m in results}
    assert by_uid["1.2.3.BAD"] == 0xC000
    assert by_uid["1.2.3.GOOD"] == 0x0000
    assert len(results) == 2


def test_stow_single_instance_returns_a_verdict_on_a_serialization_error(
    monkeypatch,
):
    import clean_code.http_helpers as hh

    class _Bad:
        SOPInstanceUID = "1.2.3.BAD"

    def _write(*a, **k):
        raise ValueError("invalid value for VR US")

    monkeypatch.setattr(hh, "dcmwrite", _write)
    monkeypatch.setattr(hh, "_http_retry", lambda fn: pytest.fail("POST attempted"))
    status, _http, msg = hh.stow_single_instance(_Bad(), "http://127.0.0.1:1")
    assert status == 0xC000
    assert "Serialize" in msg


# ---------------------------------------------------------------------------
# R5 - IMAGE level is refused, not relabelled as study rows
# ---------------------------------------------------------------------------
@pytest.mark.parametrize("root", ["study", "patient"])
def test_an_image_level_query_is_refused(root):
    from clean_code.handlers import cfind

    from tests.conftest import make_fake_find_event

    event = make_fake_find_event()
    event.identifier.QueryRetrieveLevel = "IMAGE"
    fn = cfind._handle_study_root if root == "study" else cfind._handle_patient_root
    out = list(fn(event, "http://127.0.0.1:1/clarus/dicomweb", False, 100))
    assert out == [
        (0xA900, None)
    ], "IMAGE level has no study-row equivalent; it must be refused"


def test_an_unknown_level_is_refused_too():
    from clean_code.handlers import cfind

    from tests.conftest import make_fake_find_event

    event = make_fake_find_event()
    event.identifier.QueryRetrieveLevel = "FRAME"
    out = list(
        cfind._handle_study_root(
            event, "http://127.0.0.1:1/clarus/dicomweb", False, 100
        )
    )
    assert out == [(0xA900, None)]


# ---------------------------------------------------------------------------
# R6 - a malformed Failed SOP Sequence is transient, not permanent
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    "body",
    [
        {"00081198": {"Value": ["not-an-item"]}},
        {"00081198": {"Value": [{"00081197": "272"}]}},
        {"00081198": {"Value": [{"00081197": {"Value": "272"}}]}},
        {"00081198": {"Value": [{"00081155": {"Value": "not-a-list"}}]}},
        {"00081198": {"Value": "not-a-list"}},
    ],
)
def test_a_malformed_failed_sop_sequence_is_a_transient_body_error(body):
    """`item.get("00081197", {})` without type checks raised AttributeError,
    which the caller's wide `except Exception` mapped to 0xC000 - a PERMANENT
    refusal for a merely corrupt body."""
    import clean_code.http_helpers as hh

    with pytest.raises(hh.StowBodyNotJsonObject):
        hh._parse_stow_failures_all(json.dumps(body))


def test_a_well_formed_failed_sop_sequence_still_parses():
    import clean_code.http_helpers as hh

    body = {
        "00081198": {
            "Value": [
                {
                    "00081197": {"Value": [272]},
                    "00000902": {"Value": ["Boom"]},
                    "00081155": {"Value": ["1.2.3"]},
                }
            ]
        }
    }
    out = hh._parse_stow_failures_all(json.dumps(body))
    assert out == [{"sop_uid": "1.2.3", "reason": "[code=272] Boom"}]


def test_a_malformed_batch_response_maps_to_a700(monkeypatch):
    """The batch path must answer 0xA700 (retryable), never 0xC000."""
    import clean_code.http_helpers as hh

    class _Resp:
        status_code = 200
        text = json.dumps({"00081198": {"Value": "oops"}})

    monkeypatch.setattr(hh, "_http_retry", lambda fn: _Resp())
    results, _backoff = hh.stow_batch_instances(
        [("1.2.3", None, b"raw")], "http://127.0.0.1:1"
    )
    assert [status for _uid, status, _h, _m in results] == [0xA700]


# ---------------------------------------------------------------------------
# R7 - the byte budget is enforced by the backpressure ratio
# ---------------------------------------------------------------------------
def test_the_byte_budget_drives_backpressure():
    """`advanced.stow_queue_max_mb` was validated and then never read: 20000
    slots of 0.5-50 MB is up to 10 GB against a configured 500 MB."""
    from clean_code.handlers.cstore import _check_backpressure

    # 2% of the slots, 400% of the bytes: slots alone would admit.
    assert _check_backpressure(400, 20000) == 0x0000
    assert (
        _check_backpressure(
            400, 20000, pending_bytes=2 * 1024**3, max_queue_bytes=512 * 1024**2
        )
        == 0xC000
    )


def test_the_byte_budget_is_optional():
    from clean_code.handlers.cstore import _check_backpressure

    assert (
        _check_backpressure(400, 20000, pending_bytes=10**12, max_queue_bytes=0)
        == 0x0000
    )


def test_the_queue_measures_its_pending_bytes(tmp_path):
    from clean_code.outbox import OutboxQueue

    q = OutboxQueue(str(tmp_path), max_queue_bytes=1024)
    assert q.max_queue_bytes == 1024
    assert q.pending_bytes() == 0


# ---------------------------------------------------------------------------
# R8 - UID path segments are percent-encoded
# ---------------------------------------------------------------------------
def test_the_verification_url_quotes_every_uid(monkeypatch):
    """A crafted UID of `1.2?a=b` redirected the GET to another resource, and
    a 200 there confirmed a SOP the server never stored."""
    import clean_code.handlers.naction as na

    seen = {}

    class _Resp:
        status_code = 200

    def _wado(url, timeout, tls=None):
        seen["url"] = url
        return _Resp()

    class _Index:
        def lookup_sop(self, uid):
            return {"study_uid": "1.2?a=b", "series_uid": "../x", "sop_uid": "3.4#f"}

        def mark_verified(self, uid):
            pass

    monkeypatch.setattr(na, "_wado_metadata_request", _wado)
    reason = na._sop_confirm("3.4#f", _Index(), "http://h/clarus/dicomweb", 5)
    assert reason == 0
    assert "1.2?a=b" not in seen["url"]
    assert "../x" not in seen["url"]
    assert "3.4#f" not in seen["url"]
    assert "1.2%3Fa%3Db" in seen["url"]
    assert "..%2Fx" in seen["url"]
    assert "3.4%23f" in seen["url"]


# ---------------------------------------------------------------------------
# R9 - a gate-blocked job is not claimed by a worker
# ---------------------------------------------------------------------------
def test_a_job_for_a_busy_peer_is_handed_back(tmp_path):
    """The worker claimed a job BEFORE taking the gate, so with two workers a
    job for a peer already being streamed to waited invisible to the queue.

    Exercises the pick guard itself: the worker loop blocks on shutdown, so the
    decision is what is pinned here - a job whose peer is busy is released
    unretried, and one whose peer is free is kept.
    """
    import clean_code.handlers.cmove as cm
    from clean_code.move_jobs import MoveJobManager

    from tests.test_0911_rabbit_batch1 import _TestDb

    mgr = MoveJobManager(shared_db=_TestDb(str(tmp_path / "jobs.db")), max_workers=1)
    mgr.create_job("1.2.3", "PEER", "10.0.0.9", 104)

    # The peer is owned by ANOTHER worker (the gate is per thread).
    entered = threading.Event()
    release = threading.Event()

    def _other_worker():
        cm._job_gate_enter("10.0.0.9", 104, "PEER")
        entered.set()
        release.wait(5.0)
        cm._job_gate_exit("10.0.0.9", 104, "PEER")

    holder = threading.Thread(target=_other_worker, daemon=True)
    holder.start()
    try:
        assert entered.wait(5.0)
        assert cm._peer_is_busy("10.0.0.9", 104, "PEER") is True
        job = mgr.pick_pending_job(timeout=0.01)
        assert job is not None
        assert mgr.requeue_job_without_retry(job["id"]) is True
        back = mgr.pick_pending_job(timeout=0.01)
        assert back is not None, "the job must stay visible to the queue"
        assert back["attempts"] == job["attempts"], "no retry was charged"
    finally:
        release.set()
        holder.join(5.0)

    # With the peer free the guard lets the job through.
    assert cm._peer_is_busy("10.0.0.9", 104, "PEER") is False


def test_the_worker_loop_consults_the_peer_gate():
    """F13 moved the busy-peer guard INTO the pick: the worker passes a
    `skip` predicate to `pick_pending_job` that references the gate, so a job
    for a busy peer is never claimed in the first place."""
    import inspect

    import clean_code.handlers.cmove as cm

    src = inspect.getsource(cm._cmove_worker)
    # The pick must carry a skip predicate that consults the gate. A skip
    # that never references `_peer_is_busy` would claim a busy peer's job
    # again, which is the starvation this guard exists to prevent.
    assert "pick_pending_job(" in src
    assert "skip=" in src, "the pick must carry a skip predicate"
    assert "_peer_is_busy(" in src, "the skip predicate must consult the gate"
    # The predicate must be wired into the SAME pick call: everything from
    # `skip=` onward must mention the gate.
    pick_body = src.split("skip=", 1)[1]
    assert "_peer_is_busy(" in pick_body, "the gate must gate the pick itself"


def test_the_handback_does_not_burn_a_retry(tmp_path):
    from clean_code.move_jobs import MoveJobManager

    from tests.test_0911_rabbit_batch1 import _TestDb

    mgr = MoveJobManager(shared_db=_TestDb(str(tmp_path / "jobs.db")), max_workers=1)
    mgr.create_job("1.2.3", "PEER", "10.0.0.5", 11112)
    claimed = mgr.pick_pending_job(timeout=0.01)
    assert claimed is not None
    attempts = claimed["attempts"]

    assert mgr.requeue_job_without_retry(claimed["id"]) is True
    again = mgr.pick_pending_job(timeout=0.01)
    assert again is not None
    assert (
        again["attempts"] == attempts
    ), "a handback must not charge the job an attempt it never made"
    mgr.close() if hasattr(mgr, "close") else None


def test_a_busy_peer_is_reported_busy():
    import threading

    import clean_code.handlers.cmove as cm

    entered = threading.Event()
    release = threading.Event()

    def _holder():
        cm._job_gate_enter("10.0.0.9", 104, "PEER")
        entered.set()
        release.wait(2.0)
        cm._job_gate_exit("10.0.0.9", 104, "PEER")

    t = threading.Thread(target=_holder, daemon=True)
    t.start()
    try:
        assert entered.wait(2.0)
        assert cm._peer_is_busy("10.0.0.9", 104, "PEER") is True
        assert cm._peer_is_busy("10.0.0.9", 104, "OTHER") is False
    finally:
        release.set()
        t.join(2.0)
    assert cm._peer_is_busy("10.0.0.9", 104, "PEER") is False


# ---------------------------------------------------------------------------
# R10 - abandoned batches are bounded and outside the working set
# ---------------------------------------------------------------------------
def test_an_abandoned_batch_leaves_the_workingset_and_expires(tmp_path):
    from clean_code.commit_tracker import MAX_REPORT_ATTEMPTS

    from tests.test_commit_tracker_persistence import _Db, _tracker

    path = str(tmp_path / "c.db")
    tracker = _tracker(_Db(path))
    tracker.register("1.2.3.90", "MODALITY", "10.0.0.5", 104, _SOP_LIST)
    job = tracker._jobs["1.2.3.90"]
    job.deadline = 0.0
    job.report_attempts = MAX_REPORT_ATTEMPTS - 1
    tracker._resolve = lambda j: False
    tracker._tick()

    assert "1.2.3.90" not in tracker._jobs
    assert (
        tracker.pending_count() == 0
    ), "an abandoned batch must not be counted as pending forever"
    assert tracker.abandoned_count() == 1
    tracker.close()

    restarted = _tracker(_Db(path))
    try:
        assert "1.2.3.90" in restarted._abandoned
        assert (
            restarted.purge_abandoned(max_age_sec=3600.0) == 0
        ), "a fresh abandonment is not expired yet"
        assert restarted.purge_abandoned(max_age_sec=-1.0) == 1
    finally:
        restarted.close()

    # The row is gone for good, so a later restart cannot resurrect it.
    final = _tracker(_Db(path))
    try:
        assert "1.2.3.90" not in final._abandoned
    finally:
        final.close()


def test_the_purge_is_wired_into_the_hourly_cleanup():
    import clean_code.handlers.cmove as cm

    src = inspect.getsource(cm._cmove_cleanup_worker)
    assert (
        "purge_abandoned" in src
    ), "the retention sweep must ride the existing hourly cycle"


# ---------------------------------------------------------------------------
# R11 - a short key file is re-read before being replaced
# ---------------------------------------------------------------------------
def test_a_short_key_file_is_reread_before_replacement(tmp_path, monkeypatch):
    """Process B saw A's half-published file and published its OWN key over it,
    so from then on the two processes masked the same PatientID into
    different tokens."""
    from clean_code import utils

    key_path = tmp_path / ".patient_id_mask.key"
    key_path.write_bytes(b"")

    good = b"K" * utils._MASK_KEY_BYTES
    real_read = utils._read_key

    def _competitor_finishes(path):
        # The other process completes its publication while the repair is
        # re-reading: the re-read is exactly what lets us adopt it.
        key_path.write_bytes(good)
        return real_read(path)

    monkeypatch.setattr(utils, "_mask_key_dir_override", str(tmp_path))
    monkeypatch.setattr(utils, "_read_key", _competitor_finishes)
    got = utils._load_or_create_key()

    assert (
        got == good
    ), "the key that finished publishing must be adopted, not overwritten"
    assert key_path.read_bytes() == good


# ---------------------------------------------------------------------------
# R12 - pynetdicom suppression is per thread
# ---------------------------------------------------------------------------
def test_pynetdicom_silencing_is_per_thread():
    """Moving the logger LEVEL silenced every thread's records for the whole
    association (seconds, times the retries)."""
    import clean_code.nevent as nevent

    pn = logging.getLogger("pynetdicom")
    if nevent._pn_quiet_filter not in pn.filters:
        pn.addFilter(nevent._pn_quiet_filter)

    seen = []

    class _Cap(logging.Handler):
        def emit(self, record):
            seen.append((threading.get_ident(), record.getMessage()))

    handler = _Cap()
    pn.addHandler(handler)
    level = pn.level
    pn.setLevel(logging.DEBUG)
    try:
        nevent._PN_QUIET.on = True
        pn.info("suppressed")
        nevent._PN_QUIET.on = False
        pn.info("kept")
    finally:
        pn.removeHandler(handler)
        pn.setLevel(level)

    messages = [m for _t, m in seen]
    assert "suppressed" not in messages
    assert "kept" in messages


def test_the_reporter_does_not_move_the_logger_level():
    import clean_code.nevent as nevent

    src = inspect.getsource(nevent.send_n_event_report)
    assert (
        "setLevel" not in src
    ), "the process-wide pynetdicom level must not be touched"
    assert not hasattr(nevent, "_LEVEL_LOCK")


_SOP_LIST = [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.1")]
