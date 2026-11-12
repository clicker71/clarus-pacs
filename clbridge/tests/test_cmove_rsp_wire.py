# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0909 RSP policy (e) - wire tests: what the bridge's C-MOVE yield pattern
actually puts on the wire, verified end-to-end through pynetdicom 3.0.4.

Method (architect's probe, analysis/0909_architect_microdicom_dimse_progress_
REPORT.md): a stub-SCP AE "BRIDGE" replays handle_c_move's yield sequence for
each policy outcome; a viewer AE "VIEWER" both initiates the C-MOVE and hosts
the Move Destination (the listener that would receive pynetdicom's store_assoc
and the C-STOREs). Every C-MOVE-RSP the SCU sees is captured.

Proves, on the wire:
1. success  -> EXACTLY ONE final RSP (0x0000) with 0 sub-operations, and
   ZERO A-ASSOCIATE to the Move Destination (pynetdicom returns before
   opening store_assoc when the sub-operation count is 0);
2. failure  -> final 0xB000 Warning + (0008,0058) Failed SOP Instance UID
   List (the count is non-empty so exactly ONE store_assoc is opened first);
3. cancel   -> final 0xFE00 Cancel + (0008,0058);
4. unsolicited C-STORE on a separate inbound association mid-transfer ->
   log-and-ignore, the success final 0x0000 is still delivered.
