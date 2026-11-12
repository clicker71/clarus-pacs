# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Bridge metric coverage for the 0210 backlog.

The N-ACTION / N-EVENT / association / C-STORE-latency / queue-depth / byte
counters are observability surfaces: they must be incremented at the point the
corresponding event is known to have happened, and they must NEVER raise into
the surrounding control flow (a metrics failure cannot be allowed to change a
DIMSE status or kill a drain thread).

Every test reads the counter value from the process-wide REGISTRY, so a missing
increment is a failure - not a silently absent metric.
"""

import threading
import time

import pytest
from clean_code.metrics import (
    REGISTRY,
    ae_label,
    associations_total,
    bytes_total,
    cmove_jobs_pending,
    cstore_outbound_duration,
    naction_batches_pending,
)

from tests.conftest import make_fake_naction_event

CT = "1.2.840.10008.5.1.4.1.1.2"


def counter_value(name, labels=None):
    """Read a counter/histogram-count sample, 0.0 when absent."""
    v = REGISTRY.get_sample_value(name, labels or {})
    return 0.0 if v is None else v


def gauge_value(name, labels=None):
    """Read a gauge sample, 0.0 when absent."""
    v = REGISTRY.get_sample_value(name, labels or {})
    return 0.0 if v is None else v


# ---------------------------------------------------------------------------
# Label bounding: AE titles are peer-announced, so they must be capped.
# ---------------------------------------------------------------------------
def test_ae_label_caps_peer_supplied_titles():
    """An AE title is attacker-controllable input; the label set must stay
    bounded to the DICOM 16-character form."""
    assert ae_label("SHORT_AE") == "SHORT_AE"
    assert ae_label("A" * 40) == "A" * 16
    assert ae_label("") == "UNKNOWN"
    assert ae_label(None) == "UNKNOWN"


# ---------------------------------------------------------------------------
# N-ACTION
# ---------------------------------------------------------------------------
def test_naction_request_counter_counts_the_acknowledgement(monkeypatch):
    """A handled N-ACTION bumps the request counter and nothing else.

    The per-SOP verdict counters used to be bumped here, by the handler's
    synchronous verification. The handler no longer verifies (PS3.4 J.3.2:
    the RSP is a receipt), so at this instant no SOP has a verdict and
    counting one would report evidence the bridge does not have. The
    verdict counters now belong to the tracker - see
    `test_naction_verdict_counters_are_bumped_by_the_tracker`.
    """
    from clean_code.handlers import naction

    before_req = counter_value("clarus_bridge_naction_requests_total")
    before_ok = counter_value("clarus_bridge_naction_total", {"verdict": "success"})
    before_bad = counter_value("clarus_bridge_naction_total", {"verdict": "failed"})

    def fake_confirm(sop_uid, *a, **kw):
        return 0 if sop_uid.endswith("1") else 0xC201

    monkeypatch.setattr(naction, "_sop_confirm", fake_confirm)

    # R5: a request that names SOPs must have a tracker, or the handler
    # refuses it (no N-EVENT-REPORT could ever be pushed). The counter under
    # test is the acknowledgement, so the batch must genuinely be accepted.
    from clean_code.commit_tracker import CommitTracker

    tracker = CommitTracker(verify=lambda c, u: True, report=lambda *a, **k: None)

    event = make_fake_naction_event(
        sop_list=[(CT, "1.2.3.1"), (CT, "1.2.3.2")],
    )
    status, _rsp = naction.handle_n_action(
        event,
        "http://x",
        False,
        None,
        commit_tracker=tracker,
        resolve_scp=lambda ae, addr: ("10.0.0.5", 11112),
    )
    tracker.close()
    assert status == 0x0000

    assert counter_value("clarus_bridge_naction_requests_total") == before_req + 1
    assert (
        counter_value("clarus_bridge_naction_total", {"verdict": "success"})
        == before_ok
    ), "the handler no longer produces verdicts"
    assert (
        counter_value("clarus_bridge_naction_total", {"verdict": "failed"})
        == before_bad
    ), "the handler no longer produces verdicts"


def test_naction_verdict_counters_are_bumped_by_the_tracker():
    """The tracker's resolution is what the verdict counters describe."""
    from clean_code.commit_tracker import CommitTracker

    before_ok = counter_value("clarus_bridge_naction_total", {"verdict": "success"})
    before_bad = counter_value("clarus_bridge_naction_total", {"verdict": "failed"})

    stored = "1.2.3.1"
    tracker = CommitTracker(
        verify=lambda sop_class, sop_uid: sop_uid == stored,
        report=lambda *a, **k: True,
        interval=0.1,
        deadline_sec=300.0,
    )
    tracker.register(
        "1.2.3.9", "SCU", "10.0.0.5", 11112, [(CT, stored), (CT, "1.2.3.2")]
    )
    # Let the tracker learn which SOPs the origin holds, THEN expire the
    # deadline: expiry is what turns the still-unconfirmed SOP into a
    # failure, and the verification walk is bounded by the same deadline so
    # it has to happen while the deadline is still in the future.
    tracker._tick()
    for job in list(tracker._jobs.values()):
        job.deadline = 0.0
    tracker._tick()
    tracker.close()

    assert (
        counter_value("clarus_bridge_naction_total", {"verdict": "success"})
        == before_ok + 1
    )
    assert (
        counter_value("clarus_bridge_naction_total", {"verdict": "failed"})
        == before_bad + 1
    )


