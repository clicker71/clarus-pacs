# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0911 CodeRabbit batch 1 regression tests - one falsifiable test per fix.

Each test maps to one finding class (A-G). Reverting the corresponding fix
must make the test fail. A2/B2/B3/F2 are covered by updated assertions in the
existing suites (test_cstore_handler.py, test_nevent.py, test_cmove_handler.py,
test_status_fixes.py, test_storage_commitment_wire.py, test_cfind_handler.py)
rather than duplicated here.
"""

import os
import sqlite3
import threading
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import clean_code.handlers.cmove as cm
import clean_code.http_helpers as hh
import httpx
from clean_code.config import ConfParser
from clean_code.handlers.naction import handle_n_action
from clean_code.http_server import BridgeHTTPHandler
from clean_code.main import _init_dimse_ae, _populate_args
from clean_code.move_jobs import MoveJobManager
from clean_code.outbox import OutboxQueue
from clean_code.protocol import _json_to_dataset_mwl
from pydicom import Dataset

from tests.conftest import make_fake_move_event, make_fake_naction_event

MR = "1.2.840.10008.5.1.4.1.1.4"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
class _TestDb:
    """Minimal SharedDb-compatible wrapper (mirrors the handler test suite)."""

    def __init__(self, path):
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


def _noop_counter():
    return type(
        "C",
        (),
        {"labels": lambda *a, **k: type("L", (), {"inc": lambda *a, **k: None})()},
    )()


def _counter(name):
    """Delta-friendly read of a prometheus counter's internal value."""
    from clean_code import metrics as m

    obj = getattr(m, name, None)
    if obj is not None and hasattr(obj, "_value"):
        return obj._value.get()
    return 0


def _stub_move_jobs(ops, finish_returns=True):
    class S:
        def finish_job(self, job_id, success=False, error_message=None):
            ops["finished"] = (job_id, success, error_message)
            return finish_returns

        def record_scp_success(self, *a, **k):
            pass

        def record_scp_failure(self, *a, **k):
            pass

        def set_job_failed_sops(self, job_id, sops):
            ops["failed_sops"] = sops

        def update_job_progress(self, *a, **k):
            pass

        def is_job_cancelled(self, job_id):
            return False

    return S()


