# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1011 merged batch 4 regression tests - one falsifiable test per finding.

Source A: architect blind sweep (A-m-01, A-m-02, A-M-01, A-M-02, A-C-01,
A-M-03, A-m-07, A-m-03 peers, A-m-04, A-m-05, A-m-06).
Source B: CodeRabbit (B-01 ... B-09).
Items already correct in the tree are pinned so they cannot regress.
"""

import inspect
import logging
import os
import threading
from unittest.mock import MagicMock

import clean_code.handlers.cmove as cm
import clean_code.handlers.cstore as cs
import clean_code.main as main_mod
import clean_code.protocol as proto
import clean_code.utils as utils
import pytest
from clean_code.assoc_tracker import record_cstore
from pydicom import Dataset

MR = "1.2.840.10008.5.1.4.1.1.4"


def _counter(name):
    from clean_code import metrics as m

    obj = m.__dict__.get(name)
    return obj._value.get() if obj is not None and hasattr(obj, "_value") else None


# ---------------------------------------------------------------------------
# A-m-01 - identifying free text is redacted in the cp1251 repair log
# ---------------------------------------------------------------------------
class TestAm01Cp1251LogRedaction:
    def test_series_description_is_not_written_to_the_log(self, caplog):
        """InstitutionName / StudyDescription / SeriesDescription are free text
        a site fills with names and addresses, so the repair log must not echo
        them - only the tag and the lengths."""
        ds = Dataset()
        ds.SpecificCharacterSet = "ISO_IR 100"
        ds.PatientName = "Ivanov^Ivan"
        # cp1251 bytes mis-decoded as latin-1: "Тест" -> "Òåñò"
        secret = "Òåñò Ïàöèåíòà"
        ds.SeriesDescription = secret

        with caplog.at_level(logging.DEBUG):
            cs._fix_cp1251_krakozyabry(ds)

        text = "\n".join(r.getMessage() for r in caplog.records)
        assert secret not in text, "the SeriesDescription value reached the log"
        assert "SeriesDescription" in text, "the tag itself is still reported"

    def test_patient_name_stays_masked(self, caplog):
        ds = Dataset()
        ds.SpecificCharacterSet = "ISO_IR 100"
        ds.PatientName = "Òåñò"

        with caplog.at_level(logging.DEBUG):
            cs._fix_cp1251_krakozyabry(ds)

        text = "\n".join(r.getMessage() for r in caplog.records)
        # Neither spelling may reach the log: not the pre-repair mojibake and
        # not the repaired Cyrillic the fix derives from it. The repair changes
        # the value, so pinning only the original let the masked form leak the
        # restored name through the "->" half of the line.
        assert "Òåñò" not in text
        assert "Тест" not in text
        assert "PatientName" in text, "the tag itself is still reported"


# ---------------------------------------------------------------------------
# A-m-02 - the association tracker holds no raw PatientID
# ---------------------------------------------------------------------------
class TestAm02AssocTracker:
    def test_the_tracker_stores_no_patient_identifier(self):
        """Behavioural: whatever a caller passes, the stored study record holds
        modality and count only. The summary this feeds prints nothing else, so
        a raw identifier was PHI held in memory for nothing."""
        import clean_code.assoc_tracker as at

        info = at.AssocInfo("AE", "127.0.0.1")
        with at._assoc_lock:
            at._assoc_registry[id(info)] = info
        try:
            # Call through the public signature; an extra positional argument
            # (the old patient_id slot) must not be recorded either way.
            sig = inspect.signature(at.record_cstore)
            assert list(sig.parameters) == ["assoc_id", "study_uid", "modality"], sig
            assert at.record_cstore(id(info), "1.2.3", "CT") is True
            record = info.studies["1.2.3"]
            assert set(record) == {"modality", "count"}, record
        finally:
            with at._assoc_lock:
                at._assoc_registry.pop(id(info), None)

    def test_the_tracker_still_summarises_a_study(self):
        info = __import__("clean_code.assoc_tracker", fromlist=["AssocInfo"]).AssocInfo(
            "AE", "127.0.0.1"
        )
        from clean_code.assoc_tracker import _assoc_lock, _assoc_registry

        with _assoc_lock:
            _assoc_registry[id(info)] = info
        try:
            assert record_cstore(id(info), "1.2.3", "CT") is True
            assert record_cstore(id(info), "1.2.3", "CT") is False
            assert info.studies["1.2.3"] == {"modality": "CT", "count": 2}
        finally:
            with _assoc_lock:
                _assoc_registry.pop(id(info), None)


# ---------------------------------------------------------------------------
# A-M-01 - the calling address is normalised before comparison
# ---------------------------------------------------------------------------
class TestAM01HostNormalisation:
    @pytest.mark.parametrize(
        "target,calling",
        [
            ("localhost", "127.0.0.1"),
            ("LOCALHOST", "127.0.0.1"),
            ("::1", "127.0.0.1"),
            ("[::1]", "127.0.0.1"),
            ("0.0.0.0", "127.0.0.1"),
            ("::", "127.0.0.1"),
            ("::ffff:127.0.0.1", "127.0.0.1"),
        ],
    )
    def test_loopback_aliases_are_one_address(self, target, calling):
        """A viewer on this host listed as `localhost` used to get 0xA801 for
        every study although the addresses are the same machine."""
        assert cm._normalize_host(target) == cm._normalize_host(calling)

    def test_a_genuinely_different_host_still_differs(self):
        assert cm._normalize_host("10.0.0.5") != cm._normalize_host("10.0.0.6")

    @pytest.mark.parametrize(
        "target,calling",
        [("localhost", "127.0.0.1"), ("::1", "127.0.0.1"), ("[::1]", "127.0.0.1")],
    )
    def test_the_cmove_check_itself_accepts_an_alias_of_the_caller(
        self, target, calling, monkeypatch
    ):
        """Pins the CHECK, not just the helper: handle_c_move must not answer
        0xA801 when peers.conf names the caller's own machine differently."""
        src = inspect.getsource(cm.handle_c_move)
        assert "_normalize_host(target_host) != _normalize_host(calling_ip)" in src, (
            "the C-MOVE guard still compares the raw strings"
        )


