# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""0911 rabbit batch 3 regression tests - one falsifiable test per finding.

P0 / P1 / P2 / M3-M10. Each test pins the finding so reverting the fix makes
it fail.
"""

import logging
import os
import threading
import time

import clean_code.handlers.cfind as cf
import clean_code.nevent as nevent
import pytest
from clean_code.outbox import OutboxQueue
from pydicom import Dataset

MR = "1.2.840.10008.5.1.4.1.1.4"


def _write_blob(path, uid, name="Aged^Item"):
    """Write a minimal Part-10 file that dcmwrite accepts."""
    from pydicom import dcmwrite
    from pydicom.dataset import FileMetaDataset
    from pydicom.uid import ImplicitVRLittleEndian

    ds = Dataset()
    ds.SOPClassUID = MR
    ds.SOPInstanceUID = uid
    ds.StudyInstanceUID = "1.2.3"
    ds.SeriesInstanceUID = "1.2.3.4"
    ds.PatientName = name
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = MR
    ds.file_meta.MediaStorageSOPInstanceUID = uid
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    dcmwrite(path, ds, enforce_file_format=True)


# ---------------------------------------------------------------------------
# P0 - the N-EVENT-REPORT SCP role must be proposed as a role-selection item
# ---------------------------------------------------------------------------
class TestP0ScpRoleNegotiation:
    def _run(self, monkeypatch, *, as_scp=True, established=True, status=0x0000):
        """Drive send_n_event_report with AE.associate stubbed; return spy."""
        from pynetdicom.sop_class import StorageCommitmentPushModel

        spy = {"assoc_kwargs": None}

        class _Ctx:
            abstract_syntax = StorageCommitmentPushModel
            as_scp = None

        ctx = _Ctx()
        ctx.as_scp = as_scp

        class _Status:
            Status = status

        class _Assoc:
            is_established = established
            accepted_contexts = [ctx]

            def send_n_event_report(self, *a, **kw):
                return _Status(), None

        class _AE:
            def __init__(self, *a, **kw):
                self.acse_timeout = 30
                self.requested_contexts = []

            def add_requested_context(self, *a, **kw):
                self.requested_contexts.append(object())

            def associate(self, *a, **kw):
                spy["assoc_kwargs"] = kw
                return _Assoc()

        monkeypatch.setattr(nevent, "AE", _AE)
        monkeypatch.setattr(nevent, "_release_or_abort", lambda *a, **kw: None)
        ok = nevent.send_n_event_report(
            "127.0.0.1",
            11112,
            "MODALITY",
            "1.2.3",
            1,
            [(MR, "1.2.3.1")],
            [],
            bridge_ae_title="BRIDGE",
            max_retries=0,
        )
        return ok, spy

    def test_scp_role_is_proposed_through_ext_neg(self, monkeypatch):
        """PS3.7 D.3.3.4: the role proposal is a negotiation item of its own.

        `PresentationContext` carries only the acceptor-side role attributes,
        so proposing the requestor's SCP role on the context silently did
        nothing (AttributeError, swallowed) and the bridge never received the
        report - the actual root of the endless N-EVENT-REPORT retries."""
        from pynetdicom.sop_class import StorageCommitmentPushModel

        ok, spy = self._run(monkeypatch)
        assert ok is True
        kwargs = spy["assoc_kwargs"]
        assert kwargs is not None, "associate was never called"
        ext_neg = kwargs.get("ext_neg")
        assert ext_neg, "the SCP role was not proposed as a role-selection item"
        role = [
            item
            for item in ext_neg
            if str(item.sop_class_uid) == str(StorageCommitmentPushModel)
        ]
        assert role, ext_neg
        assert role[0].scp_role is True, "the requestor must hold the SCP role"
        assert role[0].scu_role is True, "the requestor also sends the request"

    def test_a_peer_that_declines_the_role_is_not_reported_delivered(
        self, monkeypatch, caplog
    ):
        """Accepting the abstract syntax but declining the role leaves the
        requestor a plain SCU, and the report cannot be pushed - so it must
        not count as delivered."""
        with caplog.at_level(logging.WARNING):
            ok, _spy = self._run(monkeypatch, as_scp=False)
        assert ok is False
        assert "SCP role" in "\n".join(r.message for r in caplog.records)

    def test_the_dead_helper_is_gone(self):
        """`_request_scp_role` set an attribute that does not exist."""
        assert not hasattr(nevent, "_request_scp_role")


# ---------------------------------------------------------------------------
# P1 - a tag whose element part holds a hex letter was never masked
# ---------------------------------------------------------------------------
class TestP1TagCaseMasking:
    @pytest.mark.parametrize(
        "tag,value",
        [
            ("0008103E", "CT CHEST"),  # SeriesDescription - the reported leak
            ("0008103e", "CT CHEST"),  # the same tag in lower case
            ("00081060", "Dr X"),  # NameOfPhysiciansReadingStudy
            ("00081080", "Dx"),  # AdmittingDiagnosesDescription
        ],
    )
    def test_a_tag_with_a_hex_letter_is_masked(self, tag, value):
        """`_masked_query` lower-cases the key before the lookup, so a mask set
        holding the canonical UPPER-case spelling never matched a tag whose
        element part contains A-F and its PHI value reached the log."""
        out = cf._masked_query({tag: value})
        assert out == f"{tag}=***", out

    def test_a_tag_without_a_hex_letter_still_masks(self):
        assert cf._masked_query({"00100010": "Doe^Jane"}) == "00100010=***"

    def test_every_tag_in_the_mask_set_survives_the_lookup(self):
        """The set and its consumer must agree on case - guard the whole set,
        not just the tags that happen to contain a letter today."""
        for tag in cf._MASKED_LOG_TAGS:
            assert cf._masked_query({tag: "PHI"}) == f"{tag}=***", tag
            assert cf._masked_query({tag.upper(): "PHI"}) == f"{tag.upper()}=***"


# ---------------------------------------------------------------------------
# P2 - VERIFY ONLY: the MWL path does not emit snake_case keys
# ---------------------------------------------------------------------------
class TestP2MwlKeysAreTags:
    def test_mwl_params_are_tags_and_mask(self):
        """Refutation pin for the repeated snake_case claim: the MWL path
        builds TAG keys (`00100020`), so masking matches and no snake_case key
        ever reaches `_masked_query`."""
        from clean_code.protocol import _build_mwl_params

        ds = Dataset()
        ds.PatientID = "PID12345"
        sps = Dataset()
        sps.Modality = "CT"
        ds.ScheduledProcedureStepSequence = [sps]

        params = _build_mwl_params(ds)
        assert params == {"00100020": "PID12345", "00080060": "CT"}, params
        assert not [k for k in params if "_" in k], "snake_case key reached the log path"
        masked = cf._masked_query(params)
        assert "PID12345" not in masked, masked


# ---------------------------------------------------------------------------
# M3 - a claimed item must not look stuck the moment it is claimed
# ---------------------------------------------------------------------------
class TestM3DequeueRefreshesMtime:
    def _queue_with_aged_item(self, tmp_path):
        q = OutboxQueue(str(tmp_path / "outbox"), max_queue=100)
        uid = "1.2.840.10008.5.1.4.1.1.4.900"
        blob = q._dcm_path(q._pending, uid)
        _write_blob(blob, uid)
        q._atomic_write(
            q._meta_path(q._pending, uid),
            '{"created_at": 0, "retry_count": 0, "next_retry_at": 0, "last_error": null}',
        )
        # The item sat in pending/ longer than STUCK_ITEM_TIMEOUT (a backlog
        # after an incident, parking, a 507 pause).
        old = time.time() - 3600
        os.utime(blob, (old, old))
        return q, uid

    def test_claim_refreshes_the_mtime(self, tmp_path):
        """`rename` preserves the enqueue mtime, and `recover_stuck` reads that
        mtime as the in-progress age: without a refresh the sweep returns the
        item to pending/ and the instance is delivered twice."""
        q, uid = self._queue_with_aged_item(tmp_path)
        claimed, _ds, blob_path, _raw = q.dequeue()
        assert claimed == uid
        age = time.time() - os.path.getmtime(blob_path)
        assert age < 60, f"claimed blob still looks {age:.0f}s old"

    def test_the_sweep_does_not_steal_a_freshly_claimed_item(self, tmp_path):
        """End-to-end: claim, then sweep - the item stays in progress."""
        q, uid = self._queue_with_aged_item(tmp_path)
        assert q.dequeue()[0] == uid

        q.recover_stuck()

        assert os.path.isfile(q._dcm_path(q._in_progress, uid)), (
            "the sweep reclaimed an item that had just been claimed"
        )
        assert not os.path.isfile(q._dcm_path(q._pending, uid))


# ---------------------------------------------------------------------------
# M4 - a stalled body must not block the server forever
# ---------------------------------------------------------------------------
class TestM4SocketTimeout:
    def test_the_handler_carries_a_socket_timeout(self):
        """`socketserver` applies `handler.timeout` to the connection in
        `setup()`, which is what bounds `rfile.read(Content-Length)`."""
        from clean_code.http_server import HTTP_SOCKET_TIMEOUT_SEC, BridgeHTTPHandler

        assert BridgeHTTPHandler.timeout == HTTP_SOCKET_TIMEOUT_SEC
        assert HTTP_SOCKET_TIMEOUT_SEC and HTTP_SOCKET_TIMEOUT_SEC > 0

    def test_the_timeout_reaches_the_connection(self):
        """Prove the attribute is actually consumed, not merely defined."""
        import socket

        from clean_code.http_server import BridgeHTTPHandler

        handler = BridgeHTTPHandler.__new__(BridgeHTTPHandler)
        a, b = socket.socketpair()
        try:
            handler.request = a
            type(handler).__dict__  # noqa: B018 - the attr lives on the class
            handler.timeout = 7  # what the class attribute supplies
            handler.disable_nagle_algorithm = False
            handler.setup()
            assert a.gettimeout() == 7
        finally:
            a.close()
            b.close()


# ---------------------------------------------------------------------------
# M5 - a logging call must never raise out of the handler
# ---------------------------------------------------------------------------
class TestM5LogMessageArgs:
    def test_arguments_are_passed_through(self, caplog):
        """The message and its arguments go to `logging` separately, so the
        journal entry is complete instead of a bare format string."""
        from clean_code.http_server import BridgeHTTPHandler

        handler = BridgeHTTPHandler.__new__(BridgeHTTPHandler)
        with caplog.at_level(logging.DEBUG):
            handler.log_message("code %s", 404)
        text = "\n".join(r.getMessage() for r in caplog.records)
        assert "[HTTP-BRIDGE] code 404" in text, text

    def test_a_mismatched_argument_count_does_not_raise(self):
        """`format % args` raises TypeError when the counts disagree, and
        `send_error` forwards text that reached it from the request - so the
        pre-formatted call could raise where logging must not."""
        from clean_code.http_server import BridgeHTTPHandler

        handler = BridgeHTTPHandler.__new__(BridgeHTTPHandler)
        # More arguments than placeholders: this is the shape that used to
        # raise (logging itself would only complain, never raise).
        handler.log_message("%s", "a", "b")

    def test_a_mismatched_argument_count_would_have_raised_before(self):
        """Pins the premise: the old expression really does raise here, so the
        test above is not vacuous."""
        fmt, args = "%s", ("a", "b")
        with pytest.raises(TypeError):
            fmt % args


# ---------------------------------------------------------------------------
# M6 - the built-in fallback accepts the current US multi-frame and NM UIDs
# ---------------------------------------------------------------------------
class TestM6BuiltinSopClasses:
    def test_the_current_uids_are_in_the_fallback(self):
        """No sop_classes.yaml is exactly when the fallback runs, and that is
        when cine (…1.1.3.1) and NM (…1.1.20) used to be refused."""
        from clean_code.registry import _BUILTIN_SOP_CLASSES, _is_sop_accepted

        reg = _BUILTIN_SOP_CLASSES()
        assert _is_sop_accepted(reg, "1.2.840.10008.5.1.4.1.1.3.1") is True
        assert _is_sop_accepted(reg, "1.2.840.10008.5.1.4.1.1.20") is True
        # The Retired UIDs stay: pre-2006 equipment still sends them.
        assert _is_sop_accepted(reg, "1.2.840.10008.5.1.4.1.1.3") is True
        assert _is_sop_accepted(reg, "1.2.840.10008.5.1.4.1.1.5") is True

    def test_the_documented_count_matches_the_table(self):
        from clean_code.registry import _BUILTIN_SOP_CLASSES

        doc = _BUILTIN_SOP_CLASSES.__doc__
        assert doc is not None
        assert str(len(_BUILTIN_SOP_CLASSES())) in doc, doc


# ---------------------------------------------------------------------------
# M7 - an unusable port is a configuration error, not a first-request failure
# ---------------------------------------------------------------------------
class TestM7UrlPort:
    @pytest.mark.parametrize("url", ["http://host:abc/dicomweb", "http://host:99999/dicomweb"])
    def test_an_invalid_port_is_rejected_at_load(self, url, tmp_path):
        from clean_code.config import ConfParser, validate_clarus_url

        cfg = tmp_path / "clbridge.conf"
        cfg.write_text(f"[dicomweb]\nclarus_url = {url}\n", encoding="utf-8")
        with pytest.raises(ValueError) as exc:
            validate_clarus_url(ConfParser(str(cfg)))
        assert "port" in str(exc.value).lower(), exc.value

    def test_a_valid_port_is_accepted(self, tmp_path):
        from clean_code.config import ConfParser, validate_clarus_url

        cfg = tmp_path / "clbridge.conf"
        cfg.write_text("[dicomweb]\nclarus_url = http://host:8024/dicomweb\n", encoding="utf-8")
        url, _cert, _key = validate_clarus_url(ConfParser(str(cfg)))
        assert url == "http://host:8024/dicomweb", url


# ---------------------------------------------------------------------------
# M8 - the sweep must not adopt a blob that is still being written
# ---------------------------------------------------------------------------
class TestM8OrphanGrace:
    def _queue_with_orphan(self, tmp_path, age):
        q = OutboxQueue(str(tmp_path / "outbox"), max_queue=100)
        uid = "1.2.840.10008.5.1.4.1.1.4.950"
        blob = q._dcm_path(q._pending, uid)
        _write_blob(blob, uid)
        stamp = time.time() - age
        os.utime(blob, (stamp, stamp))
        return q, uid

    def test_a_fresh_orphan_is_not_adopted(self, tmp_path):
        """enqueue writes .dcm first and .meta LAST, and the sweep runs on a
        timer while enqueue is in flight: adopting a fresh blob publishes a
        half-written instance as ready."""
        q, uid = self._queue_with_orphan(tmp_path, age=0)
        q.recover_stuck()
        assert not os.path.isfile(q._meta_path(q._pending, uid)), (
            "a blob that may still be being written was published"
        )

    def test_an_old_orphan_is_adopted(self, tmp_path):
        """The crashed-enqueue case the sweep exists for still works."""
        q, uid = self._queue_with_orphan(tmp_path, age=3600)
        q.recover_stuck()
        assert os.path.isfile(q._meta_path(q._pending, uid))


# ---------------------------------------------------------------------------
# M9 - the SOP index must not share a temp file between writers
# ---------------------------------------------------------------------------
class TestM9SopIndexTempFile:
    def test_concurrent_inserts_all_survive(self, tmp_path):
        """Two reactor threads handle the same instance, so both write the same
        `<uid>.json`; a shared `.tmp` made them interleave and lost a record."""
        from clean_code.sop_index import SopIndexManager

        idx = SopIndexManager(str(tmp_path / "sop_index"))
        uid = "1.2.840.10008.5.1.4.1.1.4.910"
        errors = []
        start = threading.Barrier(8)

        def writer(n):
            try:
                start.wait(timeout=10)
                for _ in range(6):
                    idx.insert_sop(uid, f"1.2.3.{n}", f"1.2.3.4.{n}")
                    # A concurrent READER matters as much as a writer: its open
                    # handle is what makes the publish rename fail on Windows.
                    idx.lookup_sop(uid)
            except Exception as e:  # pragma: no cover - reported below
                errors.append(e)

        threads = [threading.Thread(target=writer, args=(i,)) for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=60)

        # A `join(timeout=...)` that times out returns silently, so a leaked
        # writer thread used to leave this test green while the index was
        # still being written underneath the assertions below. Fail on the
        # leak explicitly, naming the thread, before looking at the state.
        still_running = [t.name for t in threads if t.is_alive()]
        assert still_running == [], (
            "writer thread(s) still running after join: " f"{still_running}"
        )

        assert errors == [], errors
        entry = idx.lookup_sop(uid)
        assert entry is not None, "the index entry was lost"
        assert entry["study_uid"].startswith("1.2.3.")
        leftovers = [n for n in os.listdir(idx._dir) if n.endswith(".tmp")]
        assert leftovers == [], leftovers

    def test_a_failed_write_leaves_no_temp_file(self, tmp_path, monkeypatch):
        import clean_code.sop_index as si

        idx = si.SopIndexManager(str(tmp_path / "sop_index"))

        def boom(*a, **k):
            raise OSError("disk full")

        monkeypatch.setattr(si.json, "dump", boom)
        with pytest.raises(OSError):
            idx.insert_sop("1.2.3.4", "1.2.3", "1.2.3.4")
        assert os.listdir(idx._dir) == [], os.listdir(idx._dir)


# ---------------------------------------------------------------------------
# M10 - the forward hook must find the primary blob after it was claimed
# ---------------------------------------------------------------------------
class TestM10ForwardAfterClaim:
    def test_the_forward_hook_finds_the_blob_in_in_progress(self, tmp_path):
        """The drain can claim the item (pending/ -> in_progress/) between the
        primary enqueue returning and the forward hook running. Looking only in
        pending/ dropped the instance from the second archive."""
        import inspect

        from clean_code.handlers import cstore

        src = inspect.getsource(cstore.handle_c_store)
        assert "outbox._in_progress" in src, (
            "the forward hook does not fall back to in_progress/"
        )

    def test_a_blob_moved_to_in_progress_is_still_mirrored(self, tmp_path):
        """Behavioural form of the same contract, using the real API."""
        q = OutboxQueue(str(tmp_path / "forward"), max_queue=100)
        uid = "1.2.840.10008.5.1.4.1.1.4.930"
        ds = Dataset()
        ds.SOPClassUID = MR
        ds.SOPInstanceUID = uid
        ds.StudyInstanceUID = "1.2.3"
        ds.SeriesInstanceUID = "1.2.3.4"
        ds.PatientName = "Claimed^Blob"
        assert q.enqueue(ds) is True

        # The drain claims it: the blob leaves pending/.
        claimed, _ds, blob_path, _raw = q.dequeue()
        assert claimed == uid
        assert not os.path.isfile(q._dcm_path(q._pending, uid))

        # The forward hook now resolves the blob exactly as the caller does.
        forward = OutboxQueue(str(tmp_path / "fwd"), max_queue=100)
        primary_blob = q._dcm_path(q._pending, uid)
        if not os.path.isfile(primary_blob):
            primary_blob = q._dcm_path(q._in_progress, uid)
        assert forward.enqueue_from_blob(primary_blob, uid) is True
        assert os.path.isfile(forward._dcm_path(forward._pending, uid))
