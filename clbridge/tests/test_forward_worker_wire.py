# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0409 Stage 3 wire tests: _forward_worker DIMSE drain against a real SCP.

Architect spec (analysis/0409_architect_bridge_forwarder_REPORT.md Stage 3):
  - N instances -> all delivered, one C-STORE-RSP per instance, statuses counted;
  - SCP returns 0xC000 -> retry with backoff -> after max_retries -> undelivered
    dir + counter;
  - SCP unreachable -> worker pauses, no retry burn (primary keeps draining -
    two-queue isolation);
  - restart with files in forward/pending -> they drain (replay via
    recover_stuck semantics).
Invariant pinned: every C-STORE-RSP is read and accounted (no silent loss),
and the forward worker cannot stall the primary drain.

The FORWARD side is a real clean_code OutboxQueue; the SCP is a real
pynetdicom AE (pattern: test_bridge_ts_order.py). We drive the real
_forward_process_batch / _forward_worker from clean_code.main (import only
- cmove helpers are reused, never edited).
"""

import os
import socket
import threading
import time

from clean_code.main import (
    _forward_blob_pair,
    _forward_process_batch,
    _forward_worker,
)
from clean_code.outbox import OutboxQueue
from pydicom import Dataset
from pydicom.dataset import FileMetaDataset
from pydicom.uid import ExplicitVRLittleEndian as EVRLE
from pynetdicom import AE, evt

CT = "1.2.840.10008.5.1.4.1.1.2"  # CT Image Storage
MR = "1.2.840.10008.5.1.4.1.1.4"  # MR Image Storage (a different class)


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class _ForwardScp:
    """Real pynetdicom SCP that records received SOPs and returns a status.

    `respond_status` can be set to simulate delivery failures (e.g. 0xC000).
    Every handled C-STORE records (SOP instance UID, outgoing status).
    """

    def __init__(
        self, ae_title="FWD_SCP", sop_class=CT, ts_list=(EVRLE,), respond_status=0x0000
    ):
        self.received = []
        self.statuses = []
        self.respond_status = respond_status
        self.ae = AE(ae_title=ae_title)
        self.ae.add_supported_context(sop_class, list(ts_list))
        self.port = _free_port()

        def _on_store(event):
            uid = str(getattr(event.dataset, "SOPInstanceUID", "?"))
            out = self.respond_status
            self.received.append(uid)
            self.statuses.append(out)
            return out

        self.server = self.ae.start_server(
            ("127.0.0.1", self.port),
            block=False,
            evt_handlers=[(evt.EVT_C_STORE, _on_store)],
        )
        time.sleep(0.3)

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


def _make_ct(uid, sop_class=CT):
    """A minimal but valid DICOM dataset (EVRLE) for C-STORE."""
    ds = Dataset()
    ds.SOPClassUID = sop_class
    ds.SOPInstanceUID = uid
    ds.StudyInstanceUID = "1.2.3." + uid
    ds.SeriesInstanceUID = "1.2.3.4." + uid
    ds.PatientName = "FWD^Test"
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = sop_class
    ds.file_meta.MediaStorageSOPInstanceUID = uid
    ds.file_meta.TransferSyntaxUID = EVRLE
    return ds


def _seed_forward(tmp_path, uids, max_retries=10, undelivered_total=None):
    """Create a forward OutboxQueue with N real pending items."""
    q = OutboxQueue(
        outbox_dir=str(tmp_path / "fwd"),
        max_queue=100,
        max_retries=max_retries,
        undelivered_total=undelivered_total,
    )
    for u in uids:
        assert q.enqueue(_make_ct(u)) is True
    return q


def _claim(q, uids):
    """dequeue() each ready uid; return process-able item tuples."""
    items = []
    for _ in uids:
        res = q.dequeue()
        if res[0] is None:
            break
        uid, ds, blob_path, raw = res
        sop_class, stored_ts = _forward_blob_pair(raw)
        items.append((uid, sop_class, stored_ts, blob_path))
    return items


def _counter(name):
    """Delta-friendly read of a prometheus counter's internal value."""
    from clean_code import metrics as m

    obj = getattr(m, name, None)
    if obj is not None and hasattr(obj, "_value"):
        return obj._value.get()
    return 0