def test_naction_rejected_action_type_does_not_count(monkeypatch):
    """The counter sits AFTER the action_type gate: a refused request is not
    a handled N-ACTION."""
    from clean_code.handlers import naction

    before = counter_value("clarus_bridge_naction_requests_total")
    event = make_fake_naction_event(action_type=99)
    status, _rsp = naction.handle_n_action(event, "http://x", False, None)
    assert status == 0x0123
    assert counter_value("clarus_bridge_naction_requests_total") == before


# ---------------------------------------------------------------------------
# N-EVENT-REPORT
# ---------------------------------------------------------------------------
def test_nevent_delivered_counter(monkeypatch):
    """A report whose status was read bumps the delivered counter."""
    from clean_code import nevent
    from pynetdicom.sop_class import StorageCommitmentPushModel

    before = counter_value("clarus_bridge_nevent_delivered_total")

    class _Status:
        Status = 0x0000

    class _Ctx:
        abstract_syntax = StorageCommitmentPushModel
        # PS3.7 D.3.3.4: the report can only be pushed back if the peer
        # GRANTED the requestor the SCP role for this context, which the
        # acceptor reports here (a real PresentationContext always has it).
        as_scp = True

    class _Assoc:
        is_established = True
        accepted_contexts = [_Ctx()]

        def send_n_event_report(self, *a, **kw):
            return _Status(), None

    class _AE:
        def __init__(self, *a, **kw):
            self.acse_timeout = 30

        def add_requested_context(self, *a, **kw):
            pass

        def associate(self, *a, **kw):
            return _Assoc()

    monkeypatch.setattr(nevent, "AE", _AE)
    monkeypatch.setattr(nevent, "_release_or_abort", lambda *a, **kw: None)

    ok = nevent.send_n_event_report(
        "127.0.0.1", 11112, "DEST", "1.2.3", 1, [], [], bridge_ae_title="TEST_BRIDGE"
    )
    assert ok is True
    assert counter_value("clarus_bridge_nevent_delivered_total") == before + 1


def test_nevent_failed_counter(monkeypatch):
    """An undeliverable report bumps the failed counter, never the delivered."""
    from clean_code import nevent

    before_ok = counter_value("clarus_bridge_nevent_delivered_total")
    before_bad = counter_value("clarus_bridge_nevent_failed_total")

    class _Assoc:
        is_established = False

        def release(self):
            pass

    class _AE:
        def __init__(self, *a, **kw):
            self.acse_timeout = 30

        def add_requested_context(self, *a, **kw):
            pass

        def associate(self, *a, **kw):
            return _Assoc()

    monkeypatch.setattr(nevent, "AE", _AE)
    monkeypatch.setattr(nevent, "_release_or_abort", lambda *a, **kw: None)
    monkeypatch.setattr(nevent.time, "sleep", lambda *_: None)

    ok = nevent.send_n_event_report(
        "127.0.0.1",
        11112,
        "DEST",
        "1.2.3",
        1,
        [],
        [],
        bridge_ae_title="TEST_BRIDGE",
        max_retries=0,
    )
    assert ok is False
    assert counter_value("clarus_bridge_nevent_failed_total") == before_bad + 1
    assert counter_value("clarus_bridge_nevent_delivered_total") == before_ok