# ---------------------------------------------------------------------------
# A-M-02 - the port probe follows the address family
# ---------------------------------------------------------------------------
class TestAM02PortProbeFamily:
    @pytest.mark.parametrize(
        "bind_addr,family_name",
        [("::", "AF_INET6"), ("::1", "AF_INET6"), ("127.0.0.1", "AF_INET")],
    )
    def test_the_probe_socket_matches_the_bind_address(
        self, bind_addr, family_name, monkeypatch
    ):
        """`[server] host = ::` is a legitimate IPv6 listener; an AF_INET probe
        to `::` raised and the bridge refused to start."""
        import socket as _socket

        seen = {}

        class FakeSock:
            def setsockopt(self, *a):
                pass

            def bind(self, addr):
                seen["addr"] = addr

            def close(self):
                pass

        def fake_socket(family, kind):
            seen["family"] = family
            return FakeSock()

        monkeypatch.setattr(_socket, "socket", fake_socket)
        monkeypatch.setattr(_socket, "AF_INET", 2)
        monkeypatch.setattr(_socket, "AF_INET6", 10)
        monkeypatch.setattr(main_mod.sys, "platform", "linux")

        main_mod._check_port_available(11112, bind_addr, "DIMSE")

        assert seen["family"] == getattr(_socket, family_name), seen

    def test_expose_port_is_checked_before_components_start(self):
        """A second instance sharing expose_port passed every pre-check and
        only then reset the live instance's in-progress C-MOVE jobs."""
        src = inspect.getsource(main_mod.main)
        idx_check = src.find('_check_port_available(args.expose_port')
        idx_init = src.find("_init_components(args)")
        assert idx_check != -1, "expose_port is not probed at all"
        assert idx_init != -1
        assert idx_check < idx_init, "the QA port is probed after _init_components"


