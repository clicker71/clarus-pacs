# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P9 REST - MWL parent paging, outbox byte ledger, cmove resume reconciliation.

Each test fails when its fix is reverted.
"""

from pydicom.dataset import Dataset


def _dataset(sop_uid, size=4096):
    """A writable dataset: dcmwrite requires Media Storage file meta."""
    import pydicom

    ds = Dataset()
    ds.SOPInstanceUID = sop_uid
    ds.SOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
    ds.PatientName = "Doe^John"
    # PixelData's VR is ambiguous (OB or OW) without these, and dcmwrite
    # refuses to guess.
    ds.BitsAllocated = 8
    ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelRepresentation = 0
    ds.Rows = 1
    ds.Columns = size
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.PixelData = b"\x00" * size
    ds.file_meta = pydicom.dataset.FileMetaDataset()
    ds.file_meta.TransferSyntaxUID = pydicom.uid.ExplicitVRLittleEndian
    ds.file_meta.MediaStorageSOPClassUID = ds.SOPClassUID
    ds.file_meta.MediaStorageSOPInstanceUID = sop_uid
    return ds


# ---------------------------------------------------------------------------
# cfind.py:292 - the MWL limit counts MATCHING rows, not candidates
# ---------------------------------------------------------------------------
class TestMwlLimitCountsMatches:
    def test_a_matching_row_behind_rejected_candidates_is_found(self, monkeypatch):
        """With a limit of one the old code fetched `max_responses * 4 + 1`
        candidates and gave up. A page of out-of-window rows could hide the
        only matching row behind them."""
        from clean_code.handlers import cfind

        # Every page is FULL of out-of-window rows until the last one, which
        # holds the single matching workitem. A short first page would end the
        # walk before the second was ever requested.
        outside = {"00404005": {"Value": ["20260706100000"]}}

        def fake_qido(url, params, timeout, limit=None, offset=0, tls=None):
            assert limit, "the paged walk must send a page size"
            # Page 0 is COMPLETELY out of window and exactly `limit` rows long
            # (a real origin fills a page). Page 1 starts with the matching
            # workitem and is still full, so only the match itself ends the
            # walk - and a walk that stops after page 0 never sees it.
            if offset >= limit:
                return [
                    {"00404005": {"Value": ["20260706220000"]}},
                ] + [
                    dict(outside)
                ] * (limit - 1)
            return [dict(outside)] * limit

        ds = Dataset()
        item = Dataset()
        item.ScheduledProcedureStepStartDate = "20260705-20260707"
        item.ScheduledProcedureStepStartTime = "2100-2300"
        ds.ScheduledProcedureStepSequence = [item]

        from tests.conftest import make_fake_find_event

        event = make_fake_find_event()
        event.identifier = ds

        monkeypatch.setattr(cfind, "_qido_request", fake_qido)

        out = list(cfind._handle_mwl(event, "http://h/dicomweb", False, 1, 5))

        sent = [row for status, row in out if status == 0xFF00]
        assert len(sent) == 1, f"the matching row behind page 1 was not found: {sent}"

    def test_no_cap_reads_the_single_answer_once(self, monkeypatch):
        from clean_code.handlers import cfind

        from tests.conftest import make_fake_find_event

        calls = []

        def fake_qido(url, params, timeout, limit=None, offset=0, tls=None):
            calls.append(offset)
            return [{"00404005": {"Value": ["20260706120000"]}}]

        monkeypatch.setattr(cfind, "_qido_request", fake_qido)
        event = make_fake_find_event()

        out = list(cfind._handle_mwl(event, "http://h/dicomweb", False, 0, 5))

        assert [s for s, _ in out][-1] == 0x0000
        assert len(calls) == 1


# ---------------------------------------------------------------------------
# outbox.py:217 - admission uses the ledger, not a directory walk
# ---------------------------------------------------------------------------
class TestOutboxByteLedger:
    def _queue(self, tmp_path, max_bytes):
        from clean_code.outbox import OutboxQueue

        return OutboxQueue(
            outbox_dir=str(tmp_path / "ob"),
            max_queue=100,
            max_retries=3,
            cleanup=False,
            retry_interval=1,
            max_queue_bytes=max_bytes,
        )

    def test_a_published_blob_enters_the_ledger(self, tmp_path):
        q = self._queue(tmp_path, 10 * 1024 * 1024)
        ds = _dataset("1.2.3")

        assert q.enqueue(ds) is True
        with q._pending_lock:
            ledger = q._pending_bytes
        assert ledger is not None and ledger >= 4096, ledger

    def test_admission_does_not_walk_the_directory(self, tmp_path, monkeypatch):
        """The walk cost one `getsize` per pending blob on EVERY admitted
        C-STORE. The ledger replaces it."""
        q = self._queue(tmp_path, 10 * 1024 * 1024)
        for n in range(3):
            assert q.enqueue(_dataset(f"1.2.3.{n}", 512)) is True

        def _boom():
            raise AssertionError("admission walked the pending directory")

        monkeypatch.setattr(q, "_pending_bytes_walk", _boom)

        assert q.enqueue(_dataset("1.2.3.99", 512)) is True

    def test_delivery_releases_its_bytes(self, tmp_path):
        q = self._queue(tmp_path, 10 * 1024 * 1024)
        q.enqueue(_dataset("1.2.3"))
        with q._pending_lock:
            after_enqueue = q._pending_bytes

        q.mark_delivered("1.2.3")
        with q._pending_lock:
            after_delivery = q._pending_bytes

        assert (
            after_delivery < after_enqueue
        ), f"delivery kept {after_enqueue - after_delivery} bytes charged"

    def test_in_flight_slots_count_against_the_queue_cap(self, tmp_path):
        """With `max_queue=1` two UIDs both passed the slot check before
        either incremented the count."""
        from clean_code.outbox import OutboxQueue

        q = OutboxQueue(
            outbox_dir=str(tmp_path / "ob"),
            max_queue=1,
            max_retries=3,
            cleanup=False,
            retry_interval=1,
        )
        # Simulate a producer that was admitted and has not published yet.
        with q._pending_lock:
            q._reserved_names.add("other.dcm")

        assert (
            q.enqueue(_dataset("1.2.3", 64)) is False
        ), "an in-flight slot was not counted"