# ---------------------------------------------------------------------------
# 1. All delivered, one C-STORE-RSP per instance
# ---------------------------------------------------------------------------
def test_all_delivered_one_rsp_per_instance(tmp_path):
    scp = _ForwardScp(respond_status=0x0000)
    uids = ["1.2.840.113543.99.1", "1.2.840.113543.99.2", "1.2.840.113543.99.3"]
    q = _seed_forward(tmp_path, uids)
    try:
        items = _claim(q, uids)
        assert len(items) == 3
        before_del = _counter("forward_delivered_total")
        ok = _forward_process_batch(
            q,
            items,
            "FWD_SCP",
            "CLBRIDGE",
            "127.0.0.1",
            scp.port,
            network_timeout=5,
            dimse_timeout=5,
            acse_timeout=5,
        )
        assert ok is True
        assert scp.received == uids
        # One C-STORE-RSP read per instance, each 0x0000.
        assert scp.statuses == [0x0000, 0x0000, 0x0000]
        assert len(scp.statuses) == len(scp.received) == 3
        assert q.pending_count() == 0
        assert _counter("forward_delivered_total") == before_del + 3
    finally:
        scp.shutdown()


# ---------------------------------------------------------------------------
# 2. SCP returns 0xC000 -> after max_retries (1 here) -> undelivered + counter
# ---------------------------------------------------------------------------
def test_nonzero_rsp_retries_to_undelivered(tmp_path):
    from clean_code.metrics import forward_undelivered_total

    scp = _ForwardScp(respond_status=0xC000)
    # max_retries=1 keeps the test fast: one non-zero RSP burns the only retry
    # and OutboxQueue.mark_failed moves it to undelivered (semantics identical
    # for any budget; the production default is 10). undelivered_total is wired
    # to the forward counter exactly as main.py wires the forward OutboxQueue.
    q = _seed_forward(
        tmp_path,
        ["1.2.840.113543.99.10"],
        max_retries=1,
        undelivered_total=forward_undelivered_total,
    )
    try:
        items = _claim(q, ["1.2.840.113543.99.10"])
        assert len(items) == 1
        before_fail = _counter("forward_failed_total")
        before_und = _counter("forward_undelivered_total")
        ok = _forward_process_batch(
            q,
            items,
            "FWD_SCP",
            "CLBRIDGE",
            "127.0.0.1",
            scp.port,
            network_timeout=5,
            dimse_timeout=5,
            acse_timeout=5,
        )
        assert ok is True
        assert scp.received == ["1.2.840.113543.99.10"]
        assert scp.statuses == [0xC000]
        assert _counter("forward_failed_total") == before_fail + 1
        und = os.path.join(q._undelivered, q._safe_uid("1.2.840.113543.99.10") + ".dcm")
        assert os.path.isfile(und), "file must land in undelivered"
        assert _counter("forward_undelivered_total") == before_und + 1
        assert q.pending_count() == 0
    finally:
        scp.shutdown()


# ---------------------------------------------------------------------------
# 3. SCP unreachable -> pause, no retry burn; isolated from the primary queue
# ---------------------------------------------------------------------------
def test_unreachable_pauses_no_burn_and_isolated(tmp_path, monkeypatch):
    # Close a bound port so nothing is listening -> association cannot form.
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    dead_port = s.getsockname()[1]
    s.close()

    q = _seed_forward(tmp_path, ["1.2.840.113543.99.20"])
    primary = OutboxQueue(outbox_dir=str(tmp_path / "primary"))
    assert primary.enqueue(_make_ct("1.2.840.113543.99.99")) is True

    # Swap main's shutdown event for a local Event so we stop the loop
    # deterministically; monkeypatch restores it after the test.
    stop = threading.Event()
    monkeypatch.setattr("clean_code.main._shutdown_requested", stop)

    def _run():
        _forward_worker(
            q,
            "FWD_SCP",
            "CLBRIDGE",
            "127.0.0.1",
            dead_port,
            network_timeout=2,
            dimse_timeout=2,
            acse_timeout=2,
        )

    t = threading.Thread(target=_run, name="fwd-test-worker", daemon=True)
    t.start()
    time.sleep(1.2)  # an association attempt + part of the pause window
    stop.set()
    t.join(timeout=3.0)

    # No retry burn: still pending, retry_count stayed 0.
    assert q.pending_count() == 1
    meta = q._meta_path(q._pending, "1.2.840.113543.99.20")
    if os.path.isfile(meta):
        import json

        with open(meta, "r", encoding="utf-8") as f:
            m = json.load(f)
        assert m.get("retry_count", 0) == 0, "unreachable SCP must NOT burn retries"
    # Isolation: the forward worker never touched the primary queue.
    assert primary.pending_count() == 1
    # Primary delivery is independent: manual mark_delivered (the primary
    # OUTBOX worker's job) clears it regardless of the forward stall.
    primary.mark_delivered("1.2.840.113543.99.99")
    assert primary.pending_count() == 0