"""

import logging
import socket
import threading
import time

from pydicom.dataset import Dataset
from pydicom.uid import ExplicitVRLittleEndian
from pynetdicom import AE, evt
from pynetdicom.sop_class import (
    CTImageStorage,
    StudyRootQueryRetrieveInformationModelMove,
)

logging.basicConfig(level=logging.CRITICAL)

ADDR = "127.0.0.1"
STUDY = "2.16.840.114421.12451.9835700745"
FAILED_UID = "1.2.840.114421.9.99.1"


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind((ADDR, 0))
    p = s.getsockname()[1]
    s.close()
    return p


def _id_dataset():
    """Identifier Dataset carrying (0008,0058) Failed SOP Instance UID List."""
    ds = Dataset()
    ds.add_new((0x0008, 0x0058), "UI", [FAILED_UID])
    return ds


class _Scp:
    """Stub C-MOVE SCP ("BRIDGE") replaying handle_c_move's yields for one
    policy outcome. `pattern` is a callable(viewer_port) -> iterable of the
    yields the handler emits (after the destination yield)."""

    def __init__(self, outcome):
        self.ae = AE(ae_title="BRIDGE")
        self.ae.network_timeout = 30
        self.ae.acse_timeout = 30
        self.ae.dimse_timeout = 60
        self.ae.add_supported_context(StudyRootQueryRetrieveInformationModelMove)
        # The bridge SCP also carries storage contexts for store_assoc.
        self.ae.add_requested_context(CTImageStorage)
        self.ae.add_supported_context(CTImageStorage)
        self.port = _free_port()
        self.outcome = outcome
        # Rule C C5.4: unsolicited C-STORE arriving on a separate inbound
        # association is logged and ignored - the held reactor must not be
        # disturbed and the final RSP must still be delivered.
        self.unexpected_stores = []

        def _handler(event):
            yield (ADDR, self._viewer_port)
            if self.outcome == "success":
                yield 0
            elif self.outcome == "success_delayed":
                time.sleep(1.0)  # hold the reactor while an intruder arrives
                yield 0
            elif self.outcome == "failure":
                yield 1
                yield 0xB000, _id_dataset()
            elif self.outcome == "cancel":
                yield 1
                yield 0xFE00, _id_dataset()
            else:  # pragma: no cover
                raise AssertionError(self.outcome)

        self._viewer_port = None
        self.server = self.ae.start_server(
            (ADDR, self.port),
            block=False,
            evt_handlers=[
                (evt.EVT_C_MOVE, _handler),
                (evt.EVT_C_STORE, self._on_unexpected_store),
            ],
        )
        time.sleep(0.2)

    def _on_unexpected_store(self, event):
        """Log-and-ignore: record the unsolicited SOP and accept it."""
        sop_uid = getattr(event.dataset, "SOPInstanceUID", "?")
        self.unexpected_stores.append(str(sop_uid))
        return 0x0000

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


class _Viewer:
    """Viewer AE ("VIEWER"): the C-MOVE requester AND the Move Destination.
    Counts every accepted association (would-be store_assoc + C-STORE) plus
    every C-STORE received."""

    def __init__(self):
        self.ae = AE(ae_title="VIEWER")
        self.ae.network_timeout = 30
        self.ae.acse_timeout = 30
        self.ae.dimse_timeout = 60
        self.ae.add_requested_context(StudyRootQueryRetrieveInformationModelMove)
        self.ae.add_requested_context(CTImageStorage)
        self.ae.add_supported_context(CTImageStorage)
        self.accepted = []
        self.stores = []
        self.port = _free_port()
        self.server = self.ae.start_server(
            (ADDR, self.port),
            block=False,
            evt_handlers=[
                (evt.EVT_ACCEPTED, lambda e: self.accepted.append(1)),
                (evt.EVT_C_STORE, self._on_store),
            ],
        )
        time.sleep(0.2)

    def _on_store(self, event):
        self.stores.append(1)
        return 0x0000

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


def _run(outcome):
    """Return (rsp_statuses, viewer) for one outcome over the wire."""
    scp = _Scp(outcome)
    viewer = _Viewer()
    scp._viewer_port = viewer.port
    try:
        assoc = viewer.ae.associate(ADDR, scp.port, ae_title="BRIDGE")
        try:
            assert assoc.is_established, "C-MOVE association failed"
            ds = Dataset()
            ds.StudyInstanceUID = STUDY
            ds.QueryRetrieveLevel = "STUDY"
            statuses = []
            for status, identifier in assoc.send_c_move(
                ds,
                "VIEWER",
                StudyRootQueryRetrieveInformationModelMove,
                msg_id=7,
            ):
                if not status:
                    break
                if status.Status not in (0xFF00, 0xFF01):  # non-pending = final
                    statuses.append((status.Status, identifier))
                    break
            return statuses, viewer
        finally:
            try:
                assoc.release()
            except Exception:
                pass
    finally:
        scp.shutdown()
        viewer.shutdown()


def _has_failed_uid(identifier):
    """True when the Identifier carries (0008,0058) containing FAILED_UID."""
    if identifier is None:
        return False
    elem = identifier.get((0x0008, 0x0058))
    if elem is None:
        return False
    val = elem.value
    if isinstance(val, (list, tuple)):
        return FAILED_UID in [str(x) for x in val]
    return str(val) == FAILED_UID


def test_success_one_rsp_0000_no_associate():
    """Success path (yield 0): exactly ONE final RSP (0x0000) with zero
    completed sub-operations, and ZERO A-ASSOCIATE to the Move Destination."""
    statuses, viewer = _run("success")
    assert [s for s, _ in statuses] == [0x0000], statuses
    status, _identifier = statuses[-1]
    completed = getattr(status, "NumberOfCompletedSuboperations", None)
    # pynetdicom omits the counter (None) when it is zero - never non-zero.
    assert completed in (0, None), f"completed sub-ops must be 0/none, got {completed}"
    # pynetdicom returns before opening store_assoc when the count is 0.
    assert (
        viewer.accepted == []
    ), f"success path must NOT open a store_assoc; accepted={viewer.accepted}"
    assert viewer.stores == [], "no C-STORE on the success path"


def test_failure_one_rsp_b000_with_0008_0058():
    """Failure path (yield 1, then 0xB000 + (0008,0058)): the warning reaches
    the SCU with the Failed SOP Instance UID List; one store_assoc is opened
    (count non-empty) but no C-STORE arrives."""
    statuses, viewer = _run("failure")
    assert [s for s, _ in statuses] == [0xB000], statuses
    _, identifier = statuses[-1]
    assert _has_failed_uid(
        identifier
    ), "Warning must carry (0008,0058) in the Identifier"
    assert viewer.accepted, "a store_assoc is opened for the failure path"
    assert viewer.stores == [], "no C-STORE occurs (handled via moved slices)"


def test_cancel_one_rsp_fe00_with_0008_0058():
    """Cancel path (yield 1, then 0xFE00 + (0008,0058)): the Cancel final
    reaches the SCU carrying the Failed SOP Instance UID List."""
    statuses, viewer = _run("cancel")
    assert [s for s, _ in statuses] == [0xFE00], statuses
    _, identifier = statuses[-1]
    assert _has_failed_uid(
        identifier
    ), "Cancel must carry (0008,0058) in the Identifier"
    assert viewer.accepted, "a store_assoc is opened for the cancel path"
    assert viewer.stores == [], "no C-STORE occurs on cancel"


def test_unexpected_cstore_mid_move_final_rsp_survives():
    """Rule C C5.4: an unsolicited C-STORE arriving on a separate inbound
    association while the C-MOVE handler is held is logged-and-ignored - the
    transfer is undisturbed and the success final (0x0000) is still
    delivered with no store_assoc to the Move Destination (policy e)."""
    scp = _Scp("success_delayed")
    viewer = _Viewer()
    scp._viewer_port = viewer.port
    intruder_sop = "1.2.840.114421.9.99.999"
    try:
        assoc = viewer.ae.associate(ADDR, scp.port, ae_title="BRIDGE")
        try:
            assert assoc.is_established, "C-MOVE association failed"

            def _intrude():
                """Open a SECOND association and send an unsolicited C-STORE
                to the bridge SCP while the move handler holds the reactor."""
                time.sleep(0.2)
                cstore_ds = Dataset()
                cstore_ds.file_meta = Dataset()
                cstore_ds.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
                cstore_ds.SOPClassUID = str(CTImageStorage)
                cstore_ds.SOPInstanceUID = intruder_sop
                a2 = viewer.ae.associate(ADDR, scp.port, ae_title="BRIDGE")
                try:
                    if not a2.is_established:
                        raise AssertionError("intruder association failed")
                    status = a2.send_c_store(cstore_ds, msg_id=2)
                    assert status is not None, "C-STORE reply expected"
                finally:
                    try:
                        a2.release()
                    except Exception:
                        pass

            intruder = threading.Thread(target=_intrude, daemon=True)
            intruder.start()

            ds = Dataset()
            ds.StudyInstanceUID = STUDY
            ds.QueryRetrieveLevel = "STUDY"
            statuses = []
            for status, identifier in assoc.send_c_move(
                ds,
                "VIEWER",
                StudyRootQueryRetrieveInformationModelMove,
                msg_id=7,
            ):
                if not status:
                    break
                if status.Status not in (0xFF00, 0xFF01):
                    statuses.append((status.Status, identifier))
                    break
            intruder.join(timeout=10)

            # The unsolicited C-STORE was received and accepted...
            assert scp.unexpected_stores == [intruder_sop], scp.unexpected_stores
            # ...and the held C-MOVE still delivered its honest success final.
            assert [s for s, _ in statuses] == [0x0000], statuses
            # Success path: the intruder must not cost a store_assoc to the
            # Move Destination.
            assert (
                viewer.accepted == []
            ), f"success path must not open a store_assoc; accepted={viewer.accepted}"
        finally:
            try:
                assoc.release()
            except Exception:
                pass
    finally:
        scp.shutdown()
        viewer.shutdown()


class _WireDb:
    """Minimal SharedDb-shaped wrapper for MoveJobManager (one connection,
    one lock) - the same contract the bridge's own SharedDb exposes."""

    def __init__(self):
        import sqlite3

        self._conn = sqlite3.connect(":memory:", check_same_thread=False)
        self._lock = threading.Lock()

    def locked(self):
        class _L:
            def __init__(s, lock, conn):
                s._lock = lock
                s._conn = conn

            def __enter__(s):
                s._lock.acquire()
                return s._conn

            def __exit__(s, *a):
                s._lock.release()

        return _L(self._lock, self._conn)

    def close(self):
        with self._lock:
            try:
                self._conn.close()
            except Exception:
                pass


