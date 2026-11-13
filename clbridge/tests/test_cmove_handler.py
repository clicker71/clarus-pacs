# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Tests for handle_c_move — C-MOVE handler: validation, job creation, dispatching.

P31: Tests for MoveDestination validation, unknown AE rejection, study UID
validation, and C-GET rejection.
"""

import pytest
from clean_code.handlers.cmove import _is_self_delivery_target, handle_c_move
from clean_code.move_jobs import MoveJobManager
from clean_code.utils import mask_patient_id
from pydicom import Dataset

from tests.conftest import make_fake_move_event


class _TestDb:
    """Minimal SharedDb-compatible wrapper for move_jobs tests."""

    def __init__(self, path):
        import sqlite3
        import threading

        self._conn = sqlite3.connect(path, check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._lock = threading.Lock()

    def locked(self):
        class _L:
            def __init__(s, lock, conn):
                s._lock, s._conn = lock, conn

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


@pytest.fixture
def move_jobs():
    """In-memory MoveJobManager with pre-registered DEST_AE peer."""
    db = _TestDb(":memory:")
    m = MoveJobManager(shared_db=db)
    m.register_peer("DEST_AE", "127.0.0.1", 11113)
    yield m
    m.close()
    db.close()


class TestHandleCMove:

    def test_rejects_missing_move_destination(self, move_jobs):
        """C-MOVE without MoveDestination → 0xC000."""
        ds = Dataset()
        ds.StudyInstanceUID = "1.2.3"
        # MoveDestination NOT set on Dataset
        event = make_fake_move_event(study_uid="1.2.3", move_dest="")
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        results = list(gen)
        assert len(results) == 1
        assert results[0] == (0xC000, None)

    def test_rejects_unknown_move_dest(self, move_jobs):
        """Unknown AE Title not in scp_targets → 0xA801."""
        ds = Dataset()
        ds.StudyInstanceUID = "1.2.3"
        ds.MoveDestination = "UNKNOWN_AE"
        event = make_fake_move_event(study_uid="1.2.3", move_dest="UNKNOWN_AE")
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="nonexistent",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        results = list(gen)
        assert len(results) == 1
        assert results[0] == (0xA801, None)

    def test_creates_job_and_yields_target(self, move_jobs):
        """Valid C-MOVE: yields destination; a completed job yields success 0.

        RSP policy (e): the generator holds the dialog (DB poll) until the
        job is terminal; a successful job makes it yield the bare int 0
        (zero sub-operations) so pynetdicom sends the final 0000 with no
        store_assoc.
        """
        move_jobs.register_peer("DEST_AE", "127.0.0.1", 11112)
        event = make_fake_move_event(
            study_uid="1.2.3.4.5",
            move_dest="DEST_AE",
            calling_ae="DEST_AE",
            calling_ip="127.0.0.1",
        )
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        # yield 1: (addr, port) — Move Destination
        first = next(gen)
        assert first == ("127.0.0.1", 11112)
        # Job #1 exists as pending (create_job ran inside the generator).
        _, _, status = move_jobs.get_job_progress(1)
        assert status == "pending"
        # Complete the job -> the next pull yields the success int 0.
        move_jobs.finish_job(1, success=True)
        assert next(gen) == 0, "all-succeeded job yields 0 sub-operations"
        import pytest as _pytest

        with _pytest.raises(StopIteration):
            next(gen)

    def test_missing_study_uid(self, move_jobs):
        """C-MOVE without StudyInstanceUID → 0xC000."""
        ds = Dataset()
        ds.MoveDestination = "DEST_AE"
        event = make_fake_move_event(study_uid="", move_dest="DEST_AE")
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        results = list(gen)
        assert len(results) == 1
        assert results[0] == (0xC000, None)

    def test_empty_move_destination_string(self, move_jobs):
        """MoveDestination exists but is empty string → 0xC000."""
        ds = Dataset()
        ds.StudyInstanceUID = "1.2.3"
        ds.MoveDestination = "   "  # whitespace only
        event = make_fake_move_event(study_uid="1.2.3", move_dest="   ")
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        results = list(gen)
        assert len(results) == 1
        assert results[0] == (0xC000, None)

    def test_creates_job_in_database(self, move_jobs):
        """Valid C-MOVE creates a job in the database (status = pending)."""
        move_jobs.register_peer("DEST_AE", "127.0.0.1", 11112)
        ds = Dataset()
        ds.StudyInstanceUID = "1.2.3.4.5"
        ds.MoveDestination = "DEST_AE"
        event = make_fake_move_event(
            study_uid="1.2.3.4.5",
            move_dest="DEST_AE",
            calling_ae="DEST_AE",
            calling_ip="127.0.0.1",
        )
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        next(gen)  # consume first yield
        gen.close()
        job = move_jobs.pick_pending_job()
        assert job is not None
        assert job["study_uid"] == "1.2.3.4.5"
        assert job["move_dest_ae"] == "DEST_AE"

    def test_lazy_reload_picks_up_new_peer(self, move_jobs, tmp_path):
        """C-MOVE miss triggers peers.conf reload — new peer picked up without restart."""
        move_jobs.lazyload_peers_interval = 0
        peers_file = tmp_path / "peers.conf"
        ds = Dataset()
        ds.StudyInstanceUID = "1.2.3"
        ds.MoveDestination = "NEW_AE"
        event = make_fake_move_event(
            study_uid="1.2.3",
            move_dest="NEW_AE",
            calling_ae="NEW_AE",
            calling_ip="10.0.0.1",
        )
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file=str(peers_file),
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        results = list(gen)
        assert results == [(0xA801, None)], "Should reject unknown peer"

        peers_file.write_text("NEW_AE  10.0.0.1:104\n")
        event2 = make_fake_move_event(
            study_uid="1.2.3",
            move_dest="NEW_AE",
            calling_ae="NEW_AE",
            calling_ip="10.0.0.1",
        )
        gen2 = handle_c_move(
            event2,
            move_jobs,
            peers_file=str(peers_file),
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        first = next(gen2)
        assert first == ("10.0.0.1", 104), "Should succeed after peers.conf reload"
        # Job created (job #2; #1 was rejected as A801 so never created).
        move_jobs.finish_job(1, success=True)
        assert next(gen2) == 0, "all-succeeded job yields 0 sub-operations"
        gen2.close()


class TestCMoveSelfLoopGuards:
    """P0/P2/P3 (2608): self-delivery guard, CIDR substitution, poll timeout."""

    def test_self_delivery_loopback_rejected(self, move_jobs):
        """Move Destination pointing at the bridge's own listener -> 0xA801, no job."""
        move_jobs.register_peer("SELF_AE", "127.0.0.1", 8104)
        event = make_fake_move_event(
            study_uid="1.2.3",
            move_dest="SELF_AE",
            calling_ae="SELF_AE",
            calling_ip="127.0.0.1",
        )
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
            bridge_dimse_host="0.0.0.0",
            bridge_dimse_port=8104,
        )
        results = list(gen)
        assert results == [(0xA801, None)]
        assert move_jobs.pick_pending_job(timeout=0) is None

    def test_cidr_host_substituted_with_client_ip(self, move_jobs):
        """CIDR peers entry: target_host replaced by the calling client IP.

        After the destination yield the generator polls the DB, so the job
        is completed (success) before the next pull -> yield 0.
        """
        move_jobs.register_peer("CIDR_AE", "192.168.1.0/24", 11113)
        event = make_fake_move_event(
            study_uid="1.2.3",
            move_dest="CIDR_AE",
            calling_ae="CIDR_AE",
            calling_ip="192.168.1.5",
        )
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
            bridge_dimse_host="0.0.0.0",
            bridge_dimse_port=8104,
        )
        first = next(gen)
        assert first == ("192.168.1.5", 11113)
        job_id = 1  # fresh manager: first study job
        move_jobs.finish_job(job_id, success=True)
        assert next(gen) == 0, "all-succeeded job yields 0 sub-operations"
        gen.close()
        job = move_jobs.get_job_progress(job_id)
        assert job[2] == "success"

    def test_poll_timeout_finishes_jobs_as_failed(self, move_jobs):
        """Injected 0s poll timeout: unfinished job delivered NOTHING ->
        final 0xC000 (Failure), because "Sub-operations Complete" (0xB000)
        would be a lie at zero completed slices (PS3.4 Table C.4-2). The
        status must be the terminal yield, not a bare sub-operation count.
        The Identifier must NOT carry (0000,0902): Error Comment is a
        command-set element (PS3.7 Table 9.1-1), not a data-set element."""
        move_jobs.register_peer("DEST_AE", "127.0.0.1", 11112)
        event = make_fake_move_event(
            study_uid="1.2.3",
            move_dest="DEST_AE",
            calling_ae="DEST_AE",
            calling_ip="127.0.0.1",
        )
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
            bridge_dimse_host="0.0.0.0",
            bridge_dimse_port=8104,
            move_poll_timeout=0,
        )
        results = list(gen)
        status, identifier = results[-1]
        assert status == 0xC000, f"zero-delivered job -> Failure, got {hex(status)}"
        assert identifier is not None
        assert (
            0x0000,
            0x0902,
        ) not in identifier, (
            "Error Comment is a command-set element, not a data-set element"
        )
        assert move_jobs.pick_pending_job(timeout=0) is None
        _, _, st = move_jobs.get_job_progress(1)
        assert st == "failed"