# ---------------------------------------------------------------------------
# 4. Restart replay: files already in forward/pending drain after a restart
# ---------------------------------------------------------------------------
def test_restart_replay_drains_pending(tmp_path, monkeypatch):
    scp = _ForwardScp(respond_status=0x0000)
    uids = ["1.2.840.113543.99.30", "1.2.840.113543.99.31"]
    dirpath = str(tmp_path / "fwd")
    q1 = OutboxQueue(outbox_dir=dirpath)
    for u in uids:
        assert q1.enqueue(_make_ct(u)) is True
    # Simulate a restart: open a NEW OutboxQueue over the SAME directory. The
    # pending items on disk are recovered (OutboxQueue.recover_stuck semantics)
    # and the worker drains them.
    q2 = OutboxQueue(outbox_dir=dirpath)
    assert q2.pending_count() == 2

    stop = threading.Event()
    monkeypatch.setattr("clean_code.main._shutdown_requested", stop)

    def _run():
        _forward_worker(
            q2,
            "FWD_SCP",
            "CLBRIDGE",
            "127.0.0.1",
            scp.port,
            network_timeout=5,
            dimse_timeout=5,
            acse_timeout=5,
        )

    t = threading.Thread(target=_run, name="fwd-replay", daemon=True)
    t.start()
    deadline = time.time() + 6.0
    while time.time() < deadline and q2.pending_count() > 0:
        time.sleep(0.1)
    stop.set()
    t.join(timeout=3.0)

    assert q2.pending_count() == 0
    assert set(scp.received) == set(uids)
    scp.shutdown()


# ---------------------------------------------------------------------------
# 5. SCP cannot accept one of the batch's requested contexts -> capability
#    verdict: that instance burns retries -> after max_retries it goes to
#    undelivered, counters converge, and there is NO requeue loop.
# ---------------------------------------------------------------------------
def test_context_rejected_retries_to_undelivered_no_loop(tmp_path):
    from clean_code.metrics import forward_undelivered_total

    # SCP supports ONLY MR. We enqueue a mixed batch: one MR instance the SCP
    # accepts (delivered) and one CT instance whose presentation context is NOT
    # negotiated. The association establishes on the MR context, so the CT item
    # reaches the capability-verdict branch (its context is absent from the
    # accepted set) rather than a whole-batch association failure.
    MR_UID = "1.2.840.113543.99.60"  # MR, accepted by the SCP.
    CT_UID = "1.2.840.113543.99.61"  # CT, its context is rejected.
    scp = _ForwardScp(sop_class=MR, ts_list=(EVRLE,), respond_status=0x0000)

    q = OutboxQueue(
        outbox_dir=str(tmp_path / "fwd"),
        max_queue=100,
        max_retries=1,  # fast: 1 = undelivered now
        undelivered_total=forward_undelivered_total,
    )
    assert q.enqueue(_make_ct(MR_UID, sop_class=MR)) is True
    assert q.enqueue(_make_ct(CT_UID, sop_class=CT)) is True

    try:
        # Claim both ready items into one batch.
        items = _claim(q, [MR_UID, CT_UID])
        assert len(items) == 2
        by_uid = {uid: (sop, ts, bp) for (uid, sop, ts, bp) in items}
        assert by_uid[MR_UID][0] == MR
        assert by_uid[CT_UID][0] == CT and by_uid[CT_UID][1] == str(EVRLE)

        before_fail = _counter("forward_failed_total")
        before_und = _counter("forward_undelivered_total")
        before_del = _counter("forward_delivered_total")
        ok = _forward_process_batch(
            q,
            items,
            "FWD_SCP",
            "CLBRIDGE",
            "127.0.0.1",
            scp.port,
            network_timeout=5,
            dimse_timeout=5,
            acse_timeout=5,
        )
        assert ok is True  # association usable (MR accepted)

        # The MR instance was delivered; the CT instance was never sent.
        assert scp.received == [MR_UID]

        # Capability verdict on the CT instance: exactly ONE failed + ONE
        # undelivered (max_retries=1), ONE delivered on MR.
        assert _counter("forward_failed_total") == before_fail + 1
        assert _counter("forward_undelivered_total") == before_und + 1
        assert _counter("forward_delivered_total") == before_del + 1

        und = os.path.join(q._undelivered, q._safe_uid(CT_UID) + ".dcm")
        assert os.path.isfile(und), "CT instance must land in undelivered"

        # No requeue loop: queue is empty and nothing further can be claimed.
        assert q.pending_count() == 0
        assert q.dequeue()[0] is None
    finally:
        scp.shutdown()