class _RealScp:
    """Stub C-MOVE SCP that drives the REAL handle_c_move, so the count the
    handler actually computes crosses the wire to the viewer."""

    def __init__(self, poll_timeout):
        from clean_code.handlers.cmove import handle_c_move
        from clean_code.move_jobs import MoveJobManager

        self.ae = AE(ae_title="BRIDGE")
        self.ae.network_timeout = 30
        self.ae.acse_timeout = 30
        self.ae.dimse_timeout = 60
        self.ae.add_supported_context(StudyRootQueryRetrieveInformationModelMove)
        self.ae.add_requested_context(CTImageStorage)
        self.ae.add_supported_context(CTImageStorage)
        self.port = _free_port()
        self.move_jobs = MoveJobManager(shared_db=_WireDb())
        self._viewer_port = None
        self._poll_timeout = poll_timeout

        def _on_move(event):
            yield from handle_c_move(
                event,
                self.move_jobs,
                peers_file="",
                clarus_url="http://127.0.0.1:8024/dicomweb",
                bridge_ae_title="BRIDGE",
                move_poll_timeout=self._poll_timeout,
            )

        self.server = self.ae.start_server(
            (ADDR, self.port),
            block=False,
            evt_handlers=[(evt.EVT_C_MOVE, _on_move)],
        )
        time.sleep(0.2)

    def wait_for_job(self, timeout=10.0):
        """Block until handle_c_move has created its job, then return the id."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.move_jobs._db.locked() as conn:
                row = conn.execute(
                    "SELECT id FROM move_jobs ORDER BY id DESC LIMIT 1"
                ).fetchone()
            if row:
                return int(row[0])
            time.sleep(0.05)
        raise AssertionError("handle_c_move never created its job")

    def shutdown(self):
        try:
            self.ae.shutdown()
        except Exception:
            pass


def _run_real(outcome, poll_timeout=30.0):
    """Drive the REAL handle_c_move; `outcome` is zero / partial / timeout.

    A helper thread simulates the worker by updating the job once the handler
    has created it; the handler's own terminal-count selection then decides
    what the viewer receives.
    """
    scp = _RealScp(poll_timeout)
    viewer = _Viewer()
    scp._viewer_port = viewer.port
    scp.move_jobs.register_peer("VIEWER", "127.0.0.1", viewer.port)

    def _simulate():
        job_id = scp.wait_for_job()
        if outcome == "zero":
            scp.move_jobs.update_job_progress(job_id, completed=0, failed=432, total=432)
            scp.move_jobs.set_job_failed_sops(job_id, [FAILED_UID])
            scp.move_jobs.finish_job(job_id, success=False)
        elif outcome == "partial":
            scp.move_jobs.update_job_progress(job_id, completed=430, failed=2, total=432)
            scp.move_jobs.set_job_failed_sops(job_id, [FAILED_UID])
            scp.move_jobs.finish_job(job_id, success=False)
        # "timeout": leave the job pending; the handler's poll timeout fires.

    sim = threading.Thread(target=_simulate, daemon=True)
    sim.start()
    try:
        assoc = viewer.ae.associate(ADDR, scp.port, ae_title="BRIDGE")
        try:
            assert assoc.is_established, "C-MOVE association failed"
            ds = Dataset()
            ds.StudyInstanceUID = STUDY
            ds.QueryRetrieveLevel = "STUDY"
            statuses = []
            for status, identifier in assoc.send_c_move(
                ds,
                "VIEWER",
                StudyRootQueryRetrieveInformationModelMove,
                msg_id=7,
            ):
                if not status:
                    break
                if status.Status not in (0xFF00, 0xFF01):
                    statuses.append((status.Status, identifier))
                    break
            return statuses, viewer
        finally:
            try:
                assoc.release()
            except Exception:
                pass
    finally:
        sim.join(timeout=30)
        scp.shutdown()
        viewer.shutdown()


def test_zero_delivered_receives_c000_with_0008_0058():
    """Zero delivered (0 of 432): the SCU receives 0xC000, not 0x0000, and
    the Identifier carries (0008,0058)."""
    statuses, viewer = _run_real("zero")
    assert [s for s, _ in statuses] == [0xC000], (
        f"a zero-delivered move must answer Failure, got {statuses}"
    )
    _, identifier = statuses[-1]
    assert _has_failed_uid(
        identifier
    ), "Failure must carry (0008,0058) in the Identifier"


def test_partial_receives_b000_with_0008_0058():
    """Partial delivery (430 of 432): 0xB000 with (0008,0058), not 0x0000."""
    statuses, viewer = _run_real("partial")
    assert [s for s, _ in statuses] == [0xB000], (
        f"a partial move must answer Warning, got {statuses}"
    )
    _, identifier = statuses[-1]
    assert _has_failed_uid(
        identifier
    ), "Warning must carry (0008,0058) in the Identifier"


def test_poll_timeout_zero_delivered_receives_c000():
    """Poll timeout with nothing delivered: 0xC000, not 0x0000."""
    statuses, viewer = _run_real("timeout", poll_timeout=2.0)
    assert [s for s, _ in statuses] == [0xC000], (
        f"a timed-out zero-delivered move must answer Failure, got {statuses}"
    )