class TestCMoveRspPolicy:
    """1112 RSP policy: the held dialog yields a truthful count + final.

    handle_c_move now polls the DB until every job is terminal and then
    branches:
    - all succeeded -> yield 0 (int): pynetdicom emits the final 0000, no
      store_assoc;
    - failures -> yield max(1, expected_total), then 0xB000/0xC000 +
      (0008,0058) - a bare 0 would send 0x0000 Success and stop reading;
    - cancelled -> yield 1, then 0xFE00 + (0008,0058).
    No QIDO HTTP is issued in the reactor (the instance-count lookup and its
    QIDO latency left the DIMSE reactor).
    """

    def _start(
        self,
        move_jobs,
        study="1.2.3.4.5",
        dest="DEST_AE",
        host="127.0.0.1",
        port=11112,
        ip="127.0.0.1",
        cancelled=False,
    ):
        move_jobs.register_peer(dest, host, port)
        event = make_fake_move_event(
            study_uid=study, move_dest=dest, calling_ae=dest, calling_ip=ip
        )
        if cancelled:
            flag = {"c": False}

            def _cf():
                return flag["c"]

            event.is_cancelled = _cf
            return event, flag
        return event, None

    def test_success_yields_zero_and_never_emits_progress(self, move_jobs):
        """All-successful job: yield dest, then int 0, then end — no 0xFF00."""
        event, _ = self._start(move_jobs)
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        assert next(gen) == ("127.0.0.1", 11112)
        move_jobs.finish_job(1, success=True)
        pulled = list(gen)  # resumes poll -> yields 0 -> ends
        assert pulled == [0], f"expected only the success int 0, got {pulled}"

    def test_success_makes_no_http_call(self, move_jobs, monkeypatch):
        """The reactor issues NO QIDO/HTTP during a full success C-MOVE.

        Regression for the removed `_get_study_instance_count` QIDO lookup,
        whose latency (up to 3 s) used to block the DIMSE reactor.
        """
        calls = []
        import clean_code.handlers.cmove as cm

        def _fake_get(*a, **k):
            calls.append((a, k))
            raise AssertionError("reactor must not issue HTTP")

        monkeypatch.setattr(cm.requests, "get", _fake_get)
        # The deleted function itself is gone from the module.
        assert not hasattr(cm, "_get_study_instance_count")
        event, _ = self._start(move_jobs)
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        next(gen)
        move_jobs.finish_job(1, success=True)
        assert list(gen) == [0]
        assert calls == [], f"reactor issued HTTP: {calls}"

    def test_failure_partial_yields_expected_total_then_warning_0008_0058(
        self, move_jobs
    ):
        """Job finished failed with completed<total -> yield the expected
        total (keeps the dialog open), then 0xB000 with (0008,0058) Failed
        SOP Instance UID List (PS3.4 C.4.2.1.4.2)."""
        event, _ = self._start(move_jobs)
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        assert next(gen) == ("127.0.0.1", 11112)
        move_jobs.update_job_progress(1, completed=2, failed=1, total=3)
        move_jobs.set_job_failed_sops(1, ["1.2.3.4.5.6"])
        move_jobs.finish_job(1, success=False)
        pulled = list(gen)
        assert len(pulled) == 2
        assert pulled[0] == 3, (
            "a partial failure advertises the expected total so pynetdicom "
            "keeps the dialog open and reads the 0xB000 (a bare 0 would send "
            "0x0000 Success and stop reading)"
        )
        status, identifier = pulled[1]
        assert status == 0xB000, hex(status)
        assert identifier is not None and (0x0008, 0x0058) in identifier

    def test_failure_zero_yields_one_then_c000_0008_0058(self, move_jobs):
        """Job finished failed with zero completed -> yield 1 (the dialog
        must stay open), then 0xC000 (Failure, not a lying "sub-operations
        complete" Warning). The Identifier must NOT carry (0000,0902) Error
        Comment - it is a command-set element (PS3.7 Table 9.1-1), not a
        data-set element."""
        event, _ = self._start(move_jobs)
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        assert next(gen) == ("127.0.0.1", 11112)
        move_jobs.finish_job(1, success=False)
        pulled = list(gen)
        assert len(pulled) == 2
        assert pulled[0] == 1, (
            "zero delivered must still advertise a non-zero count (a bare 0 "
            "would send 0x0000 Success and drop the 0xC000)"
        )
        status, identifier = pulled[1]
        assert status == 0xC000, hex(status)
        assert identifier is not None
        assert (
            0x0000,
            0x0902,
        ) not in identifier, (
            "Error Comment is a command-set element, not a data-set element"
        )

    def test_cancel_yields_one_then_fe00_0008_0058(self, move_jobs):
        """Cancel pending: yield 1, then 0xFE00 Cancel with (0008,0058), and
        the unfinished job is finished as failed."""
        event, flag = self._start(move_jobs, cancelled=True)
        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
        )
        assert next(gen) == ("127.0.0.1", 11112)
        # The in-flight slice was attempted but not delivered on cancel.
        move_jobs.update_job_progress(1, completed=1, failed=0, total=2)
        move_jobs.set_job_failed_sops(1, ["1.2.3.4.5.6"])
        flag["c"] = True  # cancel observed at the poll
        results = list(gen)
        # The first payloaded yield must be the non-empty int 1 (keeps the
        # dialog open so pynetdicom reads the FE00 status yield).
        assert results[0] == 1, f"cancel branch yields 1 first, got {results}"
        status, identifier = results[1]
        assert status == 0xFE00, hex(status)
        assert identifier is not None and (0x0008, 0x0058) in identifier
        _, _, st = move_jobs.get_job_progress(1)
        assert st == "failed", "cancelled job finished as failed"


