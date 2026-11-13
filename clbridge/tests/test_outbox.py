# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""Tests for OutboxQueue — filesystem-backed reliable DICOM persistence."""

import os
import shutil
import tempfile
import time

import pytest
from pydicom import Dataset


@pytest.fixture
def outbox():
    """Create a temporary OutboxQueue for testing."""
    from clean_code.outbox import OutboxQueue

    tmpdir = tempfile.mkdtemp()
    oq = OutboxQueue(outbox_dir=tmpdir)
    yield oq
    oq.close()
    shutil.rmtree(tmpdir, ignore_errors=True)


def _make_dataset(sop_uid):
    """Create a minimal pydicom Dataset with given SOPInstanceUID."""
    ds = Dataset()
    ds.SOPInstanceUID = sop_uid
    ds.PatientName = "Test^Patient"
    ds.StudyInstanceUID = "1.2.840.10008.5.1.4.1.1.2." + sop_uid[-3:]
    ds.SeriesInstanceUID = "1.2.840.10008.5.1.4.1.1.3." + sop_uid[-3:]
    # Ensure file_meta is present for dcmwrite
    from pydicom.dataset import FileMetaDataset
    from pydicom.uid import ImplicitVRLittleEndian

    ds.file_meta = FileMetaDataset()
    ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
    ds.file_meta.MediaStorageSOPInstanceUID = sop_uid
    ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian
    return ds