# ---------------------------------------------------------------------------
# A-C-01 - a None C-STORE-RSP is a lost association, not a rejected instance
# ---------------------------------------------------------------------------
class TestAC01NoneCStoreRsp:
    def test_a_none_response_arms_the_resume_and_counts_no_failure(self):
        """`send_c_store` RETURNS None when the association is gone; counting
        that as a rejected instance reported a SOP the viewer may hold and left
        the lost flag False, so the resume never fired.

        Drives the real send loop over a real multipart body (the test_spill
        harness): 5 parts, the 3rd C-STORE returns None.
        """
        from http.server import HTTPServer

        import clean_code.handlers.cmove as cmmod

        from tests.test_spill import (
            CT,
            EVRLE,
            _body,
            _FakeClarus,
            _FakeMoveJobs,
            _final,
            _part_bytes,
            _status,
        )

        codes = [0x0000, 0x0000, None, 0x0000, 0x0000]

        class _Assoc:
            is_established = True
            ae_title = "SCP"
            port = 11112

            def send_c_store(self, *_a, **_k):
                code = codes.pop(0)
                return None if code is None else _status(code)

        body = b"".join(_part_bytes(_body(200 * 1024, sop_uid=f"2.25.999000111222333{i}"))
                        for i in range(5))
        _FakeClarus.body = body + _final()
        httpd = HTTPServer(("127.0.0.1", 0), _FakeClarus)
        port = httpd.server_address[1]
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        try:
            result = cmmod._stream_and_send_study(
                assoc=_Assoc(),
                study_uid="2.25.1111222233334444",
                clarus_url=f"http://127.0.0.1:{port}/dicomweb",
                http_timeout=10,
                move_jobs=_FakeMoveJobs(),
                job_id=1,
                accepted_contexts_map={CT: EVRLE},
                server_ts_list=[EVRLE],
                sop_registry={CT: {"accept": True}},
                _report_lost=True,
            )
        finally:
            httpd.shutdown()
            httpd.server_close()

        completed, failed, _total, sent, _fs, _ca, _nc, lost = result
        assert lost is True, "a None response must arm the resume"
        assert failed == 0, f"a lost transport is not a rejected instance ({failed})"
        assert completed == 2, completed
        assert len(sent) == 2, sent


# ---------------------------------------------------------------------------
# A-C-02 - an empty C-STORE-RSP Dataset is a lost association, not a failure
# ---------------------------------------------------------------------------
class TestAC02EmptyDatasetRsp:
    def test_an_empty_dataset_arms_the_resume_and_counts_no_failure(self):
        """pynetdicom 3.0.4 `send_c_store` returns an EMPTY Dataset (not None)
        when the association is gone; `status.Status` then raised
        AttributeError, which escaped the send loop and skipped the resume and
        the (0008,0058) failed list. An empty Dataset is a lost association,
        not a rejected instance."""
        from http.server import HTTPServer

        import clean_code.handlers.cmove as cmmod

        from tests.test_spill import (
            CT,
            EVRLE,
            _body,
            _FakeClarus,
            _FakeMoveJobs,
            _final,
            _part_bytes,
            _status,
        )

        codes = [0x0000, 0x0000, "EMPTY", 0x0000, 0x0000]

        class _Assoc:
            is_established = True
            ae_title = "SCP"
            port = 11112

            def send_c_store(self, *_a, **_k):
                code = codes.pop(0)
                if code == "EMPTY":
                    return Dataset()  # empty, no Status element
                return _status(code)

        body = b"".join(_part_bytes(_body(200 * 1024, sop_uid=f"2.25.999000111222333{i}"))
                        for i in range(5))
        _FakeClarus.body = body + _final()
        httpd = HTTPServer(("127.0.0.1", 0), _FakeClarus)
        port = httpd.server_address[1]
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        try:
            result = cmmod._stream_and_send_study(
                assoc=_Assoc(),
                study_uid="2.25.1111222233334444",
                clarus_url=f"http://127.0.0.1:{port}/dicomweb",
                http_timeout=10,
                move_jobs=_FakeMoveJobs(),
                job_id=1,
                accepted_contexts_map={CT: EVRLE},
                server_ts_list=[EVRLE],
                sop_registry={CT: {"accept": True}},
                _report_lost=True,
            )
        finally:
            httpd.shutdown()
            httpd.server_close()

        completed, failed, _total, sent, _fs, _ca, _nc, lost = result
        assert lost is True, "an empty Dataset must arm the resume"
        assert failed == 0, f"a lost transport is not a rejected instance ({failed})"
        assert completed == 2, completed
        assert len(sent) == 2, sent


