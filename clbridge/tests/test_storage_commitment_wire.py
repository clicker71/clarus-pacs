# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Wire-level regression tests for the Storage Commitment cycle.

Unlike the mock-based unit tests (test_storage_commitment.py, test_nevent.py),
these tests run REAL pynetdicom associations against a real SCP in a thread.
If pynetdicom's API contract drifts (handler return shapes, N-ACTION /
N-EVENT-REPORT primitives), these tests fail loudly.

Covers the production path end to end:
  _send_storage_commitment()  -> N-ACTION over the wire
  send_n_event_report()       -> N-EVENT-REPORT over the wire
  CommitTracker.resolve()     -> registers a batch, resolves it, reports it
"""

import functools
import socket
import time

from clean_code.commit_tracker import CommitTracker
from clean_code.handlers.cmove import _send_storage_commitment
from clean_code.http_helpers import _stc_commit_request
from clean_code.nevent import send_n_event_report
from pydicom import Dataset
from pydicom.uid import ImplicitVRLittleEndian, generate_uid
from pynetdicom import AE, evt
from pynetdicom.sop_class import StorageCommitmentPushModel

SOP_LIST = [
    ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.999"),
    ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.1000"),
]


def _free_port():
    """Ask the OS for a free loopback port."""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class RecordingScp:
    """Real pynetdicom SCP recording N-ACTION / N-EVENT-REPORT requests.

    The handlers use the pynetdicom 3.x contract: both must return a
    (status, dataset) tuple - the exact contract this suite guards.
    """

    def __init__(self):
        self.ae = AE(ae_title="TEST_SCP")
        self.ae.add_supported_context(
            StorageCommitmentPushModel, [ImplicitVRLittleEndian]
        )
        self.actions = []
        self.events = []
        self.port = _free_port()
        self.server = self.ae.start_server(
            ("127.0.0.1", self.port),
            block=False,
            evt_handlers=[
                (evt.EVT_N_ACTION, self.on_n_action),
                (evt.EVT_N_EVENT_REPORT, self.on_n_event_report),
            ],
        )

    def on_n_action(self, event):
        info = event.action_information
        self.actions.append(
            {
                "action_type": event.action_type,
                "tx_uid": str(getattr(info, "TransactionUID", "") or ""),
                "sops": [
                    (str(i.ReferencedSOPClassUID), str(i.ReferencedSOPInstanceUID))
                    for i in getattr(info, "ReferencedSOPSequence", [])
                ],
            }
        )
        rsp = Dataset()
        rsp.Status = 0x0000
        return rsp, None

    def on_n_event_report(self, event):
        info = event.event_information
        self.events.append(
            {
                "event_type": getattr(info, "EventTypeID", None),
                "tx_uid": str(getattr(info, "TransactionUID", "") or ""),
                "sops": [
                    (str(i.ReferencedSOPClassUID), str(i.ReferencedSOPInstanceUID))
                    for i in getattr(info, "ReferencedSOPSequence", [])
                ],
                "failed": [
                    (str(i.ReferencedSOPClassUID), str(i.ReferencedSOPInstanceUID))
                    for i in getattr(info, "FailedSOPSequence", [])
                ],
                # 2409 true-mode: the FailureReason travels with each failed
                # SOP and must be an enumerated C.14.1.1 code.
                "failed_reasons": [
                    int(getattr(i, "FailureReason", -1))
                    for i in getattr(info, "FailedSOPSequence", [])
                ],
            }
        )
        return 0x0000, None

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


def _make_scp():
    scp = RecordingScp()
    # Give the acceptor a moment to bind before the bridge side associates.
    time.sleep(0.3)
    return scp


def test_n_action_wire_cycle():
    """_send_storage_commitment sends a real N-ACTION; SCP answers 0x0000."""
    scp = _make_scp()
    try:
        ok = _send_storage_commitment(
            move_dest_ae="TEST_SCP",
            target_host="127.0.0.1",
            target_port=scp.port,
            sop_list=SOP_LIST,
            bridge_ae_title="CLARUS_BRIDGE",
        )
        assert ok is True
        assert len(scp.actions) == 1
        act = scp.actions[0]
        assert act["action_type"] == 1  # Storage Commitment Request
        assert act["tx_uid"]  # generated Transaction UID
        assert act["sops"] == SOP_LIST  # exact referenced instance set
    finally:
        scp.shutdown()


def test_n_event_report_wire_cycle():
    """send_n_event_report sends a real N-EVENT-REPORT; SCP answers 0x0000."""
    scp = _make_scp()
    try:
        tx = generate_uid()
        send_n_event_report(
            host="127.0.0.1",
            port=scp.port,
            calling_ae="TEST_SCP",
            tx_uid=tx,
            event_type=1,
            success_sops=SOP_LIST,
            failed_sops=[],
            bridge_ae_title="TEST_BRIDGE",
        )
        assert len(scp.events) == 1
        ev = scp.events[0]
        assert ev["event_type"] == 1  # success report
        assert ev["tx_uid"] == tx
        assert ev["sops"] == SOP_LIST
        assert ev["failed"] == []
    finally:
        scp.shutdown()


def test_commit_tracker_resolves_and_reports_over_wire():
    """Full Stage 2 cycle: register -> resolve -> N-EVENT-REPORT on the wire."""
    scp = _make_scp()
    try:
        tx = generate_uid()
        tracker = CommitTracker(
            verify=lambda sop_class, sop_uid: True,  # everything is stored
            report=functools.partial(
                send_n_event_report, bridge_ae_title="TEST_BRIDGE"
            ),
            interval=0.1,
            deadline_sec=300.0,
        )
        tracker.register(tx, "TEST_SCP", "127.0.0.1", scp.port, SOP_LIST)
        assert tracker.pending_count() == 1

        tracker._tick()  # resolve immediately, no deadline wait
        assert tracker.pending_count() == 0

        assert len(scp.events) == 1
        ev = scp.events[0]
        assert ev["event_type"] == 1
        assert ev["tx_uid"] == tx
        assert ev["sops"] == SOP_LIST
    finally:
        tracker.close() if "tracker" in dir() else None
        scp.shutdown()


def test_commit_tracker_reports_failures_when_unverified():
    """Unconfirmed SOPs at deadline are reported as failures (EventTypeID=2)."""
    scp = _make_scp()
    try:
        tx = generate_uid()
        tracker = CommitTracker(
            verify=lambda sop_class, sop_uid: False,  # nothing ever confirms
            report=functools.partial(
                send_n_event_report, bridge_ae_title="TEST_BRIDGE"
            ),
            interval=0.1,
            deadline_sec=300.0,
        )
        tracker.register(tx, "TEST_SCP", "127.0.0.1", scp.port, SOP_LIST)
        # Simulate deadline expiry: resolve without any pending confirmation.
        for job in list(tracker._jobs.values()):
            job.deadline = time.time() - 1.0
        tracker._tick()
        assert tracker.pending_count() == 0

        assert len(scp.events) == 1
        ev = scp.events[0]
        assert ev["event_type"] == 2  # complete-with-failures
        assert ev["failed"] == SOP_LIST
    finally:
        tracker.close() if "tracker" in dir() else None
        scp.shutdown()


# -- 2409 true-mode: the origin's verdict travels over the wire ------------


def test_true_mode_reports_the_origins_verdict_over_one_wire_cycle():
    """T1 (wire half): outbox readiness -> ONE origin POST -> ONE N-EVENT.

    The bridge's own delivery state is the readiness gate; the origin's
    answer is reported verbatim: the committed SOP in ReferencedSOPSequence
    and the other in FailedSOPSequence with FailureReason 0x0112.
    """
    import json
    import threading
    from http.server import BaseHTTPRequestHandler

    scp = _make_scp()
    try:
        sop_a = ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.201")
        sop_b = ("1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.5.1.4.1.1.2.202")

        def _item(c, u):
            return {
                "00081150": {"vr": "UI", "Value": [c]},
                "00081155": {"vr": "UI", "Value": [u]},
            }

        failed_item = _item(*sop_b)
        failed_item["00081197"] = {"vr": "US", "Value": ["0112"]}
        verdict = json.dumps(
            {
                "00081199": {"vr": "SQ", "Value": [_item(*sop_a)]},
                "00081198": {"vr": "SQ", "Value": [failed_item]},
            }
        ).encode()

        post_count = [0]

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                self.rfile.read(int(self.headers.get("Content-Length", 0)))
                post_count[0] += 1
                self.send_response(200)
                self.send_header("Content-Type", "application/dicom+json")
                self.send_header("Content-Length", str(len(verdict)))
                self.end_headers()
                self.wfile.write(verdict)

            def log_message(self, *args):
                pass

        import socketserver

        origin = socketserver.TCPServer(("127.0.0.1", 0), Handler)
        origin_thread = threading.Thread(target=origin.serve_forever, daemon=True)
        origin_thread.start()
        origin_url = f"http://127.0.0.1:{origin.server_address[1]}/dicomweb"

        # A tiny stand-in for the outbox: only sop_a has been delivered.
        class _Outbox:
            def delivered(self, sop_uid):
                return sop_uid == sop_a[1]

        outbox = _Outbox()

        def _resolve(job):
            committed, failed, _sentinel = _stc_commit_request(
                origin_url, job.tx_uid, job.sops
            )
            send_n_event_report(
                job.host,
                job.port,
                job.calling_ae,
                job.tx_uid,
                1 if not failed else 2,
                committed,
                failed,
                bridge_ae_title="TEST_BRIDGE",
            )

        tx = generate_uid()
        tracker = CommitTracker(
            verify=lambda c, u: outbox.delivered(u),
            report=send_n_event_report,
            resolve=_resolve,
            interval=0.1,
            deadline_sec=300.0,
        )
        tracker.register(tx, "TEST_SCP", "127.0.0.1", scp.port, [sop_a, sop_b])

        # sop_b is still in the outbox: no verdict is asked for yet.
        tracker._tick()
        assert post_count[0] == 0
        assert len(scp.events) == 0

        # Delivered now (the second tick's readiness signal).
        outbox.delivered = lambda sop_uid: True
        tracker._tick()

        assert post_count[0] == 1, "exactly ONE POST for the whole batch"
        assert tracker.pending_count() == 0

        assert len(scp.events) == 1
        ev = scp.events[0]
        assert ev["event_type"] == 2, "one failure -> complete-with-failures"
        assert ev["tx_uid"] == tx
        assert ev["sops"] == [sop_a], "the committed SOP travels in the success list"
        assert ev["failed"] == [sop_b]
        assert ev["failed_reasons"] == [0x0112], "the origin's own reason"
        assert 0x0111 not in ev["failed_reasons"], "true-mode never emits 0x0111"

        origin.shutdown()
        origin.server_close()
        tracker.close()
    finally:
        scp.shutdown()