class TestOutboxQueue:
    """Core OutboxQueue tests."""

    def test_enqueue_dequeue(self, outbox):
        """Basic insert and retrieve — blob stored as file, not in SQLite."""
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.999")
        assert outbox.enqueue(ds) is True
        assert outbox.pending_count() == 1

        # Verify blob file exists on disk
        blob_path = outbox._dcm_path(outbox._pending, "1.2.840.10008.5.1.4.1.1.2.999")
        assert os.path.isfile(blob_path), f"Expected blob file at {blob_path}"

        sop_uid, ds_out, blob_path, raw_bytes = outbox.dequeue()
        assert sop_uid == "1.2.840.10008.5.1.4.1.1.2.999"
        assert ds_out is not None
        assert ds_out.PatientName == "Test^Patient"

        # Blob file still exists after dequeue (only removed by cleanup)
        assert os.path.isfile(blob_path), "Blob file should persist after dequeue"

    def test_dequeue_unescapes_a_uid_with_percent_and_slash(self, outbox):
        """A UID whose file-name form escaped `%` and `/` must round-trip
        through enqueue -> dequeue.

        `dequeue` used to re-escape the already-escaped name (`%2F` ->
        `%252F`), so the meta lookup opened a file that does not exist and the
        item stayed in pending/ forever.
        """
        from pydicom.dataset import FileMetaDataset
        from pydicom.uid import ImplicitVRLittleEndian

        uid = "1.2.3%4/5"
        ds = Dataset()
        ds.SOPInstanceUID = uid
        ds.PatientName = "Test^Patient"
        ds.StudyInstanceUID = "1.2.3.4.5"
        ds.SeriesInstanceUID = "1.2.3.4.5.6"
        ds.file_meta = FileMetaDataset()
        ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
        ds.file_meta.MediaStorageSOPInstanceUID = uid
        ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian

        assert outbox.enqueue(ds) is True
        # The on-disk name is the escaped form.
        assert os.path.isfile(outbox._dcm_path(outbox._pending, uid))

        sop_uid, ds_out, _blob, _raw = outbox.dequeue()
        assert sop_uid == uid, f"the UID must round-trip: {sop_uid!r}"
        assert str(ds_out.SOPInstanceUID) == uid
        # The item was claimed out of pending/ (moved to in_progress/), not
        # skipped forever.
        assert not os.path.isfile(outbox._dcm_path(outbox._pending, uid))
        assert os.path.isfile(outbox._dcm_path(outbox._in_progress, uid))

    def test_orphan_sweep_unescapes_a_uid_with_percent_and_slash(self, outbox):
        """An orphan blob whose UID contains `%` and `/` must be adopted under
        the SAME .meta name dequeue will look for.

        The orphan sweep derived the UID from the ESCAPED filename and handed
        it straight to `_meta_path`, which escaped it a second time - the
        .meta it created was under a name no read path can match, so the
        orphan was re-adopted (and re-logged) on every sweep and never became
        deliverable.
        """
        from clean_code.outbox import ENQUEUE_PUBLISH_GRACE_SEC
        from pydicom import dcmwrite
        from pydicom.dataset import FileMetaDataset
        from pydicom.uid import ImplicitVRLittleEndian

        uid = "1.2.3%4/5"
        ds = Dataset()
        ds.SOPInstanceUID = uid
        ds.PatientName = "Test^Patient"
        ds.StudyInstanceUID = "1.2.3.4.5"
        ds.SeriesInstanceUID = "1.2.3.4.5.6"
        ds.file_meta = FileMetaDataset()
        ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
        ds.file_meta.MediaStorageSOPInstanceUID = uid
        ds.file_meta.TransferSyntaxUID = ImplicitVRLittleEndian

        # Simulate a crashed enqueue: .dcm on disk, no .meta.
        blob_path = outbox._dcm_path(outbox._pending, uid)
        tmp_path = blob_path + ".tmp"
        dcmwrite(tmp_path, ds, enforce_file_format=True)
        os.replace(tmp_path, blob_path)
        aged = time.time() - (ENQUEUE_PUBLISH_GRACE_SEC + 60)
        os.utime(blob_path, (aged, aged))

        rec, _und = outbox.recover_stuck()
        assert rec == 1, "the orphan was adopted exactly once"
        # (a) the .meta is the one dequeue will look for.
        assert os.path.isfile(outbox._meta_path(outbox._pending, uid))

        # (b) a second sweep finds no orphan: nothing is re-created.
        rec2, _und2 = outbox.recover_stuck()
        assert rec2 == 0, "the adopted orphan must not be re-adopted"

        # And the adopted item is deliverable.
        sop_uid, ds_out, _blob, _raw = outbox.dequeue()
        assert sop_uid == uid, f"dequeue must find the adopted orphan: {sop_uid!r}"
        assert str(ds_out.SOPInstanceUID) == uid

    def test_dequeue_lazy_parse_unmodified(self, monkeypatch, outbox):
        """OPT-1: an UNmodified blob (no .mods sidecar) is parsed header-only
        (stop_before_pixels=True) - the drain only needs the header fields;
        the payload bytes ship as raw passthrough."""
        import clean_code.outbox as ob

        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.777")
        assert outbox.enqueue(ds) is True
        calls = []
        real_dcmread = ob.dcmread

        def _fake(path, **kw):
            calls.append(kw.get("stop_before_pixels"))
            return real_dcmread(path, **kw)

        monkeypatch.setattr(ob, "dcmread", _fake)
        _uid, dout, _bp, _raw = outbox.dequeue()
        assert dout is not None
        assert calls == [True], "unmodified -> stop_before_pixels=True (header only)"

    def test_dequeue_full_parse_when_modified(self, monkeypatch, outbox):
        """OPT-1: a blob WITH a .mods audit sidecar is fully parsed (no
        stop_before_pixels) - the drain re-serializes it and must keep the
        pixel element."""
        import clean_code.outbox as ob

        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.666")
        assert (
            outbox.enqueue(
                ds,
                modifications=[((0x0010, 0x0010), "Old^Name", "New^Name")],
            )
            is True
        )
        calls = []
        real_dcmread = ob.dcmread

        def _fake(path, **kw):
            calls.append(kw.get("stop_before_pixels"))
            return real_dcmread(path, **kw)

        monkeypatch.setattr(ob, "dcmread", _fake)
        _uid, dout, _bp, _raw = outbox.dequeue()
        assert dout is not None
        assert calls == [None], "modified -> full parse (no stop_before_pixels)"

    def test_duplicate_enqueue(self, outbox):
        """Idempotent: duplicate SOPInstanceUID → INSERT OR IGNORE, orphan file removed."""
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.888")
        assert outbox.enqueue(ds) is True

        blob_path = outbox._dcm_path(outbox._pending, "1.2.840.10008.5.1.4.1.1.2.888")
        assert os.path.isfile(blob_path)

        # Duplicate: should return "duplicate" (not False — distinguish dup from error)
        assert outbox.enqueue(ds) == "duplicate"
        assert outbox.pending_count() == 1
        # With duplicate check before write, the original file survives untouched
        assert os.path.isfile(blob_path), "Original blob file should survive duplicate"

    def test_mark_delivered(self, outbox):
        """mark_delivered removes SOP from pending count, blob file deleted (BUG-004)."""
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.777")
        outbox.enqueue(ds)
        assert outbox.pending_count() == 1

        blob_path = outbox._dcm_path(outbox._pending, "1.2.840.10008.5.1.4.1.1.2.777")

        outbox.mark_delivered("1.2.840.10008.5.1.4.1.1.2.777")
        assert outbox.pending_count() == 0

        # BUG-004: with cleanup=True (default), blob is deleted immediately
        assert not os.path.isfile(
            blob_path
        ), "Blob file should be deleted after delivery (cleanup=True)"

    def test_delivered_is_false_while_the_sop_is_still_queued(self, outbox):
        """2409: `delivered` is the true-mode readiness gate.

        A SOP the outbox is still holding is NOT delivered - that is the
        whole point of asking the origin only after the bridge has finished
        delivering.
        """
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.555")
        outbox.enqueue(ds)
        assert outbox.delivered("1.2.840.10008.5.1.4.1.1.2.555") is False

        outbox.mark_delivered("1.2.840.10008.5.1.4.1.1.2.555")
        assert outbox.delivered("1.2.840.10008.5.1.4.1.1.2.555") is True

    def test_delivered_is_false_for_an_in_flight_sop(self, outbox):
        """An item claimed by a worker (in_progress/) is still not delivered."""
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.556")
        outbox.enqueue(ds)
        outbox.dequeue()  # pending/ -> in_progress/
        assert outbox.delivered("1.2.840.10008.5.1.4.1.1.2.556") is False

    def test_delivered_is_false_for_an_undelivered_sop(self, outbox):
        """A SOP that exhausted its retries must never read as delivered."""
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.557")
        outbox.enqueue(ds)
        outbox.move_to_undelivered("1.2.840.10008.5.1.4.1.1.2.557")
        assert outbox.delivered("1.2.840.10008.5.1.4.1.1.2.557") is False

    def test_delivered_is_true_for_a_sop_never_enqueued(self, outbox):
        """A modality re-request for something the bridge is not holding.

        Nothing local stands between that SOP and the origin's own verdict,
        so the readiness gate must not block on it.
        """
        assert outbox.delivered("1.2.840.10008.5.1.4.1.1.2.558") is True

    def test_mark_failed(self, outbox):
        """mark_failed increments retry count but keeps SOP pending."""
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.666")
        outbox.enqueue(ds)

        outbox.mark_failed("1.2.840.10008.5.1.4.1.1.2.666", "Connection refused")
        # Still pending after mark_failed
        assert outbox.pending_count() == 1
        # Retry count incremented
        assert outbox._get_retry_count("1.2.840.10008.5.1.4.1.1.2.666") == 1

    def test_recovery_on_startup(self, outbox):
        """recover_stuck detects pending SOPs."""
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.555")
        outbox.enqueue(ds)

        rec, und = outbox.recover_stuck()
        assert outbox.pending_count() == 1

    def test_recovery_finds_orphan_files(self, outbox):
        """Recovery: blob file without SQLite row -> creates row.

        Scenario: enqueue writes file -> bridge crashes before INSERT.
        On recovery, the orphan file should be detected and a row created.

        The blob is aged past ENQUEUE_PUBLISH_GRACE_SEC first: a fresh .dcm
        without a .meta is an enqueue still in flight (enqueue writes the blob
        first and the ready-marker last), and adopting that one published a
        half-written instance.
        """
        sop_uid = "1.2.840.10008.5.1.4.1.1.2.orphan"
        ds = _make_dataset(sop_uid)

        # Write file manually (simulating crashed enqueue — .dcm on disk, no .meta).
        from pydicom import dcmwrite

        blob_path = outbox._dcm_path(outbox._pending, sop_uid)
        tmp_path = blob_path + ".tmp"
        dcmwrite(tmp_path, ds, enforce_file_format=True)
        os.replace(tmp_path, blob_path)
        assert os.path.isfile(blob_path)
        # The crash happened long enough ago that nothing is still writing.
        from clean_code.outbox import ENQUEUE_PUBLISH_GRACE_SEC

        aged = time.time() - (ENQUEUE_PUBLISH_GRACE_SEC + 60)
        os.utime(blob_path, (aged, aged))

        # No .meta → not counted
        assert outbox.pending_count() == 0

        # Recovery syncs from FS
        rec, und = outbox.recover_stuck()
        assert outbox.pending_count() == 1

        # Should be able to dequeue it
        uid, ds_out, bp, raw = outbox.dequeue()
        assert uid == sop_uid
        assert ds_out.PatientName == "Test^Patient"

    def test_close_is_noop(self, outbox):
        """close() is no-op on filesystem outbox."""
        # Create a file to verify close() doesn't delete it
        tmp_file = os.path.join(outbox._pending, "test.tmp")
        with open(tmp_file, "w") as f:
            f.write("garbage")
        assert os.path.isfile(tmp_file)

        outbox.close()
        # close() is no-op — files remain
        assert os.path.isfile(tmp_file)

    def test_cleanup_delivered_blobs(self, outbox):
        """cleanup=False retains delivered files in done/."""
        outbox.cleanup = False

        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.cleanup")
        outbox.enqueue(ds)
        outbox.mark_delivered("1.2.840.10008.5.1.4.1.1.2.cleanup")

        blob_path = outbox._dcm_path(outbox._done, "1.2.840.10008.5.1.4.1.1.2.cleanup")
        assert os.path.isfile(blob_path), "File retained in done/ when cleanup=False"

    def test_persistence_across_restart(self, outbox):
        """Outbox survives close/reopen."""
        from clean_code.outbox import OutboxQueue

        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.444")
        outbox.enqueue(ds)
        outbox_dir = outbox._dir
        outbox.close()

        # Reopen
        oq2 = OutboxQueue(outbox_dir=outbox_dir)
        try:
            assert oq2.pending_count() == 1
            sop_uid, ds_out, blob_path, raw_bytes = oq2.dequeue()
            assert sop_uid == "1.2.840.10008.5.1.4.1.1.2.444"
            assert ds_out.PatientName == "Test^Patient"
        finally:
            oq2.close()

    def test_empty_dequeue(self, outbox):
        """dequeue on empty queue returns (None, None, None, None)."""
        sop_uid, ds, blob_path, raw_bytes = outbox.dequeue()
        assert sop_uid is None
        assert ds is None
        assert blob_path is None
        assert raw_bytes is None

    def test_pending_count_zero_initially(self, outbox):
        """Fresh outbox has zero pending."""
        assert outbox.pending_count() == 0

    def test_next_retry_wait(self, outbox):
        """next_retry_wait_sec returns reasonable value."""
        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.333")
        outbox.enqueue(ds)

        wait = outbox.next_retry_wait_sec()
        assert wait >= 0
        assert wait <= 5.1  # Max 5.0 seconds

    def test_max_retries_exceeded(self, outbox, caplog):
        """After >10 retries, logs error about manual intervention."""
        import logging

        caplog.set_level(logging.ERROR)

        ds = _make_dataset("1.2.840.10008.5.1.4.1.1.2.222")
        outbox.enqueue(ds)

        # Simulate 11 failures
        for i in range(11):
            outbox.mark_failed("1.2.840.10008.5.1.4.1.1.2.222", f"Error #{i}")

        # Check that error was logged about max retries
        assert any("needs manual intervention" in rec.message for rec in caplog.records)

    def test_no_sop_uid(self, outbox):
        """enqueue on dataset without SOPInstanceUID returns False."""
        ds = Dataset()
        ds.PatientName = "NoSOP"
        assert outbox.enqueue(ds) is False

    def test_retry_count_per_item(self, outbox):
        """_get_retry_count returns correct count for individual items."""
        ds1 = _make_dataset("1.2.840.10008.5.1.4.1.1.2.111")
        ds2 = _make_dataset("1.2.840.10008.5.1.4.1.1.2.112")
        outbox.enqueue(ds1)
        outbox.enqueue(ds2)

        outbox.mark_failed("1.2.840.10008.5.1.4.1.1.2.111", "Err1")
        outbox.mark_failed("1.2.840.10008.5.1.4.1.1.2.111", "Err2")
        outbox.mark_failed("1.2.840.10008.5.1.4.1.1.2.112", "Err3")

        assert outbox._get_retry_count("1.2.840.10008.5.1.4.1.1.2.111") == 2
        assert outbox._get_retry_count("1.2.840.10008.5.1.4.1.1.2.112") == 1


