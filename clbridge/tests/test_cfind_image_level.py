# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""C-FIND at COMPOSITE OBJECT INSTANCE (IMAGE) level.

PS3.4 C.6.1.1.5 Table C.6-4 and C.6.2.1.4 (standard/ps3.4/
image-level-C.6-4.json): the lowest level of both Query/Retrieve models is
named by QueryRetrieveLevel=IMAGE, its Unique key is SOP Instance UID
(0008,0018) and its Required key Instance Number (0020,0013). A response row
without SOPInstanceUID is not an answer at this level at all, which is why
the handler used to refuse the level instead of relabelling study rows.
"""

from unittest.mock import patch

from clean_code.handlers.cfind import (
    _handle_image_search,
    _handle_patient_root,
    _handle_study_root,
)
from clean_code.protocol import STUDY_ROOT_UID
from pydicom import Dataset

BASE = "http://127.0.0.1:8024/dicomweb"

STUDY_UID = "1.2.840.113619.2.55.3.604688.1"
SERIES_UID = "1.2.840.113619.2.55.3.604688.2"
INSTANCE_UID = "1.2.840.113619.2.55.3.604688.3"


class FakeAssoc:
    def __init__(self, bridge_ae="CLBRIDGE"):
        class _Acceptor:
            ae_title = bridge_ae

        self.acceptor = _Acceptor()


class FakeEvent:
    """Fake pynetdicom event carrying a real pydicom identifier Dataset."""

    def __init__(self, identifier, model_uid=STUDY_ROOT_UID):
        class _Context:
            abstract_syntax = model_uid

        self.identifier = identifier
        self.context = _Context()
        self.assoc = FakeAssoc()
        self.is_cancelled = False


def _image_query(level="IMAGE", with_series=True):
    ds = Dataset()
    ds.QueryRetrieveLevel = level
    ds.StudyInstanceUID = STUDY_UID
    if with_series:
        ds.SeriesInstanceUID = SERIES_UID
    return ds


STUDY_ROWS = [
    {
        "0020000D": {"Value": [STUDY_UID]},
        "00100010": {"Value": [{"Alphabetic": "DOE^JOHN"}]},
        "00100020": {"Value": ["PID-1"]},
        "00080060": {"Value": ["CT"]},
    }
]

SERIES_ROWS = [
    {
        "0020000E": {"Value": [SERIES_UID]},
        "00080060": {"Value": ["CT"]},
        "00200011": {"Value": [1]},
        "0008103E": {"Value": ["AXIAL"]},
    }
]

INSTANCE_ROWS = [
    {
        "00080016": {"Value": ["1.2.840.10008.5.1.4.1.1.2"]},
        "00080018": {"Value": [INSTANCE_UID]},
        "0020000D": {"Value": [STUDY_UID]},
        "0020000E": {"Value": [SERIES_UID]},
        "00200013": {"Value": [7]},
    }
]


def _walk(handler=None, ds=None, rows=None, max_responses=50):
    """Run a walk with the three QIDO hops stubbed by URL level."""
    study_rows = rows.get("studies", STUDY_ROWS) if rows else STUDY_ROWS
    series_rows = rows.get("series", SERIES_ROWS) if rows else SERIES_ROWS
    instance_rows = rows.get("instances", INSTANCE_ROWS) if rows else INSTANCE_ROWS
    calls = []

    def fake_qido(url, params, timeout, limit=None, tls=None, offset=0):
        calls.append((url, dict(params or {})))
        if url.endswith("/instances"):
            return instance_rows
        if url.endswith("/series"):
            return series_rows
        if url.endswith("/studies"):
            return study_rows
        return []

    event = FakeEvent(ds or _image_query())
    with patch(
        "clean_code.handlers.cfind._qido_request", side_effect=fake_qido
    ) as mock_qido:
        target = handler or _handle_image_search
        if handler in (_handle_patient_root, _handle_study_root):
            results = list(target(event, BASE, False, max_responses, 30))
        else:
            results = list(target(event, BASE, False, max_responses, 30))
    return results, calls, mock_qido


class TestImageLevelAnswers:
    """The level must answer with usable rows, not with a refusal."""

    def test_study_root_image_level_returns_instance_rows(self):
        results, calls, _m = _walk(handler=_handle_study_root)

        statuses = [status for status, _ in results]
        assert 0xA900 not in statuses, (
            "IMAGE is the Composite Object Instance level of the Study Root "
            f"model (PS3.4 C.6.2.1.4): 0xA900 is a protocol error here: {statuses}"
        )
        rows = [row for status, row in results if status == 0xFF00]
        assert rows, f"an IMAGE-level query must return rows: {statuses}"
        assert statuses[-1] == 0x0000, statuses

        row = rows[0]
        assert str(row.QueryRetrieveLevel) == "IMAGE"
        assert str(row.SOPInstanceUID) == INSTANCE_UID
        assert str(row.StudyInstanceUID) == STUDY_UID
        assert str(row.SeriesInstanceUID) == SERIES_UID
        assert int(row.InstanceNumber) == 7
        assert str(row.SOPClassUID) == "1.2.840.10008.5.1.4.1.1.2"
        # Table C.6-4 leaves the patient/series context optional; the walk
        # knows it, so the row carries it instead of dropping it.
        assert str(row.PatientName) == "DOE^JOHN"
        assert str(row.PatientID) == "PID-1"
        assert str(row.Modality) == "CT"
        assert str(row.SeriesNumber) == "1"
        assert str(row.SeriesDescription) == "AXIAL"
        # C.4.1.1.3.2: at the lowest level the retrieve location is not
        # allowed to be null, so the AE stamp is what makes the row usable.
        assert str(row.RetrieveAETitle) == "CLBRIDGE"

    def test_patient_root_image_level_returns_instance_rows(self):
        results, _calls, _m = _walk(handler=_handle_patient_root)

        statuses = [status for status, _ in results]
        assert 0xA900 not in statuses, statuses
        rows = [row for status, row in results if status == 0xFF00]
        assert rows, statuses
        assert str(rows[0].SOPInstanceUID) == INSTANCE_UID
        assert str(rows[0].QueryRetrieveLevel) == "IMAGE"

    def test_unknown_level_still_answers_a900(self):
        """0xA900 stays reserved for a level the model does not define."""
        ds = _image_query(level="PATIENT")
        results, _calls, _mock = _walk(handler=_handle_study_root, ds=ds)
        assert [status for status, _ in results] == [0xA900]


class TestImageLevelWalk:
    """The walk itself: hierarchy order, per-route keys, truncation."""

    def test_walk_is_study_then_series_then_instances(self):
        _results, calls, _m = _walk()
        urls = [url for url, _params in calls]
        assert urls[0] == f"{BASE}/studies"
        assert urls[1] == f"{BASE}/studies/{STUDY_UID}/series"
        assert urls[2] == (f"{BASE}/studies/{STUDY_UID}/series/{SERIES_UID}/instances")

    def test_instance_only_keys_stay_off_the_series_hop(self):
        """The series route answers 400 for SOP keys; they belong to the
        instance hop only (PS3.18 10.6 key scoping, qido_pure validation)."""
        ds = _image_query()
        ds.SOPInstanceUID = INSTANCE_UID
        ds.InstanceNumber = "7"
        _results, calls, _m = _walk(ds=ds)

        by_url = {url: params for url, params in calls}
        series_params = by_url[f"{BASE}/studies/{STUDY_UID}/series"]
        assert "SOPInstanceUID" not in series_params, series_params
        assert "InstanceNumber" not in series_params, series_params
        # SeriesInstanceUID narrows the series hop (it is foreign on the
        # instance route, which is already scoped by the path).
        assert series_params["SeriesInstanceUID"] == SERIES_UID

        instance_params = by_url[
            f"{BASE}/studies/{STUDY_UID}/series/{SERIES_UID}/instances"
        ]
        assert instance_params["SOPInstanceUID"] == INSTANCE_UID
        assert instance_params["InstanceNumber"] == "7"

    def test_row_without_sop_instance_uid_is_not_yielded(self):
        """The old defect: a row la(belled IMAGE with no SOPInstanceUID is
        unusable, so it must never reach the SCU."""
        broken = [{"0020000D": {"Value": [STUDY_UID]}, "00200013": {"Value": [1]}}]
        results, _calls, _m = _walk(rows={"instances": broken})

        rows = [row for status, row in results if status == 0xFF00]
        assert rows == [], f"a row without SOPInstanceUID must be dropped: {rows}"
        # Nothing matched, but the walk completed: Success, not an error.
        assert [status for status, _ in results] == [0x0000]

    def test_response_cap_reports_truncation(self):
        two_instances = [
            INSTANCE_ROWS[0],
            {
                "00080018": {"Value": [INSTANCE_UID + ".1"]},
                "00200013": {"Value": [8]},
            },
        ]
        results, _calls, _m = _walk(rows={"instances": two_instances}, max_responses=1)

        statuses = [status for status, _ in results]
        assert statuses.count(0xFF00) == 1, statuses
        # P9: 0xB001 is not defined for Query/Retrieve (PS3.4 Table C.4.1-1),
        # so an answer cut off by the cap is the defined refusal 0xA700.
        assert statuses[-1] == 0xA700, (
            "one instance was cut off by the cap, so the answer is incomplete "
            f"and must be a defined refusal: {statuses}"
        )

    def test_empty_study_set_is_a_complete_empty_answer(self):
        results, _calls, _m = _walk(rows={"studies": []})
        assert [status for status, _ in results] == [0x0000]