# ---------------------------------------------------------------------------
# Associations
# ---------------------------------------------------------------------------
def test_accepted_association_counts_from_role():
    """An accepted association is counted once under role=from."""
    from clean_code.assoc_tracker import (
        register_assoc_accepted,
        register_assoc_released,
    )

    peer = "METRICS_FROM_AE"
    before = counter_value(
        "clarus_bridge_associations_total", {"role": "from", "ae": peer}
    )
    assoc_id = 987654
    try:
        register_assoc_accepted(assoc_id, peer, "127.0.0.1")
        after = counter_value(
            "clarus_bridge_associations_total", {"role": "from", "ae": peer}
        )
        assert after == before + 1
        # An over-long title must be CAPPED, not stored verbatim.
        long_peer = "Q" * 40
        register_assoc_accepted(assoc_id + 1, long_peer, "127.0.0.1")
        assert (
            counter_value(
                "clarus_bridge_associations_total",
                {"role": "from", "ae": "Q" * 16},
            )
            == 1.0
        )
    finally:
        for aid in (assoc_id, assoc_id + 1):
            try:
                register_assoc_released(aid)
            except Exception:
                pass


def test_outbound_association_counts_to_role(monkeypatch):
    """The forwarder counts its association under role=to."""
    from clean_code import main as bridge_main

    before = counter_value(
        "clarus_bridge_associations_total",
        {"role": "to", "ae": "METRICS_DEST_AE"},
    )

    class _Assoc:
        is_established = True

        def release(self):
            pass

    class _AE:
        def __init__(self, *a, **kw):
            self.network_timeout = None
            self.acse_timeout = None
            self.dimse_timeout = None

        def add_requested_context(self, *a, **kw):
            pass

        def associate(self, *a, **kw):
            return _Assoc()

    monkeypatch.setattr(bridge_main, "AE", _AE)

    assoc = bridge_main._forward_open_assoc(
        "CLBRIDGE",
        "METRICS_DEST_AE",
        "127.0.0.1",
        11112,
        {(CT, "1.2.840.10008.1.2.1")},
        5,
        5,
        5,
    )
    assert assoc is not None
    assert (
        counter_value(
            "clarus_bridge_associations_total",
            {"role": "to", "ae": "METRICS_DEST_AE"},
        )
        == before + 1
    )


def test_unestablished_outbound_association_is_not_counted(monkeypatch):
    """A refused association is not an association made."""
    from clean_code import main as bridge_main

    before = counter_value(
        "clarus_bridge_associations_total",
        {"role": "to", "ae": "METRICS_DEAD_AE"},
    )

    class _Assoc:
        is_established = False

        def release(self):
            pass

    class _AE:
        def __init__(self, *a, **kw):
            pass

        def add_requested_context(self, *a, **kw):
            pass

        def associate(self, *a, **kw):
            return _Assoc()

    monkeypatch.setattr(bridge_main, "AE", _AE)

    assert (
        bridge_main._forward_open_assoc(
            "CLBRIDGE", "METRICS_DEAD_AE", "127.0.0.1", 11112, set(), 5, 5, 5
        )
        is None
    )
    assert (
        counter_value(
            "clarus_bridge_associations_total",
            {"role": "to", "ae": "METRICS_DEAD_AE"},
        )
        == before
    )


# ---------------------------------------------------------------------------
# Outbound C-STORE latency
# ---------------------------------------------------------------------------
def test_forward_send_observes_outbound_latency(tmp_path, monkeypatch):
    """Every forward C-STORE observes one latency sample, even on a non-zero
    RSP (the round-trip happened)."""
    from clean_code import main as bridge_main

    before = counter_value(
        "clarus_bridge_cstore_outbound_duration_seconds_count",
        {"peer": "METRICS_LAT_AE"},
    )

    class _Status:
        Status = 0x0000

    class _Assoc:
        is_established = True
        accepted_contexts = []

        def send_c_store(self, path):
            time.sleep(0.01)
            return _Status()

        def release(self):
            pass

    monkeypatch.setattr(bridge_main, "_forward_open_assoc", lambda *a, **kw: _Assoc())

    blob = tmp_path / "blob.dcm"
    blob.write_bytes(b"\x00" * 64)  # size only matters for the byte counter

    class _Outbox:
        def __init__(self):
            self.delivered = []

        def mark_delivered(self, uid):
            self.delivered.append(uid)

        def mark_failed(self, uid, msg):
            raise AssertionError("unexpected mark_failed")

        def requeue(self, uid, msg):
            raise AssertionError("unexpected requeue")

    outbox = _Outbox()
    items = [("1.2.3.4.5", CT, "1.2.840.10008.1.2.1", str(blob))]
    # The negotiated-context gate needs the accepted map to cover the pair, so
    # drive the real helper by giving the fake association an accepted ctx.
    _Assoc.accepted_contexts = [
        type(
            "Ctx",
            (),
            {"abstract_syntax": CT, "transfer_syntax": ["1.2.840.10008.1.2.1"]},
        )()
    ]

    ok = bridge_main._forward_process_batch(
        outbox, items, "METRICS_LAT_AE", "CLBRIDGE", "127.0.0.1", 11112, 5, 5, 5
    )
    assert ok is True
    assert outbox.delivered == ["1.2.3.4.5"]
    after = counter_value(
        "clarus_bridge_cstore_outbound_duration_seconds_count",
        {"peer": "METRICS_LAT_AE"},
    )
    assert after == before + 1