class TestUndelivered:
    """P0-2: UNDELIVERED — изоляция проблемных файлов."""

    def test_move_to_undelivered_moves_blob(self, outbox):
        """move_to_undelivered: blob перемещается в undelivered_dir, строка удаляется."""

        sop_uid = "1.2.840.10008.5.1.4.1.1.2.und1"
        ds = _make_dataset(sop_uid)
        outbox.enqueue(ds)
        blob_path = outbox._dcm_path(outbox._pending, sop_uid)
        assert os.path.isfile(blob_path), "Blob should exist before move"

        assert outbox.pending_count() == 1

        outbox.move_to_undelivered(sop_uid)

        # Blob removed from pending/
        assert not os.path.isfile(blob_path), "Blob should be moved away"

        # Blob exists in undelivered/
        safe_uid = outbox._safe_uid(sop_uid)
        undelivered_path = os.path.join(outbox._undelivered, f"{safe_uid}.dcm")
        assert os.path.isfile(
            undelivered_path
        ), f"Blob should be in undelivered: {undelivered_path}"

        # Pending count decreased
        assert outbox.pending_count() == 0

    def test_move_to_undelivered_nonexistent_file(self, outbox):
        """move_to_undelivered: не падает если файла нет (уже удалён)."""
        sop_uid = "1.2.840.10008.5.1.4.1.1.2.und2"
        ds = _make_dataset(sop_uid)
        outbox.enqueue(ds)
        blob_path = outbox._dcm_path(outbox._pending, sop_uid)

        # Удаляем файл вручную
        os.unlink(blob_path)

        # Не должно упасть
        outbox.move_to_undelivered(sop_uid)
        assert outbox.pending_count() == 0


