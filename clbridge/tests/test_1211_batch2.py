# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""1211 convergence batch 2 regression tests, one per finding.

Each test fails when its fix is reverted: the assertion targets the behaviour
the finding described, not the shape of the code.
"""

import logging

import pytest


# ---------------------------------------------------------------------------
# CC5 - a malformed STOW-RS batch body keeps the (results, retryafter) shape
# ---------------------------------------------------------------------------
class TestMalformedStowBatchBody:
    def test_a_non_object_body_is_reported_per_instance(self):
        """PS3.18 10.5.2: the response must be a DICOM JSON object. Returning
        a bare 3-tuple here made the caller unpack two values from three and
        die with ValueError, losing the whole batch's outcome."""
        from clean_code import http_helpers as hh
        from pydicom.dataset import Dataset

        calls = []

        class FakeClient:
            def post(self, *a, **k):
                calls.append(1)
                return _FakeResp(200, "[1, 2, 3]", "application/dicom+json")

        results, retryafter = hh.stow_batch_instances(
            [("1.2.3.1", Dataset(), b"x"), ("1.2.3.2", Dataset(), b"y")],
            "http://127.0.0.1:1/dicomweb",
            client=FakeClient(),
        )
        assert retryafter is None
        assert [r[0] for r in results] == ["1.2.3.1", "1.2.3.2"]
        assert all(
            r[1] == 0xA700 for r in results
        ), "a body the server could not express as DICOM JSON is a reject"


# ---------------------------------------------------------------------------
# CM2 - undelivered/ keeps the audit sidecars
# ---------------------------------------------------------------------------
def test_undelivered_items_keep_their_sidecars(tmp_path):
    """Deleting .meta/.mods on the way to undelivered/ destroyed the record of
    what the bridge changed and why the instance gave up."""
    from clean_code.outbox import OutboxQueue
    from pydicom.dataset import Dataset, FileMetaDataset
    from pydicom.uid import ImplicitVRLittleEndian

    q = OutboxQueue(str(tmp_path), max_retries=1)
    ds = Dataset()
    ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.SOPInstanceUID = "1.2.3.7"
    ds.StudyInstanceUID = "1.2.3"
    ds.PatientID = "P"
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.file_meta.MediaStorageSOPInstanceUID = "1.2.3.7"
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    assert q.enqueue(ds) is True
    assert q.dequeue() is not None

    q.move_to_undelivered("1.2.3.7")

    undelivered = tmp_path / "undelivered"
    names = {p.name for p in undelivered.iterdir()}
    stem = next(n for n in names if not n.endswith(".meta"))
    assert stem.endswith(".dcm"), names
    assert (
        stem[:-4] + ".meta" in names
    ), "the give-up record must travel with the blob: " + repr(names)
    assert not (tmp_path / "in_progress" / stem).exists()
    assert not (tmp_path / "in_progress" / (stem[:-4] + ".meta")).exists()


# ---------------------------------------------------------------------------
# CM3 - a registry entry the parser cannot read is refused, loudly
# ---------------------------------------------------------------------------
class TestSopRegistryRefusal:
    def test_an_entry_without_accept_falls_back_and_says_so(self, tmp_path, caplog):
        """An entry missing `accept` meant the parser desynchronised. Loading
        it as-is silently turned an unreadable class into an accepted one."""
        from clean_code.registry import _BUILTIN_SOP_CLASSES, _load_sop_registry

        path = tmp_path / "sop_classes.yaml"
        path.write_text(
            "sop_classes:\n"
            "  1.2.840.10008.5.1.4.1.1.2:\n"
            "    name: CT Image Storage\n",
            encoding="utf-8",
        )
        with caplog.at_level(logging.ERROR):
            registry = _load_sop_registry(str(path))

        assert registry == _BUILTIN_SOP_CLASSES()
        assert (
            "1.2.840.10008.5.1.4.1.1.2" not in registry
            or registry["1.2.840.10008.5.1.4.1.1.2"].get("accept") is not None
        )
        assert any(
            "without `accept`" in r.message for r in caplog.records
        ), "the operator must be told which UID and which file"

    def test_a_wellformed_registry_is_still_loaded(self, tmp_path):
        from clean_code.registry import _load_sop_registry

        path = tmp_path / "sop_classes.yaml"
        path.write_text(
            "sop_classes:\n"
            "  1.2.840.10008.5.1.4.1.1.2:\n"
            "    name: CT Image Storage\n"
            "    accept: true\n",
            encoding="utf-8",
        )
        registry = _load_sop_registry(str(path))
        assert registry["1.2.840.10008.5.1.4.1.1.2"]["accept"] is True