class TestSelfDeliveryTargetAliases:
    """P0 (2608): alias matching for the self-delivery guard (pure unit)."""

    def test_bind_any_alias_matches_loopback(self):
        """Bridge listening on 0.0.0.0: loopback target is self."""
        assert _is_self_delivery_target("127.0.0.1", 8104, "0.0.0.0", 8104)
        assert _is_self_delivery_target("localhost", 8104, "0.0.0.0", 8104)
        assert _is_self_delivery_target("0.0.0.0", 8104, "0.0.0.0", 8104)

    def test_explicit_host_alias_matches(self):
        """Bridge listening on a concrete host: localhost/loopback are self."""
        assert _is_self_delivery_target("192.168.1.10", 8104, "192.168.1.10", 8104)
        assert _is_self_delivery_target("localhost", 8104, "192.168.1.10", 8104)

    def test_port_mismatch_is_not_self(self):
        """Same host, different port: legitimate viewer on another service."""
        assert not _is_self_delivery_target("127.0.0.1", 11113, "127.0.0.1", 8104)

    def test_foreign_host_is_not_self(self):
        """Foreign host on the same port: not the bridge."""
        assert not _is_self_delivery_target("10.0.0.5", 8104, "127.0.0.1", 8104)

    def test_unset_listener_is_not_self(self):
        """No bridge port/host configured: guard is inert (fail-open)."""
        assert not _is_self_delivery_target("127.0.0.1", 8104, "127.0.0.1", None)
        assert not _is_self_delivery_target(None, 8104, "127.0.0.1", 8104)