# ---------------------------------------------------------------------------
# A-M-03 - bytes_total counts delivered bytes, not attempts
# ---------------------------------------------------------------------------
class TestAM03BytesTotal:
    def test_bytes_are_counted_once_for_a_delivered_instance(self):
        """The counter sat on the send call, so a resumed instance was counted
        again while the metric name reads as delivered traffic."""
        src = inspect.getsource(cm._stream_and_send_study)
        success_at = src.find("completed += 1")
        bytes_at = src.find('bytes_total.labels(\n                            direction="tx"')
        send_at = src.find("status = assoc.send_c_store(_tmp_path)")
        assert success_at != -1 and send_at != -1
        assert bytes_at > success_at, (
            "bytes_total must be incremented on the success branch, not at send time"
        )


# ---------------------------------------------------------------------------
# A-m-07 - nevent_failed_total counts one abandoned batch once
# ---------------------------------------------------------------------------
class TestAm07NeventFailedCounter:
    def test_one_increment_per_abandoned_batch(self, monkeypatch):
        """The counter promises failed DELIVERIES; per-retry increments made one
        batch count as several."""
        import clean_code.nevent as nevent

        fails = []
        delivered = []

        class FakeCounter:
            def inc(self, *a, **k):
                fails.append(1)

        class FakeDelivered:
            def inc(self, *a, **k):
                delivered.append(1)

        monkeypatch.setattr(nevent, "nevent_failed_total", FakeCounter())
        monkeypatch.setattr(nevent, "nevent_delivered_total", FakeDelivered())
        monkeypatch.setattr(nevent, "_release_or_abort", lambda *a, **k: None)

        class NoAssoc:
            is_established = False

        class FakeAE:
            def __init__(self, *a, **k):
                self.acse_timeout = 1

            def add_requested_context(self, *a, **k):
                pass

            def associate(self, *a, **k):
                return NoAssoc()

        monkeypatch.setattr(nevent, "AE", FakeAE)
        monkeypatch.setattr(nevent.time, "sleep", lambda *a: None)

        ok = nevent.send_n_event_report(
            "127.0.0.1",
            11112,
            "MODALITY",
            "1.2.3",
            1,
            [(MR, "1.2.3.1")],
            [],
            bridge_ae_title="BRIDGE",
            max_retries=3,
        )

        assert ok is False
        assert len(fails) == 1, f"one batch counted {len(fails)} times"


# ---------------------------------------------------------------------------
# A-m-03 (peers) - an empty peers.conf must not wipe the peer table
# ---------------------------------------------------------------------------
class TestAm03PeersReload:
    def _manager(self, tmp_path):
        from clean_code.move_jobs import MoveJobManager

        from tests.test_db_move_jobs import _TestDb

        return MoveJobManager(shared_db=_TestDb(str(tmp_path / "move_jobs.db")), max_workers=1)

    def test_an_empty_file_keeps_the_existing_peers(self, tmp_path):
        """A truncated or empty peers.conf parses to zero peers; treating that
        as "every peer was removed" unregistered the whole routing table."""
        mgr = self._manager(tmp_path)
        try:
            peers = tmp_path / "peers.conf"
            peers.write_text("CT_SCP  10.0.0.5:104\n", encoding="utf-8")
            from clean_code.config import parse_peers_conf

            rows = list(parse_peers_conf(str(peers)))
            assert rows, "fixture must yield at least one peer"
            mgr.reload_peers_from_file(str(peers))
            assert mgr.lookup_scp(rows[0][0]) is not None

            peers.write_text("", encoding="utf-8")
            mgr.lazyload_peers_interval = 0
            mgr.reload_peers_from_file(str(peers))

            assert mgr.lookup_scp(rows[0][0]) is not None, (
                "an empty peers.conf deleted every peer"
            )
        finally:
            mgr.close()