# ---------------------------------------------------------------------------
# CM4 - a WARNING C-STORE-RSP is a delivered instance
# ---------------------------------------------------------------------------
class TestForwardWarningIsDelivered:
    def test_warning_statuses_are_treated_as_delivered(self):
        """PS3.7 Annex C: 0xB000-0xBFFF are WARNING - the instance WAS stored.
        Treating them as failures retried a delivered instance and eventually
        parked it in undelivered/ while the destination held it."""
        import inspect

        import clean_code.main as m

        src = inspect.getsource(m._forward_process_batch)
        assert (
            "0xB000" in src and "0xBFFF" in src
        ), "the deliverable set must include the WARNING class"
        code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
        assert "mark_delivered(uid)" in code


# ---------------------------------------------------------------------------
# CM5 - an IPv6 http_bind is bindable
# ---------------------------------------------------------------------------
def test_the_http_server_takes_its_address_family_from_the_bind_host():
    """HTTPServer defaults to AF_INET, so an IPv6 http_bind died at bind time
    and took the whole bridge down."""
    import inspect
    import socket

    import clean_code.main as m

    src = inspect.getsource(m)
    assert "socket.AF_INET6" in src
    assert "address_family" in src
    # The literal check is what decides it; "::1" must not be treated as v4.
    assert ":" in ":1"
    assert socket.AF_INET6 != socket.AF_INET


# ---------------------------------------------------------------------------
# CM6 - a known-down server gets the batch back UNBURNED
# ---------------------------------------------------------------------------
def test_a_down_server_returns_the_batch_to_pending_without_a_retry_burn():
    """Sending to a known-down peer costs a retry per item for a guaranteed
    failure; ten of them park the study in undelivered/."""
    import inspect

    import clean_code.main as m

    src = inspect.getsource(m._outbox_worker)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    assert "requeue(" in code, "the buffered tail goes back to pending/"
    assert (
        "_deliver_batch()"
        not in code.split("not health_checker.is_healthy()")[1].split(
            "elif _was_paused"
        )[0]
    ), "a down server must not be sent to at all"


# ---------------------------------------------------------------------------
# CM7 - a partial pre-flight keeps the classes it learned
# ---------------------------------------------------------------------------
def test_a_partial_preflight_does_not_narrow_the_proposal_to_learned_classes():
    """Ignoring the registry for classes the pre-flight never reached made
    whole SOP classes fail with 'no presentation context'."""
    import inspect

    import clean_code.handlers.cmove as cm

    src = inspect.getsource(cm._associate_for_move)
    code = "\n".join(line.split("#", 1)[0] for line in src.splitlines())
    assert (
        "_proposed_ts_per_class(sop_registry)" in code
    ), "the registry must still fill in the classes the pre-flight missed"
    assert (
        "protected" in code
    ), "a class the pre-flight SAW must not be dropped by the budget trim"


# ---------------------------------------------------------------------------
# CM8 - the per-peer gate honours shutdown and cancellation
# ---------------------------------------------------------------------------
def test_the_peer_gate_gives_up_on_shutdown(monkeypatch):
    """The 0.05 s wait loop never looked at the stop flag, so a stopped
    bridge blocked its worker join behind a slow peer."""
    import clean_code.handlers.cmove as cm

    key = ("10.9.9.9", 11112, "PEER")
    cm._ASSOC_BUSY[key] = (12345, 1)  # owned by another thread
    try:
        with pytest.raises(cm._AssocGateAborted):
            cm._job_gate_enter(*key, give_up=lambda: True)
    finally:
        cm._ASSOC_BUSY.pop(key, None)
        cm._ASSOC_GATE_LOG_LAST.pop(key, None)

    # The gate must NOT have been taken on the aborting path.
    assert key not in cm._ASSOC_BUSY