class TestPatientIdIsNotLogged:
    """PatientID is a national/MRN identifier: it reaches the log only as a
    deterministic 12-hex token, never raw (152-FZ)."""

    def test_c_move_request_line_hides_the_raw_id(self, move_jobs, caplog):
        import logging

        event = make_fake_move_event(study_uid="1.2.3", move_dest="DEST_AE")
        event.identifier.PatientID = "98765432"
        with caplog.at_level(logging.INFO):
            gen = handle_c_move(
                event,
                move_jobs,
                peers_file="",
                clarus_url="http://127.0.0.1:8024/dicomweb",
                bridge_ae_title="TEST_BRIDGE",
                # The reactor holds the dialog until every job is terminal;
                # the request line is logged before the first yield, so a zero
                # poll timeout ends the generator without waiting on a job
                # this test never runs.
                move_poll_timeout=0,
            )
            list(gen)

        lines = [r.getMessage() for r in caplog.records if "C-MOVE from" in r.message]
        assert lines, [r.message for r in caplog.records]
        assert all("98765432" not in line for line in lines), lines
        assert any(mask_patient_id("98765432") in line for line in lines), lines


class _CancelAfterN:
    """Stand-in for the pynetdicom association cancel state: the value changes
    while the job runs, as a C-CANCEL arriving mid-transfer does."""

    def __init__(self, after):
        self._reads = 0
        self._after = after

    def poll(self):
        self._reads += 1
        return self._reads > self._after