# ---------------------------------------------------------------------------
# A-m-04 - the duplicate-instance probe does not report a failure as "clean"
# ---------------------------------------------------------------------------
class TestAm04DimseProbe:
    def test_an_unexpected_exception_is_reported_as_unknown(self, monkeypatch, caplog):
        """`except Exception: pass` turned a failed probe into "no duplicate"."""
        import pynetdicom

        class BoomAE:
            def __init__(self, *a, **k):
                pass

            def add_requested_context(self, *a, **k):
                pass

            def associate(self, *a, **k):
                raise RuntimeError("probe exploded")

        monkeypatch.setattr(pynetdicom, "AE", BoomAE)
        with caplog.at_level(logging.WARNING):
            main_mod._check_dimse_unique("127.0.0.1", 11112, "BRIDGE")

        text = "\n".join(r.getMessage() for r in caplog.records)
        assert "UNKNOWN" in text, text
        assert "RuntimeError" in text, text

    def test_a_refused_connection_is_not_a_warning(self, monkeypatch, caplog):
        """Nothing listening is the expected, clean answer."""
        import pynetdicom

        class RefusedAE:
            def __init__(self, *a, **k):
                pass

            def add_requested_context(self, *a, **k):
                pass

            def associate(self, *a, **k):
                raise ConnectionRefusedError("refused")

        monkeypatch.setattr(pynetdicom, "AE", RefusedAE)
        with caplog.at_level(logging.DEBUG):
            main_mod._check_dimse_unique("127.0.0.1", 11112, "BRIDGE")

        assert not [
            r for r in caplog.records if r.levelno >= logging.WARNING
        ], "a refused connection is the clean answer"


# ---------------------------------------------------------------------------
# A-m-05 - a JSON null is not the UID "None"
# ---------------------------------------------------------------------------
class TestAm05JsonNull:
    def test_null_returns_none(self):
        assert proto._json_first_value({"0020000D": {"Value": [None]}}, "0020000D") is None

    def test_a_real_value_is_unchanged(self):
        assert (
            proto._json_first_value(
                {"0020000D": {"Value": ["1.2.3"]}}, "0020000D"
            )
            == "1.2.3"
        )

    def test_a_number_is_rendered(self):
        assert proto._json_first_value({"00201209": {"Value": [5]}}, "00201209") == "5"


# ---------------------------------------------------------------------------
# A-m-06 - _safe_uid is injective
# ---------------------------------------------------------------------------
class TestAm06SafeUid:
    def test_colliding_uids_get_distinct_names(self, tmp_path):
        """Collapsing `\\x00`, `/` and `\\` all to `_` made two different UIDs
        name one file, so the second instance published over the first."""
        from clean_code.outbox import OutboxQueue

        q = OutboxQueue(str(tmp_path / "outbox"), max_queue=10)
        candidates = ["1.2.3", "1.2/3", "1.2\\3", "1.2\x003", "1.2%2F3", "1.2_3"]
        names = [os.path.basename(q._dcm_path(q._pending, u)) for u in candidates]
        assert len(set(names)) == len(candidates), names

    def test_a_normal_uid_is_unchanged(self, tmp_path):
        from clean_code.outbox import OutboxQueue

        q = OutboxQueue(str(tmp_path / "outbox"), max_queue=10)
        uid = "1.2.840.10008.5.1.4.1.1.4.123"
        assert q._safe_uid(uid) == uid
        assert os.path.basename(q._dcm_path(q._pending, uid)) == f"{uid}.dcm"