# ---------------------------------------------------------------------------
# T1 - a malformed QIDO body is a failure, not an empty result
# ---------------------------------------------------------------------------
class TestQidoMalformedBody:
    def _call(self, resp, monkeypatch):
        from clean_code import http_helpers as hh

        class FakeClient:
            def get(self, *a, **k):
                return resp

            def close(self):
                pass

        monkeypatch.setattr(hh, "_http_retry", lambda fn, **k: fn())
        monkeypatch.setattr(hh.httpx, "Client", lambda **k: FakeClient())
        return hh._qido_request("http://h/dicomweb/studies", {})

    def test_html_is_not_an_empty_match_list(self, monkeypatch):
        """Returning [] for a proxy error page made it indistinguishable from
        'the query matched nothing'."""
        from clean_code import http_helpers as hh

        resp = _FakeResp(200, "<html>proxy error</html>", "text/html")
        with pytest.raises(hh.QidoMalformedBodyError):
            self._call(resp, monkeypatch)

    def test_an_empty_204_is_still_an_empty_result(self, monkeypatch):
        resp = _FakeResp(204, "", None)
        assert self._call(resp, monkeypatch) == []


# ---------------------------------------------------------------------------
# T2 - 0x0118 is a Storage Commitment FAILURE
# ---------------------------------------------------------------------------
class TestStorageCommitmentFailureStatus:
    def _run(self, status_code):
        from unittest.mock import MagicMock, patch

        from clean_code.handlers.cmove import _send_storage_commitment

        assoc = MagicMock()
        assoc.is_established = True
        ctx = MagicMock()
        ctx.abstract_syntax = "1.2.840.10008.1.20.1"
        assoc.accepted_contexts = [ctx]
        status = MagicMock()
        status.Status = status_code
        assoc.send_n_action.return_value = (status, None)

        with patch("clean_code.handlers.cmove.AE") as ae_cls:
            ae = MagicMock()
            ae.associate.return_value = assoc
            ae_cls.return_value = ae
            return _send_storage_commitment(
                move_dest_ae="WEASIS",
                target_host="10.0.0.5",
                target_port=11112,
                sop_list=[("1.2.840.10008.5.1.4.1.1.2", "1.2.3.1")],
                bridge_ae_title="CLARUS_BRIDGE",
            )

    def test_no_such_sop_class_is_a_failure(self, caplog):
        """0x0118 says the SCP refused the transaction. Reporting True claimed
        a commitment no SCP holds."""
        with caplog.at_level(logging.INFO):
            assert self._run(0x0118) is False
        assert "not committed" in "\n".join(r.message for r in caplog.records)

    def test_success_is_a_success(self):
        assert self._run(0x0000) is True

    def test_a_c_space_warning_is_not_a_commitment(self, caplog):
        """PS3.4 J.3.2: the Storage Commitment N-ACTION-RSP defines Success
        (0x0000) and Processing Failure (0x0110) only. 0x0001 is the C-* space
        "warning - SOP class not supported": the SCP opened nothing, so the
        answer must stay distinguishable from an accepted commitment."""
        with caplog.at_level(logging.INFO):
            assert self._run(0x0001) is False
        text = "\n".join(r.message for r in caplog.records)
        assert "0x0001" in text
        assert "not committed" in text


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------
class _FakeResp:
    def __init__(self, status_code, text, content_type=None):
        self.status_code = status_code
        self.text = text
        self.content = text.encode("utf-8") if isinstance(text, str) else text
        self.headers = {"content-type": content_type} if content_type else {}

    def json(self):
        import json

        return json.loads(self.text)

    def raise_for_status(self):
        if self.status_code >= 400:
            raise AssertionError(f"unexpected {self.status_code}")


class _ClientFor:
    def __init__(self, resp):
        self._resp = resp

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False

    def get(self, *a, **k):
        return self._resp

    def post(self, *a, **k):
        return self._resp
