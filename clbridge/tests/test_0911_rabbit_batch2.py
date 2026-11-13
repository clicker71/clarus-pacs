# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0911 rabbit batch 2 regression tests - one falsifiable test per fix.

Each test pins one net-new finding (N1-N10) so that reverting the fix makes
the test fail. The tests read the modules directly (no live DICOM peer).
"""

import logging
import os
import threading

import clean_code.handlers.cfind as cf
import clean_code.handlers.cmove as cm
import httpx
import pytest


def _counter_delta(getter, action):
    before = getter()
    action()
    return getter() - before


# ---------------------------------------------------------------------------
# N2 - commit tracker: a verdict that cannot be delivered is abandoned
# ---------------------------------------------------------------------------
class TestN2CommitDeliveryCap:
    def test_commit_resolve_capped(self, caplog):
        """A batch whose N-EVENT-REPORT always fails leaves the pending set
        after MAX_REPORT_ATTEMPTS ticks and is logged as given up.

        Unbounded retry let ONE unreachable peer hold the single commit worker
        forever: each attempt burns the acse/network/dimse timeouts, so every
        other peer's verdict waited behind it."""
        from clean_code.commit_tracker import MAX_REPORT_ATTEMPTS, CommitTracker

        tracker = CommitTracker(
            verify=lambda sop_class, sop_uid: False,  # never confirmed
            report=lambda *a, **k: False,  # never delivered
            interval=0.1,
            deadline_sec=0.0,  # resolved on the first tick
        )
        try:
            tracker.register(
                "1.2.3.40", "MODALITY", "10.0.0.5", 104, [("1.2.3", "1.2.3.1")]
            )
            with caplog.at_level(logging.WARNING):
                for _ in range(MAX_REPORT_ATTEMPTS - 1):
                    tracker._tick()
                    assert tracker.pending_count() == 1, "bounded retries still pending"
                tracker._tick()
        finally:
            tracker.close()

        assert tracker.pending_count() == 0, "the batch is abandoned after the cap"
        text = "\n".join(r.message for r in caplog.records)
        assert "giving up" in text, text
        assert "1.2.3.40" in text, "the abandoned transaction is named in the log"

    def test_commit_delivered_batch_retires_on_first_tick(self):
        """The cap must not delay a deliverable batch: one successful
        N-EVENT-REPORT retires it immediately."""
        from clean_code.commit_tracker import CommitTracker

        tracker = CommitTracker(
            verify=lambda sop_class, sop_uid: True,
            report=lambda *a, **k: True,
            interval=0.1,
            deadline_sec=300.0,
        )
        try:
            tracker.register(
                "1.2.3.41", "MODALITY", "10.0.0.5", 104, [("1.2.3", "1.2.3.2")]
            )
            tracker._tick()
        finally:
            tracker.close()
        assert tracker.pending_count() == 0


# ---------------------------------------------------------------------------
# N4 - commit tracker: the verdict counters describe outcomes, not attempts
# ---------------------------------------------------------------------------
class TestN4NactionCountsOnce:
    def _success_counter(self):
        from clean_code import metrics as m

        return m.naction_total.labels(verdict="success")

    def test_naction_counts_only_a_delivered_verdict(self):
        """Two failed deliveries followed by a success bumps the counter
        exactly once, by len(ok).

        Counting before the report charged every retry of the SAME batch, so
        the metric described attempts while the batch had no outcome yet."""
        from clean_code.commit_tracker import CommitTracker

        counter = self._success_counter()
        before = counter._value.get()
        outcomes = [False, False, True]

        tracker = CommitTracker(
            verify=lambda sop_class, sop_uid: True,
            report=lambda *a, **k: outcomes.pop(0),
            interval=0.1,
            # The retry loop must not be cut short by the deadline: with a
            # zero deadline the job would be past it on the first attempt and
            # the batch would be parked before the third report could succeed.
            deadline_sec=300.0,
        )
        try:
            tracker.register(
                "1.2.3.42",
                "MODALITY",
                "10.0.0.5",
                104,
                [("1.2.3", "1.2.3.3"), ("1.2.3", "1.2.3.4")],
            )
            tracker._tick()
            tracker._tick()
            assert tracker.pending_count() == 1, "an undelivered verdict stays pending"
            tracker._tick()
        finally:
            tracker.close()

        assert tracker.pending_count() == 0
        assert (
            counter._value.get() - before == 2
        ), "exactly len(ok) counted once, after the verdict was delivered"


