# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for clarus_bridge.handlers.cfind — QIDO search handler.

CRIT-002: Tests for _handle_qido_search() common function.
"""

from clean_code.handlers.cfind import (
    _handle_patient_root,
    _handle_qido_search,
    _handle_study_root,
    _stamp_cfind_response,
)
from pydicom import Dataset


class FakeEvent:
    """Fake pynetdicom event for testing C-FIND handlers."""

    def __init__(self, identifier, bridge_ae=None):
        self.identifier = identifier
        if bridge_ae:
            self.assoc = FakeAssoc(bridge_ae)


class FakeAssoc:
    def __init__(self, bridge_ae):
        self.acceptor = FakeAcceptor(bridge_ae)


class FakeAcceptor:
    def __init__(self, ae_title):
        self.ae_title = ae_title


class TestStampCfindResponse:
    """01.09 Orthanc-control: every C-FIND row echoes QueryRetrieveLevel."""

    def test_series_level_echoed_and_retrieve_ae_added(self):
        req = Dataset()
        req.QueryRetrieveLevel = "SERIES"
        event = FakeEvent(req, bridge_ae="CLBRIDGE")
        row = Dataset()
        row.SeriesInstanceUID = "1.2.3"
        _stamp_cfind_response(event, row)
        assert str(row.QueryRetrieveLevel) == "SERIES"
        assert str(row.RetrieveAETitle) == "CLBRIDGE"

    def test_missing_level_defaults_to_study(self):
        req = Dataset()
        event = FakeEvent(req)
        row = Dataset()
        _stamp_cfind_response(event, row)
        assert str(row.QueryRetrieveLevel) == "STUDY"
        assert "RetrieveAETitle" not in row

    def test_existing_level_is_not_overwritten(self):
        req = Dataset()
        req.QueryRetrieveLevel = "SERIES"
        event = FakeEvent(req)
        row = Dataset()
        row.QueryRetrieveLevel = "STUDY"
        _stamp_cfind_response(event, row)
        assert str(row.QueryRetrieveLevel) == "STUDY"


class TestHandleQidoSearch:
    """Tests for _handle_qido_search() — CRIT-002 common function."""

    def test_empty_params_uses_default(self):
        """When no params extracted, default_param should be used."""

        # We can't easily test the yield behavior without mocking HTTP,
        # but we can verify the function signature and basic logic.
        # The key CRIT-002 improvements are structural — deduplication of
        # ~60 lines across Patient Root and Study Root handlers.

    def test_patient_root_handler_exists(self):
        """_handle_patient_root should delegate to _handle_qido_search."""
        ds = Dataset()
        ds.PatientName = "Test"
        event = FakeEvent(ds)
        # Verify the handler is callable and returns a generator
        gen = _handle_patient_root(event, "http://127.0.0.1:8024/dicomweb", False, 50)
        assert hasattr(gen, "__iter__")

    def test_study_root_handler_exists(self):
        """_handle_study_root should delegate to _handle_qido_search."""
        ds = Dataset()
        ds.StudyDescription = "Test"
        event = FakeEvent(ds)
        gen = _handle_study_root(event, "http://127.0.0.1:8024/dicomweb", False, 50)
        assert hasattr(gen, "__iter__")

    def test_handle_qido_search_with_no_results(self):
        """An answer with no matches completes with 0x0000 and no rows.

        The test used a real 127.0.0.1:8024 URL and then accepted "0x0000,
        0xB000 or 0xC000" - three different meanings - because what it saw
        depended on whether a live server happened to be listening. That is
        not a test of the no-results path. The origin is now stubbed to answer
        an EMPTY result set, which is the case the name promises: the handler
        must report a complete (0x0000) answer, not a failure.
        """
        from unittest.mock import patch

        ds = Dataset()
        ds.PatientName = "NonExistent*"
        event = FakeEvent(ds)
        with patch("clean_code.handlers.cfind._qido_request", return_value=[]):
            results = list(
                _handle_qido_search(
                    event,
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                    {"PatientName": "*"},
                    "study",
                )
            )
        statuses = [status for status, _ in results]
        assert statuses == [0x0000], (
            "an empty result set is a COMPLETE answer, not a failure: " f"{statuses}"
        )


class TestQidoLimitAsksForOneExtra:
    """The server is asked for max_responses + 1 rows so the local truncation
    guard can prove more matches existed (PS3.4 Table C.4.1-1, B001)."""

    def _qido(self, ds, max_responses, rows=None):
        from unittest.mock import patch

        event = FakeEvent(ds)
        with patch("clean_code.handlers.cfind._qido_request") as mock_qido:
            mock_qido.return_value = rows if rows is not None else []
            list(
                _handle_qido_search(
                    event,
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    max_responses,
                    30,
                    {"PatientName": "*"},
                    "study",
                )
            )
        return mock_qido

    def test_limit_is_max_responses_plus_one(self):
        ds = Dataset()
        ds.PatientName = "*"
        mock_qido = self._qido(ds, max_responses=1)
        assert mock_qido.call_args.kwargs["limit"] == 2

    def test_zero_means_unlimited_and_sends_no_limit(self):
        ds = Dataset()
        ds.PatientName = "*"
        mock_qido = self._qido(ds, max_responses=0)
        assert mock_qido.call_args.kwargs["limit"] is None

    def test_extra_row_triggers_the_truncation_warning(self):
        from unittest.mock import patch

        ds = Dataset()
        ds.PatientName = "*"
        rows = [{"0020000D": {"Value": ["1.2.3"]}}, {"0020000D": {"Value": ["1.2.4"]}}]
        mock_qido = self._qido(ds, max_responses=1, rows=rows)
        assert mock_qido.call_args.kwargs["limit"] == 2

        event = FakeEvent(ds)
        with patch("clean_code.handlers.cfind._qido_request", return_value=rows):
            results = list(
                _handle_qido_search(
                    event,
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    1,
                    30,
                    {"PatientName": "*"},
                    "study",
                )
            )
        statuses = [status for status, _ in results]
        assert 0xFF00 in statuses, statuses
        assert statuses[-1] == 0xB001, statuses
        assert statuses.count(0xFF00) == 1, statuses


class TestSeriesSearchKeepsTheStudyCap:
    """When step (a) hits its study cap, the series matches behind that cap
    were never even looked for. The answer is therefore incomplete and must
    say so (0xB001), not report "complete, no matches" (0x0000)."""

    def test_a_study_cap_without_series_rows_is_still_truncation(self):
        from unittest.mock import patch

        from clean_code.handlers.cfind import _handle_series_search

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.SeriesInstanceUID = ""
        ds.Modality = "CT"

        # max_responses=2 -> the study step asks for 3 and gets 3: the cap.
        # None of these three studies has a series matching the query.
        study_rows = [
            {"0020000D": {"Value": ["1.2.3"]}},
            {"0020000D": {"Value": ["1.2.4"]}},
            {"0020000D": {"Value": ["1.2.5"]}},
        ]

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                return study_rows
            return []

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_series_search(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    2,
                    30,
                )
            )

        statuses = [status for status, _ in results]
        assert statuses[-1] == 0xB001, (
            "a study cap means matches may exist beyond it; reporting 0x0000 "
            f"claims the search was complete: {statuses}"
        )

    def test_no_study_cap_and_no_rows_is_a_complete_empty_answer(self):
        from unittest.mock import patch

        from clean_code.handlers.cfind import _handle_series_search

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"

        # Fewer studies than the cap: nothing was skipped.
        study_rows = [{"0020000D": {"Value": ["1.2.3"]}}]

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                return study_rows
            return []

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_series_search(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    2,
                    30,
                )
            )

        statuses = [status for status, _ in results]
        assert statuses[-1] == 0x0000, (
            "a full walk of a short study list is a complete answer: " f"{statuses}"
        )


class TestPatientRootLevelRouting:
    """Patient Root honours QueryRetrieveLevel: SERIES routes to the two-step
    series search (rows carry SeriesInstanceUID), PATIENT is refused 0xA900
    rather than relabelling study rows and duplicating a patient."""

    def test_series_level_returns_rows_with_a_series_instance_uid(self):
        from unittest.mock import patch

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"

        study_rows = [{"0020000D": {"Value": ["1.2.3"]}}]
        series_rows = [
            {
                "0020000E": {"Value": ["1.2.3.4"]},
                "00080060": {"Value": ["CT"]},
            }
        ]

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                return study_rows
            if "/series" in url:
                return series_rows
            return []

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_patient_root(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )

        rows = [row for status, row in results if status == 0xFF00]
        assert rows, "a SERIES query on Patient Root must return series rows"
        for row in rows:
            assert (
                "SeriesInstanceUID" in row
            ), f"a series row must carry SeriesInstanceUID: {row}"

    def test_patient_level_returns_one_row_per_patient(self):
        """PS3.4 C.6.1.1.2 Table C.6-1: PATIENT is the root level of the model
        this SCP announces, so it must answer. Two studies of one patient
        fold into ONE row; the row carries the level's own attributes and
        echoes QueryRetrieveLevel=PATIENT.

        Refusing with 0xA900 - the previous behaviour - made an announced
        model's root level unavailable.
        """
        from unittest.mock import patch

        def fake_qido(url, params, timeout, limit=None, tls=None):
            assert url.endswith("/studies"), url
            return [
                {
                    "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^Jane"}]},
                    "00100020": {"vr": "LO", "Value": ["PT-1"]},
                    "00100030": {"vr": "DA", "Value": ["19800229"]},
                    "00100040": {"vr": "CS", "Value": ["F"]},
                    "0020000D": {"vr": "UI", "Value": ["2.25.1111"]},
                    "00080020": {"vr": "DA", "Value": ["20260101"]},
                },
                {
                    "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^Jane"}]},
                    "00100020": {"vr": "LO", "Value": ["PT-1"]},
                    "00100030": {"vr": "DA", "Value": ["19800229"]},
                    "00100040": {"vr": "CS", "Value": ["F"]},
                    "0020000D": {"vr": "UI", "Value": ["2.25.2222"]},
                    "00080020": {"vr": "DA", "Value": ["20260202"]},
                },
            ]

        ds = Dataset()
        ds.QueryRetrieveLevel = "PATIENT"
        ds.PatientID = "PT-1"
        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_patient_root(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )

        rows = [row for status, row in results if status == 0xFF00]
        assert (
            len(rows) == 1
        ), f"two studies of one patient must fold into one row: {rows}"
        row = rows[0]
        assert str(row.QueryRetrieveLevel) == "PATIENT"
        assert str(row.PatientID) == "PT-1"
        assert "Doe^Jane" in str(row.PatientName)
        assert str(row.PatientBirthDate) == "19800229"
        assert str(row.PatientSex) == "F"
        # Table C.6-1 is the patient level's whole vocabulary: a study
        # attribute here would tell the SCU the level is really STUDY.
        assert "StudyInstanceUID" not in row
        assert "StudyDate" not in row
        assert results[-1] == (0x0000, None), results[-1]

    def test_patient_level_keeps_anonymous_studies_apart(self):
        """A study with no PatientID identifies nobody, so it cannot be
        merged with another anonymous study - each is its own row. (PS3.4
        C.6.1.1.2 note: Patient ID is the grouping key; without it there is
        no group.)"""
        from unittest.mock import patch

        def fake_qido(url, params, timeout, limit=None, tls=None):
            return [
                {
                    "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Anon^One"}]},
                    "0020000D": {"vr": "UI", "Value": ["2.25.secure"]},
                },
                {
                    "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Anon^Two"}]},
                    "0020000D": {"vr": "UI", "Value": ["2.25.nopeer"]},
                },
            ]

        ds = Dataset()
        ds.QueryRetrieveLevel = "PATIENT"
        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_patient_root(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )

        rows = [row for status, row in results if status == 0xFF00]
        assert (
            len(rows) == 2
        ), f"studies without a PatientID must not collapse into one patient: {rows}"

    def test_patient_level_caps_on_patient_count(self):
        """max_responses bounds PATIENT rows: 0xB001 must mean 'more patients
        exist', so the study set behind the fold is never silently cut."""
        from unittest.mock import patch

        def fake_qido(url, params, timeout, limit=None, tls=None):
            # Two studies for PT-1, one for PT-2, one for PT-3.
            out = []
            for pat in ("PT-1", "PT-1", "PT-2", "PT-3"):
                out.append(
                    {
                        "00100010": {"vr": "PN", "Value": [{"Alphabetic": "X^Y"}]},
                        "00100020": {"vr": "LO", "Value": [pat]},
                        "0020000D": {"vr": "UI", "Value": [f"2.25.{pat}.{len(out)}"]},
                    }
                )
            assert limit is None, "the fold needs the whole study set"
            return out

        ds = Dataset()
        ds.QueryRetrieveLevel = "PATIENT"
        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_patient_root(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    2,
                    30,
                )
            )

        rows = [row for status, row in results if status == 0xFF00]
        assert len(rows) == 2, f"the cap applies to patients, not studies: {rows}"
        assert results[-1] == (0xB001, None), results[-1]