class TestOutboxSkipOnError:
    """P0-1: Outbox skip-on-error — один плохой SOP не блокирует очередь."""

    def test_max_retries_moves_to_undelivered(self, outbox):
        """После MAX_RETRIES неудач SOP паркуется в undelivered/ сам по себе.

        `mark_failed` calls `move_to_undelivered` when the counter reaches
        MAX_RETRIES, so the exhausted item leaves pending/ on its own and the
        healthy one behind it is the only candidate left. The assertion is
        therefore about the good SOP being delivered AND the bad one being
        parked by `mark_failed` alone - no manual move, and no branch that
        would also accept None.
        """
        sop_good = "1.2.840.10008.5.1.4.1.1.2.good"
        sop_bad = "1.2.840.10008.5.1.4.1.1.2.bad"

        # Enqueue 2 SOPs — good first (older), then bad
        ds_good = _make_dataset(sop_good)
        ds_bad = _make_dataset(sop_bad)
        outbox.enqueue(ds_good)
        outbox.enqueue(ds_bad)
        assert outbox.pending_count() == 2

        # Симулируем исчерпание retries для bad SOP
        for i in range(outbox.MAX_RETRIES + 1):
            outbox.mark_failed(sop_bad, f"Error #{i}")

        # mark_failed parks the item itself: no manual move_to_undelivered.
        parked = outbox._dcm_path(outbox._undelivered, sop_bad)
        assert os.path.isfile(parked), "MAX_RETRIES must park the blob"

        # dequeue must hand out the good SOP, exactly.
        uid, ds_out, bp, raw = outbox.dequeue()
        assert uid == sop_good, f"the parked SOP must not be handed out, got {uid}"
        assert ds_out.PatientName == "Test^Patient"

        # The bad item is not in pending/ any more (the filesystem is the
        # authority here - the counter is a gauge, not a claim record).
        assert not os.path.isfile(outbox._dcm_path(outbox._pending, sop_bad))
        # Nothing else is claimable.
        assert outbox.dequeue()[0] is None

    def test_an_exhausted_item_never_comes_back_out_of_the_queue(self, outbox):
        """After MAX_RETRIES the SOP is parked and cannot be dequeued again.

        This pins the PARKING path (`mark_failed` -> `move_to_undelivered`),
        which is what actually keeps an exhausted item out of the queue; the
        retry_count predicate in `dequeue` is covered separately by
        test_dequeue_skips_an_exhausted_pending_item, which needs a pending
        item that `mark_failed` never saw.
        """
        sop_good = "1.2.840.10008.5.1.4.1.1.2.g2"
        sop_bad = "1.2.840.10008.5.1.4.1.1.2.b2"

        ds_good = _make_dataset(sop_good)
        ds_bad = _make_dataset(sop_bad)
        outbox.enqueue(ds_good)
        outbox.enqueue(ds_bad)

        # Исчерпываем retries для bad
        for i in range(outbox.MAX_RETRIES):
            outbox.mark_failed(sop_bad, f"Error #{i}")

        # The exhausted blob is in undelivered/, so it is not a candidate.
        assert os.path.isfile(outbox._dcm_path(outbox._undelivered, sop_bad))

        uid, ds_out, bp, raw = outbox.dequeue()
        assert uid == sop_good, f"Expected good SOP, got {uid}"
        assert ds_out.PatientName == "Test^Patient"

    def test_dequeue_skips_an_exhausted_pending_item(self, outbox):
        """dequeue() must not hand out a pending item whose .meta is exhausted.

        `mark_failed` parks an item itself, so the retry_count predicate is
        only reachable through the RECOVERY path: a `.meta` written by a
        previous run (or restored from a backup) can carry
        `retry_count >= MAX_RETRIES` while the blob is still in pending/.

        The exhausted item is made the OLDEST of the two, because dequeue
        claims oldest-first: it is then the FIRST candidate the scan reaches.
        Without the predicate the scan claims it and returns it, whatever the
        healthy item behind it would have done - which is what makes the
        assertion about the filter rather than about the ordering.
        """
        import json
        import os as _os

        sop_good = "1.2.840.10008.5.1.4.1.1.2.g3"
        sop_bad = "1.2.840.10008.5.1.4.1.1.2.b3"

        # Enqueue the BAD one first so its .meta is the older publish.
        outbox.enqueue(_make_dataset(sop_bad))
        outbox.enqueue(_make_dataset(sop_good))

        # Rewrite the bad item's .meta as an exhausted record, leaving the
        # blob where it is (the crashed-run / restored-backup shape).
        meta_path = outbox._meta_path(outbox._pending, sop_bad)
        with open(meta_path, encoding="utf-8") as f:
            meta = json.load(f)
        meta["retry_count"] = outbox.MAX_RETRIES
        meta["next_retry_at"] = 0  # ready by time, so only the count blocks it
        with open(meta_path, "w", encoding="utf-8") as f:
            json.dump(meta, f)
        # The .meta mtime is the dequeue sort key: keep the exhausted item
        # unambiguously first.
        _t = _os.path.getmtime(meta_path) - 10
        _os.utime(meta_path, (_t, _t))
        outbox._pending_meta_cache = None

        uid, ds_out, bp, raw = outbox.dequeue()
        assert uid == sop_good, f"an exhausted pending item must be skipped, got {uid}"
        assert ds_out.PatientName == "Test^Patient"

    def test_dequeue_does_not_block_on_failure(self, outbox):
        """После mark_failed dequeue НЕ спит — сразу возвращает управление.

        P0-1: nginx-way — не блокируем очередь. Ошибка одного SOP
        не должна вызывать time.sleep() в dequeue().
        """
        import time as _time

        sop = "1.2.840.10008.5.1.4.1.1.2.noblock"
        ds = _make_dataset(sop)
        outbox.enqueue(ds)

        # Удаляем blob файл — симулируем битый файл
        blob_path = outbox._dcm_path(outbox._pending, sop)
        os.unlink(blob_path)

        t0 = _time.time()
        uid, ds_out, bp, raw = outbox.dequeue()
        elapsed = _time.time() - t0

        # dequeue НЕ должен спать — должен вернуться < 0.1s
        assert (
            elapsed < 0.5
        ), f"dequeue() took {elapsed:.2f}s — should be <0.5s (no sleep)"
        assert uid is None, "Should return None for broken blob"