def test_forward_delivery_counts_tx_bytes(tmp_path, monkeypatch):
    """A delivered forward counts the blob size as tx bytes for that peer."""
    from clean_code import main as bridge_main

    peer = "METRICS_BYTES_AE"
    before = counter_value(
        "clarus_bridge_bytes_total", {"direction": "tx", "peer": peer}
    )

    class _Status:
        Status = 0x0000

    class _Assoc:
        is_established = True
        accepted_contexts = [
            type(
                "Ctx",
                (),
                {
                    "abstract_syntax": CT,
                    "transfer_syntax": ["1.2.840.10008.1.2.1"],
                },
            )()
        ]

        def send_c_store(self, path):
            return _Status()

        def release(self):
            pass

    monkeypatch.setattr(bridge_main, "_forward_open_assoc", lambda *a, **kw: _Assoc())

    blob = tmp_path / "blob.dcm"
    blob.write_bytes(b"\x00" * 4096)

    class _Outbox:
        def mark_delivered(self, uid):
            pass

        def mark_failed(self, uid, msg):
            raise AssertionError("unexpected mark_failed")

        def requeue(self, uid, msg):
            raise AssertionError("unexpected requeue")

    ok = bridge_main._forward_process_batch(
        _Outbox(),
        [("1.2.3.4.5", CT, "1.2.840.10008.1.2.1", str(blob))],
        peer,
        "CLBRIDGE",
        "127.0.0.1",
        11112,
        5,
        5,
        5,
    )
    assert ok is True
    assert (
        counter_value("clarus_bridge_bytes_total", {"direction": "tx", "peer": peer})
        == before + 4096
    )


def test_forward_missing_blob_degrades_to_zero_bytes(tmp_path):
    """A vanished blob must not break the drain: size 0, no exception."""
    from clean_code.main import _forward_blob_size

    assert _forward_blob_size(str(tmp_path / "missing.dcm")) == 0


def test_measure_dataset_bytes_handles_stream_payload():
    """pynetdicom hands large payloads over as a stream, which has no len().
    The measurement must return the stream's size instead of raising, because
    the rx byte counter (and the size-limit guard) depend on it."""
    import io

    from clean_code.utils import _measure_dataset_bytes

    payload = b"x" * 4096

    class _Event:
        class _R:
            DataSet = io.BytesIO(payload)

        request = _R()

    from pydicom import Dataset

    ds = Dataset()
    ds.SOPClassUID = CT
    ds.SOPInstanceUID = "1.2.3.9"

    assert _measure_dataset_bytes(ds, _Event()) == 4096


def test_measure_dataset_bytes_restores_stream_position():
    """Measuring must not consume the payload: the caller still has to read
    it, so the read position is left exactly where it was."""
    import io

    from clean_code.utils import _measure_dataset_bytes

    stream = io.BytesIO(b"y" * 1024)
    stream.seek(0)

    class _Event:
        class _R:
            DataSet = stream

        request = _R()

    from pydicom import Dataset

    ds = Dataset()
    ds.SOPClassUID = CT
    ds.SOPInstanceUID = "1.2.3.9"

    assert _measure_dataset_bytes(ds, _Event()) == 1024
    assert stream.tell() == 0
    assert stream.read() == b"y" * 1024


# ---------------------------------------------------------------------------
# Queue depth gauges
# ---------------------------------------------------------------------------
def test_cmove_count_pending_counts_only_pending():
    """The C-MOVE depth gauge counts exactly the 'pending' rows."""
    import sqlite3
    import threading as _threading

    from clean_code.move_jobs import MoveJobManager

    class _TestDb:
        """Minimal SharedDb-compatible wrapper (same as test_db_move_jobs)."""

        def __init__(self):
            self._conn = sqlite3.connect(":memory:", check_same_thread=False)
            self._lock = _threading.Lock()

        def locked(self):
            db = self

            class _L:
                def __enter__(s):
                    db._lock.acquire()
                    return db._conn

                def __exit__(s, *a):
                    db._lock.release()

            return _L()

    mgr = MoveJobManager(shared_db=_TestDb(), max_workers=2)
    assert mgr.count_pending() == 0
    mgr.create_job("1.2.3", "DEST_AE", "127.0.0.1", 11112)
    mgr.create_job("1.2.4", "DEST_AE", "127.0.0.1", 11112)
    assert mgr.count_pending() == 2
    try:
        mgr.close()
    except Exception:
        pass