# ---------------------------------------------------------------------------
# N3 - C-MOVE reconcile: a degraded resumed job must not claim 0 losses
# ---------------------------------------------------------------------------
STUDY = "2.25.111122223333"
MR = "1.2.840.10008.5.1.4.1.1.4"


class _StubJobs:
    """MoveJobManager stand-in: records only what _execute_cmove_job writes."""

    def __init__(self, ops):
        self._ops = ops

    def finish_job(self, job_id, success=False, error_message=None):
        self._ops["finished"] = (job_id, success, error_message)
        return True

    def record_scp_success(self, *a, **k):
        self._ops["scp_ok"] = self._ops.get("scp_ok", 0) + 1

    def record_scp_failure(self, *a, **k):
        self._ops["scp_fail"] = self._ops.get("scp_fail", 0) + 1

    def set_job_failed_sops(self, job_id, sops):
        self._ops["failed_sops"] = sops

    def update_job_progress(self, *a, **k):
        pass

    def is_job_cancelled(self, job_id):
        return False


def _noop_counter():
    class _L:
        def inc(self, *a, **k):
            pass

    class _C:
        def labels(self, *a, **k):
            return _L()

    return _C()


def _run_execute(monkeypatch, streams, study_sops, assoc_calls=None, contexts=None):
    """Drive _execute_cmove_job with the network legs stubbed.

    `streams` is the sequence of _stream_and_send_study results, one per pass.
    `assoc_calls` collects the outcomes of each _associate_for_move call so a
    test can fail the SECOND one without touching the first.
    `contexts` is the accepted presentation-context list of the stub
    association; empty by default, which keeps the Storage Commitment push
    out of the way unless a test asks for it.
    """
    state = {"assoc": 0}
    assoc_calls = assoc_calls if assoc_calls is not None else []
    contexts = contexts if contexts is not None else []

    def fake_associate(*a, **k):
        state["assoc"] += 1
        idx = state["assoc"]
        outcome = assoc_calls[idx - 1] if idx <= len(assoc_calls) else "ok"
        if outcome == "fail":
            raise RuntimeError("association refused")
        return (
            type("A", (), {"is_established": True, "accepted_contexts": contexts})(),
            {},
        )

    def fake_stream(*a, **k):
        return streams.pop(0)

    ops = {}
    monkeypatch.setattr(cm, "_associate_for_move", fake_associate)
    monkeypatch.setattr(cm, "_preflight_study_ts_by_class", lambda *a, **k: {})
    monkeypatch.setattr(cm, "_study_sop_uids", lambda *a, **k: study_sops)
    monkeypatch.setattr(cm, "_get_clarus_transfer_syntaxes", lambda *a, **k: [])
    monkeypatch.setattr(cm, "_stream_and_send_study", fake_stream)
    monkeypatch.setattr(cm, "move_jobs_counter", _noop_counter())

    cm._execute_cmove_job(
        99,
        STUDY,
        "SCP",
        _StubJobs(ops),
        "http://server/dicomweb",
        5,
        "BRIDGE",
        target_host="127.0.0.1",
        target_port=11112,
        sop_registry={MR: {"accept": True}},
    )
    return ops


