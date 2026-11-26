# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""P9 - C-FIND status conformance, MWL interval matching, outbox accounting.

Each test fails when its fix is reverted.
"""

from pydicom.dataset import Dataset


# ---------------------------------------------------------------------------
# cfind.py - 0xB001 is undefined for Query/Retrieve; incomplete is 0xA700
# ---------------------------------------------------------------------------
class _Qido:
    """Minimal stand-in for `_qido_request` pages."""

    def __init__(self, rows, more=False):
        self.rows = rows
        self.more = more


def _event(level="STUDY"):
    from tests.conftest import make_fake_find_event

    event = make_fake_find_event()
    event.identifier.QueryRetrieveLevel = level
    return event


class TestIncompleteStatusIsDefined:
    def test_a_capped_study_search_refuses_instead_of_warning(self, monkeypatch):
        """PS3.4 Table C.4.1-1 defines no 0xB001 for C-FIND: answering it is
        sending a status the SCU has no contract for."""
        from clean_code.handlers import cfind

        rows = [{"0020000D": {"Value": ["1.2.3"]}}, {"0020000D": {"Value": ["1.2.4"]}}]
        monkeypatch.setattr(cfind, "_qido_request", lambda *a, **k: rows)

        out = list(
            cfind._handle_qido_search(
                _event(),
                "http://h/dicomweb",
                False,
                1,
                5,
                {"PatientName": "*"},
                "study",
            )
        )

        statuses = [s for s, _ in out]
        assert statuses[-1] == 0xA700, statuses
        assert 0xB001 not in statuses, "0xB001 is not a defined C-FIND status"

    def test_a_complete_search_still_succeeds(self, monkeypatch):
        from clean_code.handlers import cfind

        rows = [{"0020000D": {"Value": ["1.2.3"]}}]
        monkeypatch.setattr(cfind, "_qido_request", lambda *a, **k: rows)

        out = list(
            cfind._handle_qido_search(
                _event(),
                "http://h/dicomweb",
                False,
                10,
                5,
                {"PatientName": "*"},
                "study",
            )
        )

        assert [s for s, _ in out][-1] == 0x0000


class TestCancelBeforeTerminalStatus:
    def test_a_cancel_during_the_request_answers_cancelled(self, monkeypatch):
        """PS3.7 9.3.2: the SCU expects 0xFE00, not Success, for a search it
        abandoned while the origin was answering.

        The cancel must arrive DURING the HTTP request and the answer must
        carry no rows: an empty page means no loop iteration ever runs, so the
        in-loop check cannot see it and only the terminal-status check can.
        """
        from clean_code.handlers import cfind

        event = _event()

        def _qido(*a, **k):
            # The SCU cancels while the origin is still answering.
            event.is_cancelled = True
            return []

        monkeypatch.setattr(cfind, "_qido_request", _qido)

        out = list(
            cfind._handle_qido_search(
                event, "http://h/dicomweb", False, 10, 5, {"PatientName": "*"}, "study"
            )
        )

        assert [s for s, _ in out] == [0xFE00]


class TestParentRowsNotLimitedByResponseBudget:
    def test_a_match_in_a_later_study_is_found_with_a_limit_of_one(self, monkeypatch):
        """The response budget limits matching DESPENDANTS. Capping the study
        hop at the same number meant a match in the third study was never even
        requested."""
        from clean_code.handlers import cfind

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"

        study_rows = [{"0020000D": {"Value": [f"1.2.3.{n}"]}} for n in range(3)]

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                # A real origin honours `limit`; the fake must too, or the cap
                # has nothing to bite on and the test proves nothing.
                if limit is not None:
                    return study_rows[:limit]
                return study_rows
            # Only the LAST study has a matching series.
            if "1.2.3.2" in url:
                return [{"0020000E": {"Value": ["1.2.3.2.1"]}}]
            return []

        monkeypatch.setattr(cfind, "_qido_request", fake_qido)
        out = list(
            cfind._handle_series_search(
                _event("SERIES"), "http://h/dicomweb", False, 1, 5
            )
        )

        sent = [ds for s, ds in out if s == 0xFF00]
        assert len(sent) == 1, f"the third study's match was never requested: {sent!r}"


# ---------------------------------------------------------------------------
# protocol.py - MWL date and time describe ONE interval
# ---------------------------------------------------------------------------
class TestMwlCombinedInterval:
    def test_the_time_window_applies_within_each_day_of_the_date_range(self):
        """PS3.4 Table K.6-1 defines the two keys as ONE interval: the date
        range selects the DAYS and the time range selects the HOURS inside
        each of them.

        Concatenating the bounds into a single span
        (`20260705100000..20260707180000`) accepted July 6 at 22:00 as well,
        because that instant lies between the two endpoints - so the filter
        silently widened every multi-day request.
        """
        from clean_code.protocol import (
            _mwl_datetime_in_bounds,
            _mwl_start_datetime_bounds,
        )

        ds = Dataset()
        item = Dataset()
        item.ScheduledProcedureStepStartDate = "20260705-20260707"
        item.ScheduledProcedureStepStartTime = "1000-1800"
        ds.ScheduledProcedureStepSequence = [item]

        bounds = _mwl_start_datetime_bounds(ds)
        # Inside the date range AND inside the hours: kept.
        assert _mwl_datetime_in_bounds("20260706120000", bounds) is True
        assert _mwl_datetime_in_bounds("20260705100000", bounds) is True
        assert _mwl_datetime_in_bounds("20260707180000", bounds) is True
        # Inside the date range but outside the hours: dropped.
        assert _mwl_datetime_in_bounds("20260706220000", bounds) is False
        # Outside the date range: dropped.
        assert _mwl_datetime_in_bounds("20260704120000", bounds) is False
        assert _mwl_datetime_in_bounds("20260708120000", bounds) is False

    def test_a_time_only_request_still_compares_time_of_day(self):
        """The combined interval must not break the time-only case, whose
        bounds carry no date at all."""
        from clean_code.protocol import (
            _mwl_datetime_in_bounds,
            _mwl_start_datetime_bounds,
        )

        ds = Dataset()
        item = Dataset()
        item.ScheduledProcedureStepStartTime = "0900-1000"
        ds.ScheduledProcedureStepSequence = [item]

        bounds = _mwl_start_datetime_bounds(ds)
        assert _mwl_datetime_in_bounds("20261121093000", bounds) is True
        assert _mwl_datetime_in_bounds("20261121140000", bounds) is False


# ---------------------------------------------------------------------------
# outbox.py - the byte budget must be measurable and must not leak
# ---------------------------------------------------------------------------
class TestOutboxByteAccounting:
    def test_the_admission_estimate_is_non_zero_for_a_real_dataset(self):
        """pydicom 3.0.1's Dataset has no `to_bytes()`, so the old estimate
        raised and returned 0 for EVERY instance - the admission check
        reserved nothing."""
        from clean_code.outbox import OutboxQueue

        ds = Dataset()
        ds.PatientName = "Doe^John"
        ds.StudyInstanceUID = "1.2.3"
        ds.PixelData = b"\x00" * 4096

        assert OutboxQueue._incoming_bytes(ds) > 4096

    def test_a_failed_enqueue_returns_its_byte_reservation(self, tmp_path):
        """Repeated failures of a large instance must not exhaust the budget
        after every failed blob has been removed."""
        from clean_code.outbox import OutboxQueue

        q = OutboxQueue(
            outbox_dir=str(tmp_path / "ob"),
            max_queue=10,
            max_retries=3,
            cleanup=False,
            retry_interval=1,
            max_queue_bytes=10 * 1024 * 1024,
        )
        before = q._reserved_bytes

        ds = Dataset()
        ds.SOPInstanceUID = "1.2.3.4"
        ds.PatientName = "Doe^John"
        ds.PixelData = b"\x00" * 2048

        # Force the blob write to fail, which is one of the failure paths that
        # must hand the reservation back.
        import clean_code.outbox as ob

        original = ob.dcmwrite
        ob.dcmwrite = lambda *a, **k: (_ for _ in ()).throw(OSError("disk full"))
        try:
            assert q.enqueue(ds) is False
        finally:
            ob.dcmwrite = original

        assert (
            q._reserved_bytes == before
        ), f"the failed enqueue leaked {q._reserved_bytes - before} reserved bytes"
        assert q._reserved_names == set()