class _ConsumingCancel:
    """A cancel state that is DRAINED by reading it, as pynetdicom's
    `is_cancelled` property is: the C-CANCEL is consumed by the read that
    observes it, so a second read sees False. This is the property that made
    `hasattr(event, "is_cancelled")` destructive - it read the state before
    the callback was ever installed."""

    def __init__(self):
        self._pending = True
        self.reads = 0

    @property
    def is_cancelled(self):
        self.reads += 1
        if self._pending:
            self._pending = False
            return True
        return False


class TestCancelIsNotConsumedByProbing:
    """A cancel already pending when the handler installs its callback must
    still end the transfer with 0xFE00. Presence must be decided on the TYPE:
    probing the instance reads (and drains) the state."""

    def test_a_pending_cancel_survives_the_presence_probe(self, move_jobs):
        event = make_fake_move_event(study_uid="1.2.3", move_dest="DEST_AE")
        draining = _ConsumingCancel()

        # A class-level property, which is what pynetdicom exposes: reading it
        # consumes the pending C-CANCEL. The event already has it set BEFORE
        # the handler installs the callback, exactly the case that broke.
        class _WithConsumingCancel(type(event)):
            @property
            def is_cancelled(self):
                return draining.is_cancelled

        event.__class__ = _WithConsumingCancel

        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
            move_poll_timeout=5,
        )
        assert next(gen) == ("127.0.0.1", 11113)  # dispatch
        assert next(gen) == 1
        status, _identifier = next(gen)

        assert status == 0xFE00, (
            "the pending C-CANCEL was consumed by the presence probe; the "
            f"transfer reported {status:#06x} instead of Cancel"
        )
        assert draining.reads >= 1, "the state was never really read"


class TestCancelIsReadPerPoll:
    """A C-CANCEL that arrives AFTER dispatch must still end the transfer with
    0xFE00, so the cancel flag is read on every poll, never snapshotted."""

    def test_cancel_arriving_after_dispatch_yields_cancel_status(self, move_jobs):
        event = make_fake_move_event(study_uid="1.2.3", move_dest="DEST_AE")
        probe = _CancelAfterN(after=1)  # False on the first read, True after
        event.is_cancelled = probe.poll

        gen = handle_c_move(
            event,
            move_jobs,
            peers_file="",
            clarus_url="http://127.0.0.1:8024/dicomweb",
            bridge_ae_title="TEST_BRIDGE",
            move_poll_timeout=5,
        )
        assert next(gen) == ("127.0.0.1", 11113)  # dispatch
        assert next(gen) == 1  # sub-operation count before the final status
        status, _identifier = next(gen)

        assert status == 0xFE00, f"expected Cancel (0xFE00), got {status:#06x}"
        assert probe._reads >= 2, "cancel state was not re-read after dispatch"