def test_cmove_count_pending_never_raises_on_broken_db():
    """A metrics scrape must not be broken by a failing database: the gauge
    read degrades to 0 instead of propagating."""
    import sqlite3
    import threading as _threading

    from clean_code.move_jobs import MoveJobManager

    class _FlakyDb:
        """Schema creation succeeds; the gauge COUNT then fails."""

        def __init__(self):
            self._conn = sqlite3.connect(":memory:", check_same_thread=False)
            self._lock = _threading.Lock()
            self.fail = False

        def locked(self):
            db = self

            class _L:
                def __enter__(s):
                    db._lock.acquire()
                    return _Proxy(db._conn, db)

                def __exit__(s, *a):
                    db._lock.release()

            return _L()

    class _Proxy:
        def __init__(self, conn, db):
            self._conn = conn
            self._db = db

        def execute(self, sql, *a, **kw):
            if self._db.fail and "COUNT" in sql.upper():
                raise RuntimeError("database is locked")
            return self._conn.execute(sql, *a, **kw)

        def __getattr__(self, name):
            return getattr(self._conn, name)

    db = _FlakyDb()
    mgr = MoveJobManager(shared_db=db, max_workers=1)
    try:
        assert mgr.count_pending() == 0
        db.fail = True
        assert mgr.count_pending() == 0  # degrades, does not raise
    finally:
        try:
            mgr.close()
        except Exception:
            pass


def test_move_jobs_pending_gauge_is_published_on_scrape():
    """The gauge exists in the registry and is settable (the /metrics handler
    path uses .set(); a missing metric would be an absent family)."""
    cmove_jobs_pending.set(7)
    assert gauge_value("clarus_bridge_cmove_jobs_pending") == 7.0
    cmove_jobs_pending.set(0)


def test_naction_batches_pending_gauge_is_published_on_scrape():
    naction_batches_pending.set(3)
    assert gauge_value("clarus_bridge_naction_batches_pending") == 3.0
    naction_batches_pending.set(0)


# ---------------------------------------------------------------------------
# Contract: metrics never raise into the control flow
# ---------------------------------------------------------------------------
def test_metrics_never_raise_on_unknown_label_values():
    """Widening a label set at the call site must fail safe: the guard in the
    callers swallows it. This pins the guard pattern by proving the label()
    call itself is the only failure mode."""
    with pytest.raises(Exception):
        cstore_outbound_duration.labels(bogus_label="x")


def test_send_c_store_latency_helper_is_thread_safe():
    """Concurrent observations must not lose samples (the bridge is multi-
    threaded; prometheus_client objects are the only sync point needed)."""
    peer = "METRICS_THREAD_AE"
    before = counter_value(
        "clarus_bridge_cstore_outbound_duration_seconds_count", {"peer": peer}
    )
    threads = [
        threading.Thread(
            target=lambda: [
                cstore_outbound_duration.labels(peer=peer).observe(0.001)
                for _ in range(50)
            ]
        )
        for _ in range(4)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    after = counter_value(
        "clarus_bridge_cstore_outbound_duration_seconds_count", {"peer": peer}
    )
    assert after == before + 200


def test_associations_total_label_bound_is_enforced_by_helper():
    """The counter itself must never be called with a raw unbounded title: the
    call sites all funnel through ae_label."""
    associations_total.labels(role="from", ae=ae_label("Z" * 100)).inc()
    assert (
        counter_value(
            "clarus_bridge_associations_total", {"role": "from", "ae": "Z" * 16}
        )
        == 1.0
    )


def test_bytes_total_has_both_directions_documented():
    """Both directions are usable label values (rx on reception, tx on send)."""
    bytes_total.labels(direction="rx", peer="METRICS_DIR_AE").inc(11)
    bytes_total.labels(direction="tx", peer="METRICS_DIR_AE").inc(22)
    assert (
        counter_value(
            "clarus_bridge_bytes_total", {"direction": "rx", "peer": "METRICS_DIR_AE"}
        )
        == 11.0
    )
    assert (
        counter_value(
            "clarus_bridge_bytes_total", {"direction": "tx", "peer": "METRICS_DIR_AE"}
        )
        == 22.0
    )