# ---------------------------------------------------------------------------
# Outbox worker: same-study SOPs delivered as ONE multipart STOW-RS batch.
# Stage 1 (2208 hardening): N SOPs = 1 POST (fieldtest 1063 -> ~20).
# ---------------------------------------------------------------------------


class TestOutboxWorkerBatching:
    """_outbox_worker batches same-study instances via stow_batch_instances."""

    def _run_worker_until_call(self, outbox, batch_max_sop, batch_max_bytes):
        """Run worker; return the payload list of the FIRST stow_batch_instances call."""
        import threading as _th
        from unittest.mock import patch

        from clean_code.main import _outbox_worker

        shutdown = _th.Event()
        call_payloads = []
        call_done = _th.Event()

        def _fake_batch(payloads, clarus_url, http_timeout, client, tls=None):
            call_payloads.extend(payloads)
            call_done.set()
            # All-success per-SOP outcomes.
            return ([(uid, 0x0000, 200, "OK") for uid, _, _ in payloads], None)

        with patch("clean_code.main._shutdown_requested", shutdown), patch(
            "clean_code.http_helpers.stow_batch_instances", side_effect=_fake_batch
        ):
            t = _th.Thread(
                target=_outbox_worker,
                args=(
                    outbox,
                    "http://127.0.0.1:8024/dicomweb",
                    30,
                    None,
                    None,
                    batch_max_sop,
                    batch_max_bytes,
                    0.2,
                ),
                daemon=True,
            )
            t.start()
            # Wait until the worker flushed the whole same-study queue once.
            assert call_done.wait(timeout=10), "worker never flushed a batch"
            shutdown.set()
            t.join(timeout=10)

        return call_payloads

    def test_batches_same_study_into_one_post(self, outbox):
        """N same-study SOPs -> stow_batch_instances called ONCE with all N."""
        n = 5
        for i in range(n):
            ds = _make_dataset(f"1.2.3.4.5.6.7.{i}")
            # _make_dataset derives study from the last 3 chars -> force ONE study.
            ds.StudyInstanceUID = "1.2.840.12345"
            outbox.enqueue(ds)

        payloads = self._run_worker_until_call(
            outbox,
            batch_max_sop=n,
            batch_max_bytes=1024 * 1024,
        )
        # One batch POST carrying all N SOPs of the same study.
        assert len(payloads) == n
        # All delivered — queue drained.
        assert outbox.pending_count() == 0
        # No SOP left in_progress.
        assert not os.listdir(outbox._in_progress)

    def test_study_boundary_does_not_mix_studies(self, outbox):
        """Two studies -> batches never mix studies."""
        import threading as _th
        from unittest.mock import patch

        from clean_code.main import _outbox_worker

        shutdown = _th.Event()
        batches = []  # each element = list of UIDs in one flush
        flush_seen = _th.Event()

        def _fake_batch(payloads, clarus_url, http_timeout, client, tls=None):
            batches.append([uid for uid, _, _ in payloads])
            flush_seen.set()
            return ([(uid, 0x0000, 200, "OK") for uid, _, _ in payloads], None)

        # Enqueue 3 SOPs of study A, then 3 of study B (queue order A,A,A,B,B,B).
        for i in range(3):
            ds = _make_dataset(f"1.2.840.a.{i}")
            ds.StudyInstanceUID = "STUDY_A"
            outbox.enqueue(ds)
        for i in range(3):
            ds = _make_dataset(f"1.2.840.b.{i}")
            ds.StudyInstanceUID = "STUDY_B"
            outbox.enqueue(ds)

        with patch("clean_code.main._shutdown_requested", shutdown), patch(
            "clean_code.http_helpers.stow_batch_instances", side_effect=_fake_batch
        ):
            t = _th.Thread(
                target=_outbox_worker,
                args=(
                    outbox,
                    "http://127.0.0.1:8024/dicomweb",
                    30,
                    None,
                    None,
                    100,
                    1024 * 1024,
                    0.1,
                ),  # big count -> flush on boundary/idle
                daemon=True,
            )
            t.start()
            # Wait for at least one flush (first study boundary or idle flush).
            assert flush_seen.wait(timeout=10), "worker never flushed"
            # Give the worker time to drain both studies.
            timeout_at = time.time() + 10
            while outbox.pending_count() > 0 and time.time() < timeout_at:
                time.sleep(0.05)
            shutdown.set()
            t.join(timeout=10)

        # Both studies delivered; every flush holds SOPs of ONE study only.
        assert outbox.pending_count() == 0
        assert len(batches) >= 2, f"expected >=2 flushes, got {len(batches)}"
        for uids in batches:
            a = any(u.startswith("1.2.840.a.") for u in uids)
            b = any(u.startswith("1.2.840.b.") for u in uids)
            assert not (a and b), f"batch mixes two studies: {uids}"

    def test_507_backpressure_keeps_files_pending_no_retry_bump(self, outbox):
        """2908: 507 -> files requeued to pending, retry_count stays 0, worker pauses."""
        import threading as _th
        from unittest.mock import patch

        from clean_code.main import _outbox_worker

        sop = "1.2.840.507.1"
        ds = _make_dataset(sop)
        ds.StudyInstanceUID = "STUDY_507"
        outbox.enqueue(ds)

        class _FakeShutdown:
            def __init__(self):
                self._set = False

            def is_set(self):
                return self._set

            def wait(self, timeout=None):
                return self._set

            def set(self):
                self._set = True

        shutdown = _FakeShutdown()
        calls = [0]

        def _fake_batch(payloads, clarus_url, http_timeout, client, tls=None):
            calls[0] += 1
            return (
                [
                    (uid, 0xA700, 507, "Disk full - retry after 30s")
                    for uid, _, _ in payloads
                ],
                30,
            )

        with patch("clean_code.main._shutdown_requested", shutdown), patch(
            "clean_code.http_helpers.stow_batch_instances", side_effect=_fake_batch
        ):
            t = _th.Thread(
                target=_outbox_worker,
                args=(
                    outbox,
                    "http://127.0.0.1:8024/dicomweb",
                    30,
                    None,
                    None,
                    100,
                    1024 * 1024,
                    0.1,
                ),
                daemon=True,
            )
            t.start()
            # Wait until the 507 batch was seen and requeued.
            deadline = time.time() + 10
            while calls[0] == 0 and time.time() < deadline:
                time.sleep(0.05)
            assert calls[0] == 1, "worker never attempted the batch"
            # The 507 pause (30s) must prevent further STOW attempts.
            time.sleep(0.3)
            shutdown.set()
            t.join(timeout=10)

        # File stays pending, retry count NOT bumped (507 is backpressure).
        assert outbox.pending_count() == 1
        assert calls[0] == 1, "no retry inside the 507 pause"
        assert outbox._get_retry_count(sop) == 0


