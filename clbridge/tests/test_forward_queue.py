# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Stage 2 (0409) forward-channel tests.

Covers the accepted architect spec (analysis/0409_architect_bridge_forwarder_REPORT.md
Stage 2):
  - enqueue_from_blob: hardlink path (+ spy-copy on EXDEV fallback), .mods copy,
    .meta written LAST (ready marker), dup/full/drop semantics.
  - hook in handle_c_store: forward channel can NEVER change the modality status;
    primary-enqueue failure -> no forward attempt; forward failure/full -> 0x0000
    kept and forward_dropped_total incremented; primary untouched.

Files exercised (clean_code/ only):
  outbox.py::enqueue_from_blob / _link_or_copy
  handlers/cstore.py::handle_c_store (forward_outbox hook)
"""

import json
import os

from clean_code.handlers.cstore import handle_c_store
from clean_code.outbox import OutboxQueue
from pydicom import Dataset

from tests.conftest import (
    make_fake_cstore_event,
)

URL = "http://127.0.0.1:8024/dicomweb"
_SOP_UID = "1.2.840.10008.5.1.4.1.1.2.555"
_MODS = [((0x0010, 0x0010), "Old^Name", "New^Name")]


def _make_dataset(sop_uid):
    """Minimal DICOM dataset valid for dcmwrite/enqueue."""
    from pydicom.dataset import FileMetaDataset
    from pydicom.uid import ImplicitVRLittleEndian

    ds = Dataset()
    ds.SOPInstanceUID = sop_uid
    ds.PatientName = "Test^Patient"
    ds.StudyInstanceUID = "1.2.840.10008." + sop_uid[-4:]
    ds.SeriesInstanceUID = "1.2.840.10008.2." + sop_uid[-4:]
    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.file_meta.MediaStorageSOPInstanceUID = sop_uid
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    return ds


def _read(src):
    with open(src, "rb") as f:
        return f.read()


def _dropped_count():
    """Return the forward_dropped_total counter value (guarded, internal API)."""
    from clean_code.metrics import forward_dropped_total

    if hasattr(forward_dropped_total, "_value"):
        return forward_dropped_total._value.get()
    return 0


# ---------------------------------------------------------------------------
# Queue-level: enqueue_from_blob
# ---------------------------------------------------------------------------


class TestEnqueueFromBlob:

    def _make_queues(self, tmp_path, **fwd_kwargs):
        primary = OutboxQueue(outbox_dir=str(tmp_path / "primary"))
        forward = OutboxQueue(outbox_dir=str(tmp_path / "fwd"), **fwd_kwargs)
        ds = _make_dataset(_SOP_UID)
        assert primary.enqueue(ds) is True
        src = primary._dcm_path(primary._pending, _SOP_UID)
        assert os.path.isfile(src)
        return primary, forward, ds, src

    def test_hardlink_blob_visible_with_meta_last(self, tmp_path, monkeypatch):
        """Blob lands in forward/pending via hardlink; .meta present ready."""
        _, forward, ds, src = self._make_queues(tmp_path)

        # Record whether os.link was actually used (hardlink path taken).
        real_link = os.link
        used_link = []

        def _spy_link(a, b):
            used_link.append(True)
            return real_link(a, b)

        monkeypatch.setattr("clean_code.outbox.os.link", _spy_link)

        res = forward.enqueue_from_blob(src, _SOP_UID, modifications=_MODS)
        assert res is True
        assert forward.pending_count() == 1

        dst = forward._dcm_path(forward._pending, _SOP_UID)
        assert os.path.isfile(dst)
        assert _read(dst) == _read(src)

        # .mods sidecar mirrored.
        mods_path = forward._mods_path(forward._pending, _SOP_UID)
        assert os.path.isfile(mods_path)

        # .meta written LAST (strictly newer than .dcm and .mods).
        meta_path = forward._meta_path(forward._pending, _SOP_UID)
        assert os.path.isfile(meta_path)
        with open(meta_path, "r", encoding="utf-8") as f:
            meta = json.load(f)
        assert meta.get("retry_count") == 0
        assert os.path.getmtime(dst) <= os.path.getmtime(meta_path)
        assert os.path.getmtime(mods_path) <= os.path.getmtime(meta_path)

        # Ready marker holds: dequeue can atomically claim the mirrored item.
        sop_uid, ds_out, blob_path, _ = forward.dequeue()
        assert sop_uid == _SOP_UID
        assert ds_out is not None

        # On a filesystem that supports hard links the link path was used.
        if used_link:
            assert os.stat(src).st_ino == os.stat(blob_path).st_ino

    def test_exdev_fallback_copies_bytes(self, tmp_path, monkeypatch):
        """os.link raising (EXDEV / unsupported) -> byte-copy fallback."""
        _, forward, _, src = self._make_queues(tmp_path)
        import shutil as _shutil

        real_copy = _shutil.copyfile
        used_copy = []

        def _fake_link(a, b):
            raise OSError(18, "Invalid cross-device link")

        def _spy_copy(a, b):
            used_copy.append(True)
            return real_copy(a, b)

        monkeypatch.setattr("clean_code.outbox.os.link", _fake_link)
        monkeypatch.setattr("clean_code.outbox.shutil.copyfile", _spy_copy)

        res = forward.enqueue_from_blob(src, _SOP_UID, modifications=None)
        assert res is True
        dst = forward._dcm_path(forward._pending, _SOP_UID)
        assert os.path.isfile(dst)
        assert _read(dst) == _read(src)
        assert used_copy == [True], "copy fallback must be exercised on EXDEV"
        # ready marker still last despite copy path.
        assert os.path.isfile(forward._meta_path(forward._pending, _SOP_UID))

    def test_duplicate_does_not_double_count(self, tmp_path):
        """Second enqueue_from_blob for the same SOP -> 'duplicate', no 2nd file."""
        _, forward, _, src = self._make_queues(tmp_path)
        assert forward.enqueue_from_blob(src, _SOP_UID, modifications=None) is True
        assert (
            forward.enqueue_from_blob(src, _SOP_UID, modifications=None) == "duplicate"
        )
        assert forward.pending_count() == 1

    def test_invalid_source_returns_false(self, tmp_path):
        """Missing/None source blob -> False (never a silent success)."""
        _, forward, _, _ = self._make_queues(tmp_path)
        assert (
            forward.enqueue_from_blob(
                str(tmp_path / "nope.dcm"),
                _SOP_UID,
                modifications=None,
            )
            is False
        )

    def test_queue_full_drop_increments_counter(self, tmp_path):
        """max_queue reached -> drop-newest + forward_dropped_total, no file."""
        before = _dropped_count()

        # max_queue=1; seed one item so forward is full before the drop call.
        forward = OutboxQueue(
            outbox_dir=str(tmp_path / "fwd"),
            max_queue=1,
        )
        seed = _make_dataset("1.2.840.10008.5.1.4.1.1.2.444")
        ds = _make_dataset(_SOP_UID)
        primary = OutboxQueue(outbox_dir=str(tmp_path / "primary"))
        assert primary.enqueue(seed) is True
        seed_src = primary._dcm_path(primary._pending, "1.2.840.10008.5.1.4.1.1.2.444")
        assert (
            forward.enqueue_from_blob(seed_src, "1.2.840.10008.5.1.4.1.1.2.444") is True
        )

        assert primary.enqueue(ds) is True
        src = primary._dcm_path(primary._pending, _SOP_UID)
        assert forward.enqueue_from_blob(src, _SOP_UID, modifications=None) == "full"
        # The dropped SOP is NOT in forward/pending; the seeded one survives.
        assert not os.path.isfile(forward._dcm_path(forward._pending, _SOP_UID))
        assert os.path.isfile(
            forward._dcm_path(forward._pending, "1.2.840.10008.5.1.4.1.1.2.444")
        )
        # counter incremented over baseline.
        assert _dropped_count() > before


# ---------------------------------------------------------------------------
# Handler-level: hook inside handle_c_store
# ---------------------------------------------------------------------------


class TestForwardCstoreHook:

    def _queues(self, tmp_path):
        primary = OutboxQueue(outbox_dir=str(tmp_path / "primary"))
        forward = OutboxQueue(outbox_dir=str(tmp_path / "fwd"))
        return primary, forward

    def test_forward_success_keeps_0x0000(self, tmp_path, mock_shutdown_not_set):
        """Real forward enqueue after primary success -> 0x0000, blob mirrored."""
        primary, forward = self._queues(tmp_path)
        event = make_fake_cstore_event(sop_instance_uid=_SOP_UID)
        status = handle_c_store(
            event,
            URL,
            False,
            outbox=primary,
            forward_outbox=forward,
        )
        assert status == 0x0000
        assert os.path.isfile(forward._dcm_path(forward._pending, _SOP_UID))

    def test_forward_full_never_changes_status(self, tmp_path, mock_shutdown_not_set):
        """Forward queue full -> still 0x0000; forward_dropped_total incremented."""
        before = _dropped_count()

        forward = OutboxQueue(outbox_dir=str(tmp_path / "fwd"), max_queue=1)
        # seed forward so it is full for the incoming SOP.
        seedd = _make_dataset("1.2.840.10008.5.1.4.1.1.2.999")
        primary = OutboxQueue(outbox_dir=str(tmp_path / "primary"))
        assert primary.enqueue(seedd) is True
        seed_src = primary._dcm_path(primary._pending, "1.2.840.10008.5.1.4.1.1.2.999")
        assert (
            forward.enqueue_from_blob(seed_src, "1.2.840.10008.5.1.4.1.1.2.999") is True
        )

        event = make_fake_cstore_event(sop_instance_uid=_SOP_UID)
        status = handle_c_store(
            event,
            URL,
            False,
            outbox=primary,
            forward_outbox=forward,
        )
        # Modality status from the PRIMARY persist is untouched by the full queue.
        assert status == 0x0000
        assert not os.path.isfile(forward._dcm_path(forward._pending, _SOP_UID))
        assert _dropped_count() > before

    def test_forward_exception_keeps_0x0000(
        self, tmp_path, mock_shutdown_not_set, monkeypatch
    ):
        """Even a raised forward enqueue cannot change the modality status."""
        before = _dropped_count()
        primary, forward = self._queues(tmp_path)
        raised = []

        def _boom(*a, **k):
            raised.append(True)
            raise RuntimeError("boom")

        monkeypatch.setattr(forward, "enqueue_from_blob", _boom)
        event = make_fake_cstore_event(sop_instance_uid=_SOP_UID)
        status = handle_c_store(
            event,
            URL,
            False,
            outbox=primary,
            forward_outbox=forward,
        )
        assert status == 0x0000
        assert raised == [True]
        assert _dropped_count() > before

    def test_primary_failure_no_forward_attempt(self, tmp_path, mock_shutdown_not_set):
        """Primary enqueue returning False -> 0xA700 and forward NEVER attempted."""
        primary, forward = self._queues(tmp_path)
        calls = []

        def _failing_enq(dataset, modifications=None):
            calls.append("primary")
            return False

        primary.enqueue = _failing_enq
        fwd_calls = []

        def _spy_fwd(*a, **k):
            fwd_calls.append(True)
            return True

        forward.enqueue_from_blob = _spy_fwd

        event = make_fake_cstore_event(sop_instance_uid=_SOP_UID)
        status = handle_c_store(
            event,
            URL,
            False,
            outbox=primary,
            forward_outbox=forward,
        )
        assert status == 0xA700
        assert calls == ["primary"]
        assert fwd_calls == [], "forward must not be attempted when primary fails"
        # Keep the real method alive for teardown (not strictly needed).

    def test_no_forward_queue_touches_nothing(self, tmp_path, mock_shutdown_not_set):
        """forward_outbox=None (default) -> pure primary behaviour, 0x0000."""
        # Only the primary queue exists; assert the hook creates NO forward dir
        # and the primary survives a normal persist.
        primary = OutboxQueue(outbox_dir=str(tmp_path / "primary"))
        event = make_fake_cstore_event(sop_instance_uid=_SOP_UID)
        status = handle_c_store(event, URL, False, outbox=primary, forward_outbox=None)
        assert status == 0x0000
        # The primary blob is persisted; no forward tree appeared.
        assert os.path.isfile(primary._dcm_path(primary._pending, _SOP_UID))
        assert not os.path.exists(str(tmp_path / "fwd"))