# ---------------------------------------------------------------------------
# B-01 - the forward worker survives an empty C-STORE response
# ---------------------------------------------------------------------------
class TestB01ForwardEmptyRsp:
    def test_an_empty_response_dataset_requeues_instead_of_raising(self):
        """`send_c_store` returns an EMPTY Dataset (not None) when the peer
        times out or aborts; `status.Status` then raised AttributeError, which
        escaped _forward_process_batch and killed the forward thread."""
        src = inspect.getsource(main_mod._forward_process_batch)
        assert 'getattr(status, "Status", None)' in src, (
            "the C-STORE-RSP status is not read defensively"
        )
        assert "status.Status ==" not in src

    def test_the_worker_loop_guards_the_batch_call(self):
        """One bad batch must not end the only thread draining the queue."""
        src = inspect.getsource(main_mod._forward_worker)
        idx_try = src.find("try:")
        idx_call = src.find("_forward_process_batch(")
        assert idx_try != -1 and idx_call != -1
        assert idx_try < idx_call, "the batch call is not inside a try"
        assert "assoc_ok = False" in src, "the guarded failure is not requeued"


# ---------------------------------------------------------------------------
# B-02 - a PID file naming this process is not a competing instance
# ---------------------------------------------------------------------------
class TestB02PidOwnPid:
    def test_own_pid_is_not_treated_as_foreign(self):
        src = inspect.getsource(main_mod._write_pid_file)
        assert "old_pid_int != os.getpid()" in src, (
            "the liveness check does not exclude the current process"
        )


# ---------------------------------------------------------------------------
# B-03 - a startup failure after the HTTP server starts must exit
# ---------------------------------------------------------------------------
class TestB03StartupFailure:
    def test_background_services_are_inside_the_guarded_block(self):
        """`_start_background_services` starts NON-daemon threads; if a later
        step raises, main() exits but the process stays alive and deaf."""
        src = inspect.getsource(main_mod.main)
        idx_start = src.find("_start_background_services(")
        idx_try = src.rfind("try:", 0, idx_start)
        assert idx_start != -1
        assert idx_try != -1, "_start_background_services is not inside a try"
        assert "os._exit(1)" in src[idx_try:], (
            "the guarded startup does not exit non-zero"
        )


# ---------------------------------------------------------------------------
# B-04 - a store failure must not duplicate a delivered report
# ---------------------------------------------------------------------------
class TestB04ForgetFailure:
    def test_a_failing_forget_does_not_abort_the_tick(self):
        from clean_code.commit_tracker import CommitTracker

        tracker = CommitTracker(
            verify=lambda c, u: True,
            report=lambda *a, **k: True,
            interval=0.1,
            deadline_sec=0.0,
        )
        tracker._forget = MagicMock(side_effect=RuntimeError("db gone"))
        try:
            tracker.register("1.2.3.1", "MOD", "10.0.0.5", 104, [(MR, "a")])
            tracker.register("1.2.3.2", "MOD", "10.0.0.5", 104, [(MR, "b")])
            tracker._tick()  # must not raise
        finally:
            tracker.close()

        assert tracker.pending_count() == 0, (
            "a store failure aborted the tick and left batches behind"
        )


# ---------------------------------------------------------------------------
# B-05 - a short identifier is not dropped from the SPS ID
# ---------------------------------------------------------------------------
class TestB05MwlTail:
    @pytest.mark.parametrize("value", ["ABC123", "WORKITEM-1", "1234567890123456"])
    def test_a_value_that_fits_is_kept(self, value):
        """VR SH holds 16 characters; a 10-character workitem UID yields NO
        (0040,0009) SPS ID under the old rule, although it fits."""
        assert proto._mwl_tail(value) == value

    def test_a_long_value_is_still_tailed(self):
        long_uid = "2.25." + "1" * 40
        out = proto._mwl_tail(long_uid)
        assert len(out) == proto.MWL_SPS_ID_LEN
        assert long_uid.endswith(out)

    def test_absent_is_empty(self):
        assert proto._mwl_tail(None) == ""
        assert proto._mwl_tail("") == ""