class TestEnsureFileMeta:
    """Fix C (2408_writer_fieldfixes_abc, commit 3): _ensure_file_meta must
    ALWAYS salvage stray group-0002 elements out of the DIMSE dataset body
    into file_meta, so pydicom does not reject the write (Weasis C-STORE
    0xA700 "must be in a FileMetaDataset instance")."""

    def test_salvage_stray_0002_keeps_negotiated_ts(self):
        """Valid file_meta (negotiated TS) + stray (0002,0012) in the body.

        _ensure_file_meta moves (0002,0012) into file_meta BEFORE any early
        return, keeps the negotiated TransferSyntaxUID, and leaves the dataset
        writable to bytes. RED pre-fix: the early return left (0002,0012) in
        the body and dcmwrite() raised.
        """
        from io import BytesIO

        from clean_code.outbox import _ensure_file_meta
        from pydicom import dcmwrite
        from pydicom.dataelem import DataElement
        from pydicom.dataset import FileMetaDataset
        from pydicom.uid import ExplicitVRLittleEndian

        ds = Dataset()
        ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.7"
        ds.SOPInstanceUID = "1.2.840.10008.5.1.4.1.1.2.901"
        ds.PatientName = "Test^Patient"
        fm = FileMetaDataset()
        fm.MediaStorageSOPClassUID = ds.SOPClassUID
        fm.MediaStorageSOPInstanceUID = ds.SOPInstanceUID
        fm.TransferSyntaxUID = ExplicitVRLittleEndian
        ds.file_meta = fm
        # Stray group-0002 element in the DIMSE body (normally in file_meta).
        ds.add(DataElement(0x00020012, "UI", "1.2.840.10008.15.1.1"))

        _ensure_file_meta(ds)

        assert (
            ds.file_meta.TransferSyntaxUID == ExplicitVRLittleEndian
        ), "negotiated TS must survive"
        assert 0x00020012 in ds.file_meta, "(0002,0012) must be in file_meta"
        assert 0x00020012 not in ds, "(0002,0012) must leave the dataset body"
        # Writing to bytes must succeed now (no inline (0002,eeee) rejection).
        buf = BytesIO()
        dcmwrite(buf, ds, enforce_file_format=True)
        assert buf.getvalue()

    def test_salvage_ts_from_body_without_file_meta(self):
        """No file_meta + (0002,0010) in the body (cltranscode-style).

        Salvage moves the group into a fresh file_meta; the TransferSyntaxUID
        from the dataset body is preserved as the resulting file_meta TS.
        """
        from clean_code.outbox import _ensure_file_meta
        from pydicom.dataelem import DataElement
        from pydicom.dataset import FileMetaDataset
        from pydicom.uid import ExplicitVRLittleEndian

        ds = Dataset()
        ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        ds.SOPInstanceUID = "1.2.840.10008.5.1.4.1.1.2.902"
        # cltranscode inserts (0002,0010) into the DIMSE body with no file_meta.
        ds.add(DataElement(0x00020010, "UI", ExplicitVRLittleEndian))

        _ensure_file_meta(ds)

        assert isinstance(ds.file_meta, FileMetaDataset)
        assert 0x00020010 in ds.file_meta, "(0002,0010) must be salvaged"
        assert 0x00020010 not in ds, "(0002,0010) must leave the dataset body"
        assert (
            ds.file_meta.TransferSyntaxUID == ExplicitVRLittleEndian
        ), "body TS must be preserved"

    def test_no_ts_falls_back_to_evrle_with_warning(self, caplog):
        """No file_meta and no TransferSyntaxUID -> EVRLE fallback + WARNING."""
        import logging

        from clean_code.outbox import _ensure_file_meta
        from pydicom.uid import ExplicitVRLittleEndian

        ds = Dataset()
        ds.SOPInstanceUID = "1.2.840.10008.5.1.4.1.1.2.903"
        with caplog.at_level(logging.WARNING):
            _ensure_file_meta(ds)

        assert ds.file_meta.TransferSyntaxUID == ExplicitVRLittleEndian
        assert any("no TransferSyntaxUID" in r.message for r in caplog.records)


