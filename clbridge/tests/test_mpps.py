# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""MPPS translator: DIMSE N-CREATE/N-SET -> DICOMweb MPPS (PS3.18 section 15).

The tests pin the two transactions the standard distinguishes ONLY by the
`update` query parameter (PS3.18 15.5), the HTTP-to-DIMSE status mapping
(PS3.18 Table 15.4.3-1, PS3.7 service "mpps") and the refusals PS3.4 F.7.2.1
fixes. Every test drives `handle_n_create` / `handle_n_set` with a fake
pynetdicom event, so a status - never an exception - is what the assertion sees.
"""

import json
from unittest.mock import MagicMock, patch

import httpx
import pytest
from clean_code.handlers import mpps
from clean_code.handlers.mpps import (
    MPPS_SOP_CLASS_UID,
    handle_n_create,
    handle_n_set,
)
from pydicom.dataset import Dataset
from pydicom.sequence import Sequence

URL = "http://127.0.0.1:8024/dicomweb"
MPPS_UID = "1.2.826.0.1.3680043.10.5555.1"


# ---------------------------------------------------------------------------
# Test doubles
# ---------------------------------------------------------------------------
class _Client:
    """Minimal stand-in for `httpx.Client` that records every POST.

    A real transport is not needed: the translator only reads `status_code`,
    and the request the bridge builds (URL, headers, body) is exactly what has
    to be asserted.
    """

    calls = []
    status_code = 201
    raises = None

    def __init__(self, *args, **kwargs):
        self.kwargs = kwargs

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def post(self, url, content=None, headers=None):
        type(self).calls.append({"url": url, "content": content, "headers": headers})
        if type(self).raises is not None:
            raise type(self).raises
        return httpx.Response(
            type(self).status_code, request=httpx.Request("POST", url)
        )

    def close(self):
        pass


@pytest.fixture
def http():
    """Patch the httpx client the translator builds; reset the recorder."""
    _Client.calls = []
    _Client.status_code = 201
    _Client.raises = None
    with patch("clean_code.handlers.mpps.httpx.Client", _Client):
        yield _Client


def _event(uid=MPPS_UID, ds=None, ae="MPPS_SCU"):
    requestor = MagicMock()
    requestor.ae_title = ae
    assoc = MagicMock()
    assoc.requestor = requestor
    event = MagicMock()
    event.assoc = assoc
    event.request.AffectedSOPInstanceUID = uid
    event.request.RequestedSOPInstanceUID = uid
    event.attribute_list = ds if ds is not None else _create_dataset()
    event.modification_list = ds if ds is not None else _update_dataset()
    return event


def _create_dataset():
    """A minimal conformant N-CREATE body (PS3.4 Table F.7.2-1)."""
    ds = Dataset()
    ds.PerformedProcedureStepStatus = "IN PROGRESS"
    ds.Modality = "CT"
    ds.PerformedStationAETitle = "CT01"
    ds.PerformedProcedureStepID = "PS-1"
    ds.PerformedProcedureStepStartDate = "20261119"
    ds.PerformedProcedureStepStartTime = "101500"
    ds.PatientName = "Test^Patient"
    ds.PatientID = "P-1"
    ds.AccessionNumber = "ACC-1"
    step = Dataset()
    step.StudyInstanceUID = "1.2.3.4"
    step.ScheduledProcedureStepID = "SPS-1"
    ds.ScheduledStepAttributesSequence = Sequence([step])
    return ds


def _update_dataset(status="COMPLETED"):
    ds = Dataset()
    ds.PerformedProcedureStepStatus = status
    ds.PerformedProcedureStepEndDate = "20261119"
    ds.PerformedProcedureStepEndTime = "102000"
    series = Dataset()
    series.SeriesInstanceUID = "1.2.3.4.5"
    ref = Dataset()
    ref.ReferencedSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
    ref.ReferencedSOPInstanceUID = "1.2.3.4.5.6"
    series.ReferencedImageSequence = Sequence([ref])
    ds.PerformedSeriesSequence = Sequence([series])
    return ds


# ---------------------------------------------------------------------------
# Create: transaction shape
# ---------------------------------------------------------------------------
def test_create_posts_the_mpss_resource_with_dicom_json(http, mock_shutdown_not_set):
    status, rsp = handle_n_create(_event(), URL)

    assert status == 0x0000
    assert rsp is None
    assert len(http.calls) == 1
    call = http.calls[0]
    # PS3.18 15.4: Create is POST on the resource, with NO query parameter -
    # the `update` parameter is what turns the same URI into Update (15.5).
    assert call["url"] == f"{URL}/modality-performed-procedure-steps/{MPPS_UID}"
    assert call["headers"]["Content-Type"] == "application/dicom+json"
    assert call["headers"]["Accept"] == "application/dicom+json"


def test_create_body_carries_the_forwarded_attributes(http, mock_shutdown_not_set):
    handle_n_create(_event(), URL)

    body = json.loads(http.calls[0]["content"])
    # (0040,0252) is the state machine's key (PS3.4 F.7.2.1).
    assert body["00400252"] == {"vr": "CS", "Value": ["IN PROGRESS"]}
    assert body["00080060"] == {"vr": "CS", "Value": ["CT"]}
    assert body["00400241"] == {"vr": "AE", "Value": ["CT01"]}
    assert body["00100010"] == {"vr": "PN", "Value": [{"Alphabetic": "Test^Patient"}]}
    # The study lives INSIDE (0040,0270), never at the top level (PS3.4 F.7).
    assert "0020000D" not in body
    step = body["00400270"]["Value"][0]
    assert step["0020000D"] == {"vr": "UI", "Value": ["1.2.3.4"]}
    assert step["00400009"] == {"vr": "SH", "Value": ["SPS-1"]}


def test_create_writes_the_sop_common_pair_from_the_command_field(
    http, mock_shutdown_not_set
):
    """A data set that names a DIFFERENT instance UID cannot redirect the POST.

    DIMSE carries the UID as the Affected SOP Instance UID command field
    (PS3.7 10.1.5.1), so the data set's own (0008,0018) is an attribute the
    client is not authorised to choose.
    """
    ds = _create_dataset()
    ds.SOPInstanceUID = "9.9.9.9"
    ds.SOPClassUID = "9.9.9.8"
    handle_n_create(_event(ds=ds), URL)

    body = json.loads(http.calls[0]["content"])
    assert body["00080018"] == {"vr": "UI", "Value": [MPPS_UID]}
    assert body["00080016"] == {"vr": "UI", "Value": [MPPS_SOP_CLASS_UID]}


def test_create_url_escapes_the_uid_as_one_path_segment(http, mock_shutdown_not_set):
    """A UID carrying `/` or `?` must not address another resource."""
    handle_n_create(_event(uid="1.2?x=y"), URL)

    assert http.calls[0]["url"] == (
        f"{URL}/modality-performed-procedure-steps/1.2%3Fx%3Dy"
    )


# ---------------------------------------------------------------------------
# Update: the `?update` parameter is the transaction selector
# ---------------------------------------------------------------------------
def test_update_posts_with_the_mandatory_update_parameter(http, mock_shutdown_not_set):
    _Client.status_code = 200
    status, rsp = handle_n_set(_event(), URL)

    assert status == 0x0000
    assert http.calls[0]["url"] == (
        f"{URL}/modality-performed-procedure-steps/{MPPS_UID}?update"
    )
    assert http.calls[0]["headers"]["Content-Type"] == "application/dicom+json"


def test_update_body_keeps_the_reference_pair_inside_referenced_image_sequence(
    http, mock_shutdown_not_set
):
    _Client.status_code = 200
    handle_n_set(_event(), URL)

    body = json.loads(http.calls[0]["content"])
    series = body["00400340"]["Value"][0]
    assert series["0020000E"] == {"vr": "UI", "Value": ["1.2.3.4.5"]}
    assert series["00081140"]["Value"][0] == {
        "00081150": {"vr": "UI", "Value": ["1.2.840.10008.5.1.4.1.1.2"]},
        "00081155": {"vr": "UI", "Value": ["1.2.3.4.5.6"]},
    }


# ---------------------------------------------------------------------------
# HTTP -> DIMSE status
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    "http_status, expected",
    [
        (201, 0x0000),
        (200, 0x0000),
        # PS3.18 Table 15.4.3-1: 409 "the provided MPPS UID is already in use";
        # PS3.7 names the same fact 0x0111 Duplication SOP Instance.
        (409, 0x0111),
        (400, 0x0110),
        (503, 0x0213),
    ],
)
def test_create_maps_the_origin_answer(
    http, mock_shutdown_not_set, http_status, expected
):
    _Client.status_code = http_status
    status, rsp = handle_n_create(_event(), URL)

    assert status == expected
    assert rsp is None


def test_update_404_is_no_such_instance(http, mock_shutdown_not_set):
    """N-SET on an unknown MPPS is 0x0112 (PS3.7), not a generic failure."""
    _Client.status_code = 404
    status, _ = handle_n_set(_event(), URL)
    assert status == 0x0112


@pytest.mark.parametrize("exc", [httpx.ConnectError("refused"), httpx.ReadTimeout("t")])
def test_transport_failure_is_a_non_zero_status(http, mock_shutdown_not_set, exc):
    """Never answer 0x0000 when the upstream call did not land."""
    _Client.raises = exc
    status, rsp = handle_n_create(_event(), URL)

    assert status == 0x0110
    assert rsp is None


# ---------------------------------------------------------------------------
# Parse refusals (PS3.4 F.7.2.1 / PS3.7)
# ---------------------------------------------------------------------------
def test_create_status_other_than_in_progress_is_refused_0106(
    http, mock_shutdown_not_set
):
    """PS3.4 F.7.2.1 fixes 0106H for a non-IN-PROGRESS N-CREATE."""
    ds = _create_dataset()
    ds.PerformedProcedureStepStatus = "COMPLETED"
    status, _ = handle_n_create(_event(ds=ds), URL)

    assert status == 0x0106
    assert http.calls == [], "a refused request must not reach the origin"


def test_create_without_status_is_missing_attribute(http, mock_shutdown_not_set):
    ds = _create_dataset()
    del ds.PerformedProcedureStepStatus
    status, _ = handle_n_create(_event(ds=ds), URL)

    assert status == 0x0120
    assert http.calls == []


def test_performed_series_item_without_series_uid_is_refused(
    http, mock_shutdown_not_set
):
    """(0020,000E) is Type 1 inside every (0040,0340) item (Table F.7.2-1)."""
    ds = _update_dataset()
    del ds.PerformedSeriesSequence[0].SeriesInstanceUID
    status, _ = handle_n_set(_event(ds=ds), URL)

    assert status == 0x0120
    assert http.calls == []


def test_empty_performed_series_item_is_accepted(http, mock_shutdown_not_set):
    """An empty (0040,0340) item encodes "no series performed yet" - the
    sequence is Type 2 at N-CREATE (Table F.7.2-1) and carries no Type 1
    Series Instance UID to enforce, so it must not be refused."""
    ds = _create_dataset()
    ds.PerformedSeriesSequence = Sequence([Dataset()])
    status, _ = handle_n_create(_event(ds=ds), URL)

    assert status == 0x0000
    assert len(http.calls) == 1


def test_reference_without_instance_uid_is_refused(http, mock_shutdown_not_set):
    ds = _update_dataset()
    del (
        ds.PerformedSeriesSequence[0]
        .ReferencedImageSequence[0]
        .ReferencedSOPInstanceUID
    )
    status, _ = handle_n_set(_event(ds=ds), URL)

    assert status == 0x0120
    assert http.calls == []


def test_update_with_an_invalid_status_is_refused_0106(http, mock_shutdown_not_set):
    ds = _update_dataset(status="ABORTED")
    status, _ = handle_n_set(_event(ds=ds), URL)

    assert status == 0x0106
    assert http.calls == []


def test_missing_instance_uid_is_a_refusal_not_a_crash(http, mock_shutdown_not_set):
    """The UID comes from the command field; without it there is no resource."""
    event = _event(uid="")
    status, rsp = handle_n_create(event, URL)

    assert status == 0x0120
    assert rsp is None


def test_undecodable_dataset_returns_processing_failure(http, mock_shutdown_not_set):
    """pydicom decodes lazily: walking the set is what raises, and it must not
    escape into the association reactor."""

    class Exploding(dict):
        def __iter__(self):
            raise ValueError("broken element encoding")

    event = _event()
    event.attribute_list = Exploding()
    status, _ = handle_n_create(event, URL)

    assert status == 0x0110
    assert http.calls == []


def test_unexpected_parse_surprise_never_escapes(http, mock_shutdown_not_set):
    event = _event()
    with patch.object(mpps, "_body", side_effect=RuntimeError("boom")):
        status, rsp = handle_n_create(event, URL)

    assert status == 0x0110
    assert rsp is None


def test_shutdown_refuses_both_operations(http, mock_shutdown_set):
    assert handle_n_create(_event(), URL)[0] == 0xC000
    assert handle_n_set(_event(), URL)[0] == 0xC000
    assert http.calls == []


# ---------------------------------------------------------------------------
# JSON encoding rules (PS3.18 F.2.5 / F.2.3)
# ---------------------------------------------------------------------------
def test_zero_length_attribute_has_no_value_member():
    """F.2.5: an empty attribute is preserved WITHOUT a Value member."""
    ds = Dataset()
    ds.PatientID = ""
    encoded = mpps._dataset_json(ds)

    assert encoded["00100020"] == {"vr": "LO"}


def test_integer_string_vr_is_a_json_number_where_the_table_allows_it():
    """Table F.2.3-1: IS is "Number or String"; US offers no string form."""
    ds = Dataset()
    ds.add_new((0x0040, 0x0000), "US", 0)
    ds.add_new((0x0020, 0x1208), "IS", 12)
    encoded = mpps._dataset_json(ds)

    assert encoded["00400000"] == {"vr": "US", "Value": [0]}
    assert encoded["00201208"] == {"vr": "IS", "Value": [12]}


def test_url_for_update_carries_the_parameter():
    assert mpps._mpps_url(URL, "1.2.3", update=True).endswith("?update")
    assert not mpps._mpps_url(URL, "1.2.3", update=False).endswith("?update")