class TestN3ResumeFloor:
    def test_degraded_resume_counts_pass2_total(self, monkeypatch):
        """No authoritative SOP list + a resumed pass that saw more instances
        than pass 1 -> the job reports losses instead of 0x0000.

        Pass 1 observed 3 instances and delivered 3 before the association
        dropped; the resumed pass observed the whole study (5) and delivered
        the remaining 2 but was lost again. The study is fully accounted for,
        yet the point of the test is the DEGRADED arithmetic: `total` must
        take pass 2 into account, so a resumed job that ends short cannot
        answer "no failures"."""
        streams = [
            (3, 0, 3, [(MR, "u1"), (MR, "u2"), (MR, "u3")], [], False, False, True),
            (1, 1, 5, [(MR, "u4")], [], False, False, True),
        ]
        ops = _run_execute(monkeypatch, streams, study_sops=None)

        assert ops["finished"][1] is False, ops["finished"]
        assert ops["finished"][2] != "0 sent, 0 failed", ops["finished"]

    def test_degraded_resume_with_zero_accounted_failures(self, monkeypatch):
        """GIVE_UP with no authoritative list and an empty failed tail still
        yields at least one failure: the study was cut short, so 0x0000 is
        not an answer the bridge can support (PS3.4 C.4.2.1.4.2)."""
        # Both passes deliver everything they were asked for; the LOSS is the
        # only evidence something was left behind.
        streams = [
            (1, 0, 1, [(MR, "u1")], [], False, False, True),
            (0, 0, 0, [], [], False, False, True),
        ]
        ops = _run_execute(monkeypatch, streams, study_sops=None)

        assert ops["finished"][1] is False, ops["finished"]
        assert "1 failed" in (ops["finished"][2] or ""), ops["finished"]

    def test_storage_commitment_skipped_when_association_is_gone(self, monkeypatch):
        """A failed re-association leaves `assoc` None; the Storage Commitment
        block must not reach `accepted_contexts` through it.

        The first pass delivered its whole window, the authoritative list holds
        exactly that one SOP and the resume failed: the counters show no loss,
        so control DOES reach the SC block - and the unguarded version raises
        AttributeError out of the worker, discarding the finished job."""
        sc_calls = []
        monkeypatch.setattr(
            cm, "_send_storage_commitment", lambda *a, **k: sc_calls.append(a)
        )

        streams = [(1, 0, 1, [(MR, "u1")], [], False, False, True)]
        ops = _run_execute(
            monkeypatch, streams, study_sops=["u1"], assoc_calls=["ok", "fail"]
        )

        assert sc_calls == [], "no N-ACTION without a live association"
        assert "finished" in ops, "the job must still reach its terminal state"
        assert ops["failed_sops"] == [], ops["failed_sops"]


# ---------------------------------------------------------------------------
# N1 - C-FIND: the reading physician is PHI and must be masked in the journal
# ---------------------------------------------------------------------------
class TestN1MaskReadingPhysician:
    def test_masked_query_hides_the_reading_physician(self):
        """NameOfPhysiciansReadingStudy (0008,1060 PN) is a QIDO keyword, so
        the tag list alone never matched it and the physician's name reached
        the HTTP-error log.

        PS3.18 Table 6.7.1-1: a PN query key carries patient data, and the
        reading physician's name identifies the study just as well."""
        out = cf._masked_query(
            {"NameOfPhysiciansReadingStudy": "Dr X", "Modality": "CT"}
        )
        assert "NameOfPhysiciansReadingStudy=***" in out, out
        assert "Dr X" not in out
        assert "Modality=CT" in out, "a non-PHI key is still shown verbatim"

    def test_masked_query_still_hides_the_same_tag(self):
        """The tag form was already masked and must stay masked."""
        assert cf._masked_query({"00081060": "Dr X"}) == "00081060=***"


# ---------------------------------------------------------------------------
# N5 - C-FIND series search: a transport break is transient
# ---------------------------------------------------------------------------
def _series_event():
    from clean_code.protocol import STUDY_ROOT_UID

    from tests.conftest import make_fake_find_event

    event = make_fake_find_event(model_uid=STUDY_ROOT_UID)
    event.identifier.QueryRetrieveLevel = "SERIES"
    return event


class TestN5SeriesSearchRetryable:
    def test_step_a_transport_failure_is_a700(self, monkeypatch, mock_shutdown_not_set):
        """The study step of the two-step series search answers 0xA700 on a
        TCP break: 0xC000 is a permanent refusal the SCU never retries."""
        event = _series_event()

        def boom(*a, **k):
            raise httpx.ConnectError("refused")

        monkeypatch.setattr(cf, "_qido_request", boom)
        results = list(
            cf.handle_c_find(event, "http://127.0.0.1:8024/dicomweb", False, 50)
        )
        assert results == [(0xA700, None)], results

    def test_step_b_transport_failure_is_a700(self, monkeypatch, mock_shutdown_not_set):
        """The per-study series step answers 0xA700 on the same break."""
        event = _series_event()
        calls = {"n": 0}

        def flaky(*a, **k):
            calls["n"] += 1
            if calls["n"] == 1:
                return [{"0020000D": {"vr": "UI", "Value": ["1.2.3.4"]}}]
            raise httpx.ConnectError("refused")

        monkeypatch.setattr(cf, "_qido_request", flaky)
        results = list(
            cf.handle_c_find(event, "http://127.0.0.1:8024/dicomweb", False, 50)
        )
        assert results == [(0xA700, None)], results

    def test_step_b_http_status_stays_a_permanent_refusal(
        self, monkeypatch, mock_shutdown_not_set
    ):
        """A 4xx/5xx ANSWER is not a transport break: it keeps 0xC000, so the
        transient/permanent split stays where the batch-1 fix put it."""
        event = _series_event()
        calls = {"n": 0}

        def flaky(*a, **k):
            calls["n"] += 1
            if calls["n"] == 1:
                return [{"0020000D": {"vr": "UI", "Value": ["1.2.3.4"]}}]
            request = httpx.Request("GET", "http://x/studies/1.2.3.4/series")
            response = httpx.Response(400, request=request)
            raise httpx.HTTPStatusError(
                "bad request", request=request, response=response
            )

        monkeypatch.setattr(cf, "_qido_request", flaky)
        results = list(
            cf.handle_c_find(event, "http://127.0.0.1:8024/dicomweb", False, 50)
        )
        assert results == [(0xC000, None)], results