class TestOutboxWorkerFailures:
    """A failed batch must not stall the drain thread: per-SOP backoff in the
    outbox already keeps a retrying SOP out of the ready set, so no sleep is
    allowed in the per-SOP failure branch."""

    def test_failed_batch_sleeps_not_once_and_backs_off_every_sop(self, outbox):
        import threading as _th
        from unittest.mock import patch

        from clean_code.main import _outbox_worker

        n = 3
        uids = [f"1.2.840.fail.{i}" for i in range(n)]
        for uid in uids:
            ds = _make_dataset(uid)
            ds.StudyInstanceUID = "STUDY_FAIL"
            outbox.enqueue(ds)

        shutdown = _th.Event()
        sleep_calls = []
        flushed = _th.Event()

        def _fake_batch(payloads, clarus_url, http_timeout, client, tls=None):
            flushed.set()
            return (
                [(uid, 0xC000, 500, "boom") for uid, _, _ in payloads],
                None,
            )

        real_sleep = time.sleep

        def _recording_sleep(seconds):
            # Only record the drain path; the worker's own idle park is a
            # different call site and would mask a missing backoff.
            if seconds == 2.0:
                sleep_calls.append(seconds)
            real_sleep(seconds)

        began = time.time()
        with patch("clean_code.main._shutdown_requested", shutdown), patch(
            "clean_code.http_helpers.stow_batch_instances", side_effect=_fake_batch
        ), patch("time.sleep", side_effect=_recording_sleep):
            t = _th.Thread(
                target=_outbox_worker,
                args=(
                    outbox,
                    "http://127.0.0.1:8024/dicomweb",
                    30,
                    None,
                    None,
                    n,
                    1024 * 1024,
                    0.1,
                ),
                daemon=True,
            )
            t.start()
            assert flushed.wait(timeout=10), "worker never flushed a batch"
            shutdown.set()
            t.join(timeout=10)
        elapsed = time.time() - began

        assert sleep_calls == [], f"per-SOP sleep still taken: {sleep_calls}"
        assert outbox.pending_count() == n, "failed SOPs must stay pending"
        assert outbox._get_retry_count(uids[0]) == 1
        # Backoff, not a sleep, keeps them out of the ready set.
        import json

        now = time.time()
        for uid in uids:
            with open(outbox._meta_path(outbox._pending, uid), encoding="utf-8") as fh:
                meta = json.load(fh)
            assert meta["next_retry_at"] > now, uid
        # No per-SOP 2 s * 3 = 6 s stall on the drain thread.
        assert elapsed < 6.0, elapsed