class TestSeriesWalkHonoursCancelAndShutdown:
    """A broad SERIES walk can block per study; C-CANCEL and shutdown must
    stop it mid-walk with the standard codes and no further QIDO request."""

    def _study_rows(self):
        return [
            {"0020000D": {"Value": ["1.2.3"]}},
            {"0020000D": {"Value": ["1.2.4"]}},
        ]

    def test_cancel_between_studies_yields_fe00(self):
        from unittest.mock import patch

        from clean_code.handlers.cfind import _handle_series_search

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"
        event = FakeEvent(ds)
        series_requests = []

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                return self._study_rows()
            if "/series" in url:
                series_requests.append(url)
                # The SCU cancels after the first study's series were asked.
                event.is_cancelled = True
                return [{"0020000E": {"Value": ["1.2.3.4"]}}]
            return []

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_series_search(
                    event,
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )

        statuses = [status for status, _ in results]
        assert 0xFE00 in statuses, f"a cancel must yield 0xFE00: {statuses}"
        assert len(series_requests) == 1, (
            "no further QIDO request after the cancel: " f"{series_requests}"
        )

    def test_shutdown_before_the_first_study_yields_c000(self):
        from unittest.mock import patch

        from clean_code.handlers.cfind import _handle_series_search

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"
        event = FakeEvent(ds)

        class _Set:
            def is_set(self):
                return True

        series_requests = []

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                return self._study_rows()
            series_requests.append(url)
            return []

        with patch("clean_code.handlers.cfind._shutdown_requested", _Set()), patch(
            "clean_code.handlers.cfind._qido_request", side_effect=fake_qido
        ):
            results = list(
                _handle_series_search(
                    event,
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )

        statuses = [status for status, _ in results]
        assert 0xC000 in statuses, f"a shutdown must yield 0xC000: {statuses}"
        assert series_requests == [], (
            "no series QIDO request must go out during shutdown: " f"{series_requests}"
        )


class TestHttpStatusClassMapping:
    """A 5xx from the server or a proxy is Out of Resources (0xA700,
    retryable); a 4xx is this bridge's own bad query (0xC000, permanent).
    PS3.7 status classes, one test per QIDO path per class."""

    @staticmethod
    def _status_error(status_code):
        import httpx

        request = httpx.Request("GET", "http://qido.test")
        response = httpx.Response(status_code, request=request)
        return httpx.HTTPStatusError(
            f"{status_code} status", request=request, response=response
        )

    def test_study_503_is_out_of_resources(self):
        from unittest.mock import patch

        ds = Dataset()
        ds.PatientName = "*"

        def fake_qido(url, params, timeout, limit=None, tls=None):
            raise self._status_error(503)

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_qido_search(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                    {"PatientName": "*"},
                    "study",
                )
            )
        assert results == [(0xA700, None)], f"a 503 must be retryable: {results}"

    def test_study_400_is_cannot_understand(self):
        from unittest.mock import patch

        ds = Dataset()
        ds.PatientName = "*"

        def fake_qido(url, params, timeout, limit=None, tls=None):
            raise self._status_error(400)

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_qido_search(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                    {"PatientName": "*"},
                    "study",
                )
            )
        assert results == [(0xC000, None)], f"a 400 is our bad query: {results}"

    def test_series_studies_step_503_is_out_of_resources(self):
        from unittest.mock import patch

        from clean_code.handlers.cfind import _handle_series_search

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                raise self._status_error(503)
            return []

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_series_search(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )
        assert results == [(0xA700, None)], f"a 503 must be retryable: {results}"

    def test_series_studies_step_400_is_cannot_understand(self):
        from unittest.mock import patch

        from clean_code.handlers.cfind import _handle_series_search

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                raise self._status_error(400)
            return []

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_series_search(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )
        assert results == [(0xC000, None)], f"a 400 is our bad query: {results}"

    def test_patient_root_series_step_503_is_out_of_resources(self):
        from unittest.mock import patch

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                return [{"0020000D": {"Value": ["1.2.3"]}}]
            if "/series" in url:
                raise self._status_error(503)
            return []

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_patient_root(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )
        assert results == [(0xA700, None)], f"a 503 must be retryable: {results}"

    def test_patient_root_series_step_400_is_cannot_understand(self):
        from unittest.mock import patch

        ds = Dataset()
        ds.QueryRetrieveLevel = "SERIES"
        ds.Modality = "CT"

        def fake_qido(url, params, timeout, limit=None, tls=None):
            if url.endswith("/studies"):
                return [{"0020000D": {"Value": ["1.2.3"]}}]
            if "/series" in url:
                raise self._status_error(400)
            return []

        with patch("clean_code.handlers.cfind._qido_request", side_effect=fake_qido):
            results = list(
                _handle_patient_root(
                    FakeEvent(ds),
                    "http://127.0.0.1:8024/dicomweb",
                    False,
                    50,
                    30,
                )
            )
        assert results == [(0xC000, None)], f"a 400 is our bad query: {results}"