def _drive_execute(monkeypatch, stream_result, study_sops=None, finish_returns=True):
    """Run _execute_cmove_job with the network legs stubbed (test_rulec_resume
    pattern) and return the recorded finish tuple."""
    calls = {"assoc": 0}

    def fake_associate(*a, **k):
        calls["assoc"] += 1
        return type("A", (), {"is_established": True, "accepted_contexts": []})(), {}

    def fake_preflight(*a, **k):
        return {}

    def fake_sops(study, url, *a, **k):
        return study_sops

    def fake_caps(*a, **k):
        return []

    def fake_stream(*a, **k):
        return stream_result

    ops = {}
    stub = _stub_move_jobs(ops, finish_returns=finish_returns)
    monkeypatch.setattr(cm, "_associate_for_move", fake_associate)
    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", fake_preflight)
    monkeypatch.setattr(cm, "_study_sop_uids", fake_sops)
    monkeypatch.setattr(cm, "_get_clarus_transfer_syntaxes", fake_caps)
    monkeypatch.setattr(cm, "_stream_and_send_study", fake_stream)
    monkeypatch.setattr(cm, "move_jobs_counter", _noop_counter())

    cm._execute_cmove_job(
        99,
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
    return ops


# ---------------------------------------------------------------------------
# Class A - silent swallow -> optimistic default
# ---------------------------------------------------------------------------
class TestClassA:

    def test_naction_refuses_unregisterable_commit(self, mock_shutdown_not_set):
        """A1: an N-ACTION whose commit batch cannot be registered (no
        resolvable requestor address/port) must be refused 0x0110, not
        acknowledged 0x0000 - the Transaction UID would hang at the modality
        while no N-EVENT-REPORT can ever go out."""
        event = make_fake_naction_event(
            tx_uid="1.2.3",
            sop_list=[(MR, "1.2.3.4.5.6.7")],
        )
        tracker = MagicMock()
        status, rsp = handle_n_action(
            event,
            "http://127.0.0.1:1/dicomweb",
            False,
            MagicMock(),
            commit_tracker=tracker,
            resolve_scp=lambda ae, ip: None,
        )
        assert status == 0x0110
        assert rsp is None
        tracker.register.assert_not_called()

    def test_failed_count_lower_bounded_by_gap(self, monkeypatch):
        """A3: an instance whose SOP UID never parsed never enters _fs1, so
        len(failed_sop_uids) can be 0 while total > completed. The final
        failure count must be at least total - completed, or a study with
        losses would be finished as success (0x0000)."""
        # delivered 2 of 4, EMPTY per-attempt failed set, no cancel/no-ts/lost.
        stream = (
            2,
            0,
            4,
            [(MR, "u1"), (MR, "u2")],
            [],
            False,
            False,
            False,
        )
        ops = _drive_execute(monkeypatch, stream, study_sops=None)
        assert ops["finished"][1] is False, "partial study must NOT finish as success"
        assert "2 failed" in ops["finished"][2], ops["finished"]

    def test_stow_single_transport_error_is_retryable(self, monkeypatch):
        """A4: a mid-request transport break (RemoteProtocolError) is
        transient -> 0xA700, not the permanent 0xC000 refusal."""
        monkeypatch.setattr(hh.time, "sleep", lambda *a: None)

        class FakeClient:
            def post(self, *a, **k):
                raise httpx.RemoteProtocolError("peer reset")

        status, _, _ = hh.stow_single_instance(
            Dataset(),
            "http://127.0.0.1:1/dicomweb",
            raw_bytes=b"x",
            client=FakeClient(),
        )
        assert status == 0xA700

    def test_stow_batch_transport_error_is_retryable(self, monkeypatch):
        """A4 (batch half): the same transient break is 0xA700 per SOP."""
        monkeypatch.setattr(hh.time, "sleep", lambda *a: None)

        class FakeClient:
            def post(self, *a, **k):
                raise httpx.RemoteProtocolError("peer reset")

        results, _ = hh.stow_batch_instances(
            [("1.2.3", Dataset(), b"x")],
            "http://127.0.0.1:1/dicomweb",
            client=FakeClient(),
        )
        assert results and all(s == 0xA700 for _uid, s, _h, _m in results)

    def test_forward_missing_sop_ts_failed_without_association(self, tmp_path):
        """A5: a forward item that lost its sop_class/stored_ts is marked
        failed BEFORE any association and never blocks the batch (returns
        True, no retry forever)."""
        from clean_code.main import _forward_process_batch
        from pydicom.dataset import FileMetaDataset
        from pydicom.uid import ExplicitVRLittleEndian

        CT = "1.2.840.10008.5.1.4.1.1.2"
        q = OutboxQueue(str(tmp_path / "fwd"), max_queue=100)
        uid = "1.2.840.113543.99.1"
        ds = Dataset()
        ds.SOPClassUID = CT
        ds.SOPInstanceUID = uid
        ds.StudyInstanceUID = "1.2.3." + uid
        ds.SeriesInstanceUID = "1.2.3.4." + uid
        ds.PatientName = "FWD^Test"
        ds.file_meta = FileMetaDataset()
        ds.file_meta.MediaStorageSOPClassUID = CT
        ds.file_meta.MediaStorageSOPInstanceUID = uid
        ds.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
        assert q.enqueue(ds) is True
        uid2, _ds2, blob_path, _raw = q.dequeue()
        assert uid2 == uid
        # Corrupted item: no sop_class / stored_ts.
        items = [(uid2, "", "", blob_path)]
        before = _counter("forward_failed_total")
        ok = _forward_process_batch(
            q,
            items,
            "FWD_SCP",
            "CLBRIDGE",
            "127.0.0.1",
            1,
            network_timeout=2,
            dimse_timeout=2,
            acse_timeout=2,
        )
        assert ok is True, "batch with nothing sendable is terminal, not retryable"
        assert _counter("forward_failed_total") == before + 1


# ---------------------------------------------------------------------------
# Class B - protocol primitive from memory
# ---------------------------------------------------------------------------
class TestClassB:

    def test_storage_commitment_uses_well_known_instance_uid(self):
        """B1: the N-ACTION Affected SOP Instance UID is the well-known
        constant (PS3.4 J.3.2), never the Transaction UID."""
        sop_list = [(MR, "1.2.3.4.5.6.7")]
        mock_assoc = MagicMock()
        mock_assoc.is_established = True
        ctx = MagicMock()
        type(ctx).abstract_syntax = property(lambda self: "1.2.840.10008.1.20.1")
        mock_assoc.accepted_contexts = [ctx]
        mock_assoc.send_n_action.return_value = (MagicMock(Status=0x0000), None)

        with patch("clean_code.handlers.cmove.AE") as mock_ae_cls:
            mock_ae = MagicMock()
            mock_ae.associate.return_value = mock_assoc
            mock_ae_cls.return_value = mock_ae
            cm._send_storage_commitment(
                "WEASIS", "192.168.1.20", 11112, sop_list, "CLARUS_BRIDGE"
            )

        args, kwargs = mock_assoc.send_n_action.call_args
        assert kwargs.get("instance_uid") == "1.2.840.10008.1.20.1.1"
        # Transaction UID stays only in the data set (0008,1195).
        assert str(args[0].TransactionUID) != kwargs.get("instance_uid")


# ---------------------------------------------------------------------------
# Class C - unbounded resource / cap
# ---------------------------------------------------------------------------
class TestClassC:

    def test_default_cmove_workers_capped_at_16(self, monkeypatch, tmp_path):
        """C1: the cpu-scaled C-MOVE worker default is capped at 16, so a
        64-core host no longer defaults to 32 (outside the valid range)."""
        import clean_code.main as m

        monkeypatch.setattr(m.os, "cpu_count", lambda: 128)
        cfg_path = tmp_path / "clbridge.conf"
        cfg_path.write_text(
            "[server]\nport = 8104\n"
            "[dicomweb]\nclarus_url = http://127.0.0.1:8024/dicomweb\n"
            "[metrics]\nhealth_port = 8105\n"
            "[outbox]\noutbox_dir = outbox\n",
            encoding="utf-8",
        )
        args = _populate_args(ConfParser(str(cfg_path)))
        assert args.c_move_workers == 16

    def test_init_dimse_ae_registers_no_requested_contexts(self):
        """C2: the SCP AE registers only SUPPORTED contexts. A registry over
        128 classes would make add_requested_context raise ValueError, so
        building the AE for such a registry proves the SCU proposal is gone."""
        sop_registry = {
            f"1.2.840.10008.5.1.4.1.1.{i}": {"accept": True} for i in range(130)
        }
        args = SimpleNamespace(
            ae_title="TEST_BRIDGE",
            inbound_network_timeout=0,
            inbound_acse_timeout=15,
            inbound_dimse_timeout=60,
            max_associations=10,
            expose_port=None,
            expose_ae_title="TEST_QA",
            qa_url="",
            bridge_host="127.0.0.1",
            bridge_port=11112,
            log_level="INFO",
            storage_commitment_presence=False,
        )
        ae, expose_ae, handlers = _init_dimse_ae(
            args, None, None, sop_registry, None, None, None, None
        )
        assert ae.requested_contexts == []
        assert expose_ae is None
        assert len(ae.supported_contexts) >= 130
        assert handlers


# ---------------------------------------------------------------------------
# Class D - state in the wrong scope/time
# ---------------------------------------------------------------------------
class TestClassD:

    def test_cancel_latches_callback_not_clears(self):
        """D1: after the reactor answers Cancel, the worker's is_job_cancelled
        must still read True - the callback is LATCHED, not cleared (a clear
        would let the worker keep streaming and push Storage Commitment)."""
        db = _TestDb(":memory:")
        m = MoveJobManager(shared_db=db)
        try:
            m.register_peer("DEST_AE", "127.0.0.1", 11112)
            event = make_fake_move_event(
                study_uid="1.2.3.4.5",
                move_dest="DEST_AE",
                calling_ae="DEST_AE",
                calling_ip="127.0.0.1",
            )
            flag = {"c": False}
            event.is_cancelled = lambda: flag["c"]
            with patch("clean_code.handlers.cmove.time.sleep", lambda *a: None):
                gen = cm.handle_c_move(
                    event,
                    m,
                    peers_file="",
                    clarus_url="http://127.0.0.1:1/dicomweb",
                    bridge_ae_title="TEST_BRIDGE",
                )
                assert next(gen) == ("127.0.0.1", 11112)
                flag["c"] = True
                list(gen)
            assert (
                m.is_job_cancelled(1) is True
            ), "cancel path must latch the callback, not clear it"
        finally:
            m.close()
            db.close()

    def test_mark_delivered_removes_stale_undelivered_copy(self, tmp_path):
        """D2: a successful delivery removes any stale undelivered/{uid}.dcm
        left by an earlier exhausted-retry pass, so delivered() can stop
        reporting the saved instance as a Storage Commitment failure."""
        q = OutboxQueue(str(tmp_path))
        uid = "1.2.3.4.5"
        und = q._dcm_path(q._undelivered, uid)
        os.makedirs(os.path.dirname(und), exist_ok=True)
        with open(und, "wb") as f:
            f.write(b"stale")
        pend = q._dcm_path(q._pending, uid)
        os.makedirs(os.path.dirname(pend), exist_ok=True)
        with open(pend, "wb") as f:
            f.write(b"fresh")

        assert q.delivered(uid) is False
        q.mark_delivered(uid)
        assert q.delivered(uid) is True


# ---------------------------------------------------------------------------
# Class E - race on shared mutable state
# ---------------------------------------------------------------------------
class TestClassE:

    def test_suppress_filter_is_thread_local(self):
        """E1: pynetdicom suppression is per-thread; a streaming C-MOVE must
        not silence pynetdicom logs on unrelated threads."""
        cm._suppress_state.suppress = True
        assert cm._SuppressPynetdicomFilter().filter(None) is False
        cm._suppress_state.suppress = False
        assert cm._SuppressPynetdicomFilter().filter(None) is True

        seen = {}

        def _other():
            seen["value"] = cm._SuppressPynetdicomFilter().filter(None)

        t = threading.Thread(target=_other)
        t.start()
        t.join()
        assert seen["value"] is True, "other thread inherits the default (off)"

    def test_cache_evict_uses_pop_not_del(self):
        """E2: eviction must use pop(key, None), not `del`, so two workers
        evicting the same expired key cannot raise KeyError."""

        class SpyDict(dict):
            used_del = False

            def __delitem__(self, key):
                SpyDict.used_del = True
                super().__delitem__(key)

        cache = SpyDict()
        cache["expired"] = (0.0, {})
        cm._cache_evict(cache, now=1.0)
        assert not SpyDict.used_del, "eviction must not use `del`"
        assert "expired" not in cache


# ---------------------------------------------------------------------------
# Class F - honest outcome reporting
# ---------------------------------------------------------------------------
class TestClassF:

    def test_counter_not_double_counted_when_finish_returns_false(self, monkeypatch):
        """F1: when finish_job returns False (another waker already finished
        the job), the move_jobs_counter must NOT be incremented."""
        calls = {"counter": 0}

        class _L:
            def inc(self):
                calls["counter"] += 1

        class _C:
            def labels(self, **k):
                return _L()

        monkeypatch.setattr(cm, "move_jobs_counter", _C())
        stream = (2, 0, 2, [(MR, "u1"), (MR, "u2")], [], False, False, False)
        ops = _drive_execute(monkeypatch, stream, study_sops=None, finish_returns=False)
        assert ops["finished"][1] is True
        assert calls["counter"] == 0, "a second waker must not double-count"


# ---------------------------------------------------------------------------
# Class G - contains-vs-parse / incomplete catch
# ---------------------------------------------------------------------------
class TestClassG:

    def test_mwl_accession_resolved_from_sequence_when_flat_empty(self):
        """G1: a present-but-EMPTY flat AccessionNumber (Type 2 marker) must
        not shadow the (0040,A370) sequence carriage."""
        json_obj = {
            "00080050": {"vr": "SH"},  # Type 2: present, no value
            "0040A370": {
                "vr": "SQ",
                "Value": [{"00080050": {"vr": "SH", "Value": ["ACC123"]}}],
            },
            "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^Jane"}]},
            "00100020": {"vr": "LO", "Value": ["PID1"]},
        }
        ds = _json_to_dataset_mwl(json_obj, ["AccessionNumber"])
        assert str(ds.AccessionNumber) == "ACC123"

    def test_post_invalid_utf8_json_returns_400(self):
        """G2: json.loads on non-UTF-8 bytes raises UnicodeDecodeError (a
        ValueError, not JSONDecodeError); the handler must answer 400, not
        drop the connection."""
        h = BridgeHTTPHandler.__new__(BridgeHTTPHandler)
        h.path = "/print"
        h.headers = {"Content-Type": "application/json", "Content-Length": "4"}
        h.rfile = type("R", (), {"read": lambda self, n: b"\xff\xfe\xfd\xfc"})()
        captured = {}
        h._json_error = lambda code, msg: captured.update(code=code, msg=msg)
        h.send_error = lambda *a, **k: None
        h.send_response = lambda *a, **k: None
        h.send_header = lambda *a, **k: None
        h.end_headers = lambda: None
        h.log_message = lambda *a, **k: None
        h.do_POST()
        assert captured.get("code") == 400