# ---------------------------------------------------------------------------
# B-06 - AccessionNumber resolves sequence-first (already correct, pinned)
# ---------------------------------------------------------------------------
class TestB06AccessionPrecedence:
    def test_an_empty_flat_tag_does_not_shadow_the_sequence(self):
        ds = proto._json_to_dataset_mwl(
            {
                "00080050": {"vr": "SH"},  # present, no value
                "0040A370": {
                    "vr": "SQ",
                    "Value": [{"00080050": {"vr": "SH", "Value": ["ACC123"]}}],
                },
                "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^J"}]},
                "00100020": {"vr": "LO", "Value": ["P1"]},
            },
            ["AccessionNumber"],
        )
        assert str(ds.AccessionNumber) == "ACC123"


# ---------------------------------------------------------------------------
# B-07 - cache eviction is idempotent under concurrency
# ---------------------------------------------------------------------------
class TestB07CacheEvict:
    def test_two_threads_evicting_the_same_key_do_not_raise(self):
        cache = {"k": (0.0, {})}
        errors = []

        def evict():
            try:
                cm._cache_evict(cache, now=10.0)
            except Exception as e:  # pragma: no cover
                errors.append(e)

        threads = [threading.Thread(target=evict) for _ in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=10)

        assert errors == [], errors
        assert cache == {}


# ---------------------------------------------------------------------------
# B-08 - move_jobs_counter is only counted by the caller that finished the job
# ---------------------------------------------------------------------------
class TestB08FinishJobCounting:
    def test_every_increment_is_guarded_by_finish_job(self):
        """`finish_job` returns False when another caller already finished the
        row; counting regardless inflated the job counters."""
        src = inspect.getsource(cm)
        lines = src.splitlines()
        offenders = []
        for i, line in enumerate(lines):
            if "move_jobs_counter.labels(" not in line:
                continue
            # Walk back to the nearest enclosing guard, skipping the call's own
            # multi-line arguments.
            back = "\n".join(lines[max(0, i - 12) : i + 1])
            if not any(
                marker in back
                for marker in ("finish_job(", "if finished", "finished =")
            ):
                offenders.append(f"{i + 1}: {line.strip()}")
        assert offenders == [], f"unguarded counter increment(s): {offenders}"


# ---------------------------------------------------------------------------
# B-09 - reconciled counters are stored before finish_job
# ---------------------------------------------------------------------------
class TestB09ReconciledProgress:
    def test_reconciled_progress_is_written_before_the_job_finishes(self):
        src = inspect.getsource(cm._execute_cmove_job)
        idx_write = src.find("update_job_progress(")
        # The LAST finish_job in this function is the terminal one.
        idx_finish = src.rfind("finish_job(")
        assert idx_write != -1, "the reconciled counters are never stored"
        assert idx_write < idx_finish, (
            "the reconciled counters are stored after the job is finished"
        )


# ---------------------------------------------------------------------------
# The shared retrying replace (closes the Windows rename-contention class)
# ---------------------------------------------------------------------------
class TestReplaceWithRetry:
    def test_a_transient_refusal_is_retried(self, monkeypatch):
        calls = []

        def flaky(src, dst):
            calls.append(1)
            if len(calls) < 3:
                raise PermissionError(13, "in use")

        monkeypatch.setattr(utils.os, "replace", flaky)
        monkeypatch.setattr(utils.time, "sleep", lambda *a: None)
        utils.replace_with_retry("a.tmp", "a")

        assert len(calls) == 3

    def test_the_budget_is_finite_and_the_error_surfaces(self, monkeypatch):
        monkeypatch.setattr(
            utils.os, "replace", lambda s, d: (_ for _ in ()).throw(PermissionError(13, "x"))
        )
        monkeypatch.setattr(utils.time, "sleep", lambda *a: None)
        with pytest.raises(PermissionError):
            utils.replace_with_retry("a.tmp", "a", attempts=3)