# ---------------------------------------------------------------------------
# N6 - /print: a mistyped field is a JSON 400, not a dropped connection
# ---------------------------------------------------------------------------
def _print_handler(payload):
    """A BridgeHTTPHandler wired far enough for do_POST to reach the body."""
    import json as _json

    from clean_code.http_server import BridgeHTTPHandler

    body = _json.dumps(payload).encode("utf-8")
    h = BridgeHTTPHandler.__new__(BridgeHTTPHandler)
    h.path = "/print"
    h.headers = {
        "Content-Type": "application/json",
        "Content-Length": str(len(body)),
    }
    h.rfile = type("R", (), {"read": lambda self, n: body})()
    captured = {}
    h._json_error = lambda code, msg: captured.update(code=code, msg=msg)
    h.send_error = lambda *a, **k: None
    h.send_response = lambda *a, **k: None
    h.send_header = lambda *a, **k: None
    h.end_headers = lambda: None
    h.log_message = lambda *a, **k: None
    return h, captured


class TestN6PrintTypeChecks:
    def test_printer_name_list_is_a_400(self):
        """A JSON array for printer_name is unhashable: `registry.get(list)`
        raises TypeError out of do_POST and the caller loses the connection
        instead of reading the reason."""
        h, captured = _print_handler({"printer_name": ["x"]})
        h.do_POST()
        assert captured["code"] == 400, captured
        assert "string" in captured["msg"]

    def test_printer_name_object_is_a_400(self):
        h, captured = _print_handler({"printer_name": {"a": 1}})
        h.do_POST()
        assert captured["code"] == 400, captured

    def test_images_null_is_a_400(self):
        """`images: null` passes the `.get` default and then `for ... in None`
        raises TypeError."""
        h, captured = _print_handler({"printer_name": "P1", "images": None})
        h.do_POST()
        assert captured["code"] == 400, captured
        assert "list" in captured["msg"]

    def test_images_string_is_a_400(self):
        """ "abc" would be iterated character by character and each character
        base64-decoded - a silent garbage print job instead of an error."""
        h, captured = _print_handler({"printer_name": "P1", "images": "abcdefg"})
        h.do_POST()
        assert captured["code"] == 400, captured

    def test_a_missing_printer_name_is_still_400(self):
        """The existing missing-key path is unchanged."""
        h, captured = _print_handler({})
        h.do_POST()
        assert captured["code"] == 400, captured
        assert captured["msg"] == "Missing printer_name"


# ---------------------------------------------------------------------------
# N7 - Storage Commitment rides the live C-MOVE association
# ---------------------------------------------------------------------------
def _live_sc_assoc(status=0x0000):
    """An established association that accepted the commit push model."""
    from unittest.mock import MagicMock

    assoc = MagicMock()
    assoc.is_established = True
    ctx = MagicMock()
    type(ctx).abstract_syntax = property(lambda self: "1.2.840.10008.1.20.1")
    assoc.accepted_contexts = [ctx]
    assoc.send_n_action.return_value = (MagicMock(Status=status), None)
    return assoc


class TestN7StorageCommitmentReusesAssoc:
    def test_no_second_association_is_opened(self, monkeypatch):
        """The commit N-ACTION goes out on the association that just
        delivered the study: a second AE.associate to the same peer is a
        duplicate ACSE round trip for a context that was negotiated already."""
        opened = []

        def _fail_ae(*a, **k):
            opened.append(a)
            raise AssertionError("AE was constructed")

        monkeypatch.setattr(cm, "AE", _fail_ae)
        assoc = _live_sc_assoc()

        assert cm._send_storage_commitment(
            "SCP", "127.0.0.1", 11112, [(MR, "u1")], "BRIDGE", assoc=assoc
        )
        assert opened == [], "a live association must be reused, not replaced"
        assoc.send_n_action.assert_called_once()

    def test_the_live_association_is_not_released(self, monkeypatch):
        """The caller still owns the association; releasing it here would
        break whatever it does next."""
        released = []
        monkeypatch.setattr(cm, "_release_or_abort", lambda a, **k: released.append(a))
        assoc = _live_sc_assoc()

        cm._send_storage_commitment(
            "SCP", "127.0.0.1", 11112, [(MR, "u1")], "BRIDGE", assoc=assoc
        )
        assert released == []

    def test_without_a_live_association_the_fallback_still_works(self, monkeypatch):
        """A caller with no association keeps the old behaviour."""
        from unittest.mock import MagicMock

        opened = []
        fresh = _live_sc_assoc()
        ae = MagicMock()
        ae.associate.side_effect = lambda *a, **k: opened.append(a) or fresh
        monkeypatch.setattr(cm, "AE", lambda *a, **k: ae)
        released = []
        monkeypatch.setattr(cm, "_release_or_abort", lambda a, **k: released.append(a))

        assert cm._send_storage_commitment(
            "SCP", "127.0.0.1", 11112, [(MR, "u1")], "BRIDGE"
        )
        assert len(opened) == 1, "the fallback opens its own association"
        assert released == [fresh], "and releases the association it owns"

    def test_the_cmove_job_passes_its_association(self, monkeypatch):
        """_execute_cmove_job hands its live association to the commit push."""
        seen = {}
        monkeypatch.setattr(
            cm,
            "_send_storage_commitment",
            lambda *a, **k: seen.update(args=a, kwargs=k) or True,
        )
        ctx = type("Ctx", (), {"abstract_syntax": "1.2.840.10008.1.20.1"})()
        streams = [(1, 0, 1, [(MR, "u1")], [], False, False, False)]
        ops = _run_execute(monkeypatch, streams, study_sops=["u1"], contexts=[ctx])

        assert "assoc" in seen.get("kwargs", {}), seen
        assert seen["kwargs"]["assoc"] is not None
        assert ops["finished"][1] is True, ops["finished"]


# ---------------------------------------------------------------------------
# N8 - multi-valued CS/UI/SH survive the JSON -> DIMSE conversion
# ---------------------------------------------------------------------------
class TestN8MultiValuedAttributes:
    def test_modalities_in_study_keeps_every_value(self):
        """ModalitiesInStudy (0008,0061 CS) is multi-valued; a row is not
        allowed to name one modality for a study performed on three."""
        from clean_code.protocol import _json_to_dataset

        ds = _json_to_dataset(
            {"00080061": {"vr": "CS", "Value": ["CT", "MR", "PT"]}},
            {"ModalitiesInStudy"},
        )
        values = ds[0x0008, 0x0061].value
        assert list(values) == ["CT", "MR", "PT"], repr(values)

    def test_a_single_value_is_still_a_plain_string(self):
        """VM 1 stays a scalar: an SCU reading a one-item list would see a
        different DICOM type than for any other single-valued CS."""
        from clean_code.protocol import _json_to_dataset

        ds = _json_to_dataset(
            {"00080061": {"vr": "CS", "Value": ["CT"]}}, {"ModalitiesInStudy"}
        )
        assert ds[0x0008, 0x0061].value == "CT"

    def test_a_multi_valued_person_name_is_not_joined(self):
        """PN is outside the multi-value path: its value is a structured
        object and the Alphabetic component is what is emitted."""
        from clean_code.protocol import _json_to_dataset

        ds = _json_to_dataset(
            {
                "00100010": {
                    "vr": "PN",
                    "Value": [{"Alphabetic": "Doe^Jane"}, {"Alphabetic": "Doe^John"}],
                }
            },
            {"PatientName"},
        )
        assert str(ds.PatientName) == "Doe^Jane"

    def test_the_mwl_converter_shares_the_same_helper(self):
        """The MWL converter routes through the same _json_add_vr, so the rule
        cannot drift between the two writers."""
        from clean_code.protocol import _json_add_vr
        from pydicom import Dataset

        ds = Dataset()
        assert _json_add_vr(ds, 0x0008, 0x0061, "CS", ["CT", "MR"])
        assert list(ds[0x0008, 0x0061].value) == ["CT", "MR"]


# ---------------------------------------------------------------------------
# N9 - outbox: concurrent writers of the same UID must not share a temp file
# ---------------------------------------------------------------------------
def _instance(sop_uid, pixel=b"\x00" * 64):
    from pydicom import Dataset
    from pydicom.dataset import FileMetaDataset
    from pydicom.uid import ExplicitVRLittleEndian

    ds = Dataset()
    ds.SOPClassUID = MR
    ds.SOPInstanceUID = sop_uid
    ds.StudyInstanceUID = "1.2.840.1.1"
    ds.SeriesInstanceUID = "1.2.840.1.2"
    ds.PatientName = "TMP^Race"
    ds.Rows = 1
    ds.Columns = len(pixel)
    ds.BitsAllocated = 8
    ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelRepresentation = 0
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.PixelData = pixel
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = MR
    ds.file_meta.MediaStorageSOPInstanceUID = sop_uid
    ds.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
    return ds


class TestN9ConcurrentEnqueue:
    def test_two_threads_enqueueing_the_same_uid_produce_a_valid_blob(self, tmp_path):
        """Both encoders must not write the SAME `<uid>.dcm.tmp` and then
        both `os.replace` it: the interleaved halves produce a truncated file
        that dcmread rejects, and the modality was already told 0x0000."""
        from clean_code.outbox import OutboxQueue
        from pydicom import dcmread

        queue = OutboxQueue(str(tmp_path / "outbox"), max_queue=100)
        uid = "1.2.840.10008.5.1.4.1.1.4.777"
        # Distinct pixel payloads so a mixed file is detectable, and enough
        # bytes that a torn write cannot pass for a complete dataset.
        payloads = [bytes([i]) * 4096 for i in range(8)]
        errors = []
        start = threading.Barrier(len(payloads))

        def writer(payload):
            try:
                start.wait(timeout=10)
                for _ in range(4):
                    queue.enqueue(_instance(uid, payload))
            except Exception as e:  # pragma: no cover - reported below
                errors.append(e)

        threads = [threading.Thread(target=writer, args=(p,)) for p in payloads]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=60)

        assert errors == [], errors
        blob = os.path.join(str(tmp_path / "outbox"), "pending", f"{uid}.dcm")
        assert os.path.isfile(blob), "the blob must have been placed"
        ds = dcmread(blob)
        assert str(ds.SOPInstanceUID) == uid
        # The payload must be ONE writer's payload, never a blend.
        assert list(ds.PixelData) in [
            list(p) for p in payloads
        ], "the stored pixels are a blend of two writers"
        leftovers = [n for n in os.listdir(os.path.dirname(blob)) if n.endswith(".tmp")]
        assert leftovers == [], leftovers

    def test_a_failed_write_leaves_no_temp_file(self, tmp_path, monkeypatch):
        """RAII on the failure path: the temp file is removed, not piled up."""
        from clean_code.outbox import OutboxQueue

        queue = OutboxQueue(str(tmp_path / "outbox"), max_queue=100)
        uid = "1.2.840.10008.5.1.4.1.1.4.778"

        import clean_code.outbox as ob

        def boom(*a, **k):
            raise OSError("disk full")

        monkeypatch.setattr(ob, "dcmwrite", boom)
        assert queue.enqueue(_instance(uid)) is False
        assert os.listdir(os.path.join(str(tmp_path / "outbox"), "pending")) == []


# ---------------------------------------------------------------------------
# N10 - the bundler must not silently drop a relative import
# ---------------------------------------------------------------------------
class TestN10BundleOrderRelativeImport:
    def test_a_relative_import_is_refused_loudly(self):
        """The dependency edge of `from . import config` was dropped without a
        word, so a bundle that needed the edge would be ordered wrongly and
        fail at run time. Refusing names the module instead."""
        from clean_code._bundle_order import find_imports

        try:
            find_imports("from . import config\n")
        except ValueError as e:
            assert "config" in str(e), e
        else:  # pragma: no cover - the assertion below reports it
            raise AssertionError("a relative import was accepted silently")

    def test_a_nested_relative_import_is_refused_loudly(self):
        from clean_code._bundle_order import find_imports

        with pytest.raises(ValueError) as exc:
            find_imports("from .handlers import cfind\n")
        assert "cfind" in str(exc.value)

    def test_absolute_imports_are_unchanged(self):
        from clean_code._bundle_order import find_imports

        got = find_imports("from clean_code import config\nimport clean_code.utils\n")
        assert "clean_code.config" in got
        assert "clean_code.utils" in got
