# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""MWL <-> UPS-RS mapping, in the shape PS3.4 Table K.6-1 requires.

The Modality Worklist Information Model is a flat, non-object model: no SOP
Common attributes, one row per Scheduled Procedure Step, and the SPS attributes
inside the single item of (0040,0100). These tests pin that shape:

- _build_mwl_params: matching keys are read from the top level AND from the
  (0040,0100) item, because a conformant request nests them;
- _extract_mwl_request_keywords: the same recursion for the QUERY keywords the
  response direction needs;
- _json_to_dataset_mwl: Return keys always, Optional keys only when requested,
  SPS ID / Requested Procedure ID derived, (0074,1200) never emitted;
- _handle_mwl end-to-end through the shared C-FIND path.
"""

from unittest.mock import patch

from clean_code.handlers.cfind import _handle_mwl
from clean_code.protocol import (
    MWL_OPTIONAL_SPS,
    MWL_OPTIONAL_TOP,
    MWL_RETURN_SPS,
    MWL_RETURN_TOP,
    _build_mwl_params,
    _extract_mwl_request_keywords,
    _json_to_dataset_mwl,
)
from pydicom import Dataset

WORKITEM_UID = "2.25.1234567890123456789012345678901234567"
STUDY_UID = "1.2.826.0.1.3680043.10.7777.1"


class TestBuildMwlParams:
    """Query direction: C-FIND MWL matching keys -> workitem query params."""

    def test_maps_flat_filterable_keys(self):
        """The legacy flat form still works (ScheduledStationAETitle et al).

        The VALUE of the station parameter is the standard (0040,4025) Code
        Value key, because PS3.4 Table CC.2.5-3 gives (0040,0001) no row and a
        UPS-RS origin refuses that key.
        """
        ds = Dataset()
        ds.ScheduledStationAETitle = "STATION01"
        ds.Modality = "CT"
        ds.PatientID = "PT-7"
        params = _build_mwl_params(ds)
        assert params == {
            "00404025.00080100": "STATION01",
            "00080060": "CT",
            "00100020": "PT-7",
        }

    def test_the_station_query_uses_the_standard_key(self):
        """A station C-FIND builds the (0040,4025) Code Value parameter.

        The station is a (0040,4025) Code Value on the UPS side (PS3.4 Table
        CC.2.5-3; AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3), so the query that
        names it must use that path - not the vendor (0040,0001).
        """
        ds = Dataset()
        sps = Dataset()
        sps.ScheduledStationAETitle = "INFER_1"
        ds.ScheduledProcedureStepSequence = [sps]
        params = _build_mwl_params(ds)
        assert params == {"00404025.00080100": "INFER_1"}
        assert "00400001" not in params, "the vendor key must not be sent"

    def test_maps_keys_nested_in_the_sps_sequence(self):
        """The STANDARD nested form is read too - it was a match-all silent bug.

        A conformant MWL request puts ScheduledStationAETitle inside
        (0040,0100). Before this was handled the params came back empty, so the
        server returned every workitem and the modality's filter did nothing.
        """
        ds = Dataset()
        sps = Dataset()
        sps.ScheduledStationAETitle = "STATION01"
        ds.ScheduledProcedureStepSequence = [sps]
        assert _build_mwl_params(ds) == {"00404025.00080100": "STATION01"}

    def test_top_level_wins_when_stated_both_ways(self):
        """A value at both levels resolves to the top-level one, deterministically."""
        ds = Dataset()
        ds.ScheduledStationAETitle = "TOP"
        sps = Dataset()
        sps.ScheduledStationAETitle = "NESTED"
        ds.ScheduledProcedureStepSequence = [sps]
        assert _build_mwl_params(ds) == {"00404025.00080100": "TOP"}

    def test_skips_non_filterable_key(self):
        """AccessionNumber is not a server workitem filter -> absent (= all)."""
        ds = Dataset()
        ds.ScheduledStationAETitle = "STATION01"
        ds.AccessionNumber = "ACC-1"
        assert _build_mwl_params(ds) == {"00404025.00080100": "STATION01"}

    def test_empty_values_are_skipped(self):
        """An empty value is the same statement as an absent key (match all)."""
        ds = Dataset()
        sps = Dataset()
        sps.ScheduledStationAETitle = ""
        ds.ScheduledProcedureStepSequence = [sps]
        assert _build_mwl_params(ds) == {}

    def test_empty_when_no_filterable_keys(self):
        """No matching keys -> empty dict (match all)."""
        ds = Dataset()
        ds.PatientName = "Test*"
        assert _build_mwl_params(ds) == {}


class TestExtractMwlRequestKeywords:
    """The QUERY keyword set must see inside (0040,0100)."""

    def test_sees_keywords_inside_the_sps_item(self):
        ds = Dataset()
        ds.AccessionNumber = ""
        ds.RequestedProcedureID = ""
        sps = Dataset()
        sps.ScheduledStationAETitle = "STATION01"
        sps.ScheduledProcedureStepID = ""
        ds.ScheduledProcedureStepSequence = [sps]

        keywords = _extract_mwl_request_keywords(ds)
        # Both levels are visible.
        assert "AccessionNumber" in keywords
        assert "RequestedProcedureID" in keywords
        assert "ScheduledProcedureStepID" in keywords
        assert "ScheduledStationAETitle" in keywords

    def test_empty_sequence_is_harmless(self):
        ds = Dataset()
        ds.AccessionNumber = ""
        ds.ScheduledProcedureStepSequence = []
        assert _extract_mwl_request_keywords(ds) == {"AccessionNumber"}


class TestJsonToDatasetMwl:
    """Response direction: workitem JSON -> MWL Dataset, nested and request-gated."""

    def _workitem_json(self):
        # Mirrors the server render (handlers/ups/json.rs write_workitem_json):
        # the study and the accession are carried by (0040,A370) Referenced
        # Request Sequence, NOT as bare top-level tags (Task 3, 09.10), and
        # the station by the (0040,4025) Code Value (Stage 3, 09.10).
        return {
            "00080018": {"vr": "UI", "Value": [WORKITEM_UID]},
            "00080060": {"vr": "CS", "Value": ["CT"]},
            "00100010": {"vr": "PN", "Value": [{"Alphabetic": "SMIRNOV^IVAN"}]},
            "00100020": {"vr": "LO", "Value": ["PT-1"]},
            "00321032": {"vr": "PN", "Value": [{"Alphabetic": "DR^SMITH"}]},
            "00404025": {
                "vr": "SQ",
                "Value": [
                    {
                        "00080100": {"vr": "SH", "Value": ["CT1"]},
                        "00080102": {"vr": "SH", "Value": ["99CLARUS"]},
                        "00080104": {"vr": "LO", "Value": ["AI station"]},
                    }
                ],
            },
            "00400002": {"vr": "DA", "Value": ["20260802"]},
            "00400003": {"vr": "TM", "Value": ["103000"]},
            "00400010": {"vr": "SH", "Value": ["Scanner A"]},
            "00404021": {
                "vr": "SQ",
                "Value": [{"0020000D": {"vr": "UI", "Value": [STUDY_UID]}}],
            },
            "0040A370": {
                "vr": "SQ",
                "Value": [
                    {
                        "00080050": {"vr": "SH", "Value": ["ACC-9"]},
                        "0020000D": {"vr": "UI", "Value": [STUDY_UID]},
                    }
                ],
            },
            "00741204": {"vr": "SH", "Value": ["CAD"]},
            # (0074,1200) is a UPS attribute; present in the render on purpose
            # to prove the bridge drops it.
            "00741200": {"vr": "CS", "Value": ["HIGH"]},
        }

    # -- shape (PS3.4 Table K.6-1) -----------------------------------------

    def test_return_keys_are_at_identifier_level(self):
        """PatientName/PatientID are identifier-level Return keys.

        T3: PS3.4 K.6 - a C-FIND response carries the requested keys at the
        level the identifier addresses. An empty requested-keyword set means
        the SCU asked for a STATION-shaped match, so patient identifiers are
        not a promised part of the answer; they are asserted when the request
        names them.
        """
        requested = {"PatientName", "PatientID"}
        ds = _json_to_dataset_mwl(self._workitem_json(), requested)
        assert str(ds.PatientName) == "SMIRNOV^IVAN"
        assert str(ds.PatientID) == "PT-1"

    def test_a_station_only_request_does_not_promise_patient_identifiers(self):
        """T3: no requested patient keys -> the answer is not required to carry
        them, and the bridge must not be asked to invent them."""
        ds = _json_to_dataset_mwl(self._workitem_json(), {"ScheduledStationAETitle"})
        # The station IS the requested key and must be there.
        assert "ScheduledProcedureStepSequence" in ds
        assert (
            str(ds.ScheduledProcedureStepSequence[0].ScheduledStationAETitle) == "CT1"
        )

    def test_sps_attributes_live_inside_the_sequence(self):
        """SPS attributes are NOT flat: they belong to the (0040,0100) item."""
        ds = _json_to_dataset_mwl(self._workitem_json(), set())
        assert "ScheduledProcedureStepSequence" in ds
        seq = ds.ScheduledProcedureStepSequence
        assert len(seq) == 1, "Table K.6-1: exactly one Item"
        item = seq[0]
        assert str(item.ScheduledStationAETitle) == "CT1"
        assert str(item.ScheduledProcedureStepStartDate) == "20260802"
        assert str(item.ScheduledProcedureStepStartTime) == "103000"
        assert str(item.Modality) == "CT"
        # The flat emission is gone.
        assert "ScheduledStationAETitle" not in ds

    def test_the_station_comes_from_the_sequence_code_value(self):
        """The SPS station is sourced from (0040,4025) Code Value.

        PS3.4 Table CC.2.5-3 carries the station in that sequence and gives
        (0040,0001) no row, so a workitem whose ONLY station carrier is
        (0040,4025) must still render the MWL element Table K.6-1 defines.
        """
        json_obj = self._workitem_json()
        assert "00400001" not in json_obj, "the UPS side states no vendor tag"

        ds = _json_to_dataset_mwl(json_obj, set())
        item = ds.ScheduledProcedureStepSequence[0]
        station = item[0x0040, 0x0001]
        assert station.value == "CT1"
        # The element is the DIMSE one Table K.6-1 defines (0040,0001) AE;
        # only its SOURCE on the UPS side moved into the sequence.
        assert station.keyword == "ScheduledStationAETitle"
        assert station.VR == "AE"

    def test_a_workitem_without_the_station_sequence_emits_no_station(self):
        """No (0040,4025) means no station - the Return key is then absent."""
        json_obj = self._workitem_json()
        del json_obj["00404025"]
        ds = _json_to_dataset_mwl(json_obj, set())
        item = ds.ScheduledProcedureStepSequence[0]
        assert "ScheduledStationAETitle" not in item

    def test_no_ups_priority_in_the_mwl_row(self):
        """(0074,1200) is PS3.3 C.30.1 (UPS), not in Table K.6-1 -> never emitted."""
        ds = _json_to_dataset_mwl(
            self._workitem_json(), {"ScheduledProcedureStepPriority"}
        )
        assert "ScheduledProcedureStepPriority" not in ds
        assert (
            "ScheduledProcedureStepPriority" not in ds.ScheduledProcedureStepSequence[0]
        )

    def test_no_sop_instance_uid_in_the_mwl_row(self):
        """MWL has no SOP Common attributes, so the workitem UID cannot leak in."""
        ds = _json_to_dataset_mwl(self._workitem_json(), set())
        assert "SOPInstanceUID" not in ds
        assert "SOPClassUID" not in ds

    # -- derived identifiers -------------------------------------------------

    def test_sps_id_is_the_workitem_uid_tail(self):
        """(0040,0009) is the last 16 characters of the workitem UID."""
        ds = _json_to_dataset_mwl(self._workitem_json(), set())
        item = ds.ScheduledProcedureStepSequence[0]
        assert str(item.ScheduledProcedureStepID) == WORKITEM_UID[-16:]
        assert len(str(item.ScheduledProcedureStepID)) == 16

    def test_rp_id_is_the_study_uid_tail_when_requested(self):
        """(0040,1001) is Optional: emitted only when asked, from the study UID."""
        ds = _json_to_dataset_mwl(self._workitem_json(), {"RequestedProcedureID"})
        assert str(ds.RequestedProcedureID) == STUDY_UID[-16:]

    def test_rp_id_absent_without_a_study(self):
        """No study in either carriage -> no Requested Procedure -> nothing."""
        json_obj = self._workitem_json()
        del json_obj["0040A370"]
        del json_obj["00404021"]
        ds = _json_to_dataset_mwl(json_obj, {"RequestedProcedureID"})
        assert "RequestedProcedureID" not in ds

    def test_rp_id_not_emitted_when_not_requested(self):
        """Optional means request-gated: silence when the modality did not ask."""
        ds = _json_to_dataset_mwl(self._workitem_json(), set())
        assert "RequestedProcedureID" not in ds

    # -- the sequence carriage (Task 3, 09.10) ------------------------------

    def test_study_and_accession_are_read_from_the_referenced_request_sequence(self):
        """The study and the accession come from (0040,A370), not a bare tag.

        The server render carries both inside Referenced Request Sequence
        (PS3.4 Table CC.2.5-3). A bridge that read only the bare top-level tags
        would lose them the moment the render switched to the sequence, so the
        MWL row must be built from the sequence.
        """
        json_obj = self._workitem_json()
        assert "0020000D" not in json_obj, "fixture has no bare study tag"
        assert "00080050" not in json_obj, "fixture has no bare accession tag"

        ds = _json_to_dataset_mwl(
            json_obj, {"StudyInstanceUID", "AccessionNumber", "RequestedProcedureID"}
        )
        assert str(ds.StudyInstanceUID) == STUDY_UID
        assert str(ds.AccessionNumber) == "ACC-9"
        assert str(ds.RequestedProcedureID) == STUDY_UID[-16:]

    def test_bare_tags_still_work_as_a_fallback(self):
        """An older render (bare 00080050/0020000D) is still read."""
        json_obj = self._workitem_json()
        del json_obj["0040A370"]
        json_obj["0020000D"] = {"vr": "UI", "Value": [STUDY_UID]}
        json_obj["00080050"] = {"vr": "SH", "Value": ["ACC-BARE"]}
        ds = _json_to_dataset_mwl(
            json_obj, {"StudyInstanceUID", "AccessionNumber", "RequestedProcedureID"}
        )
        assert str(ds.StudyInstanceUID) == STUDY_UID
        assert str(ds.AccessionNumber) == "ACC-BARE"

    def test_input_information_sequence_is_the_last_resort_for_the_study(self):
        """With no Referenced Request Sequence and no bare tag, (0040,4021)."""
        json_obj = self._workitem_json()
        del json_obj["0040A370"]
        ds = _json_to_dataset_mwl(
            json_obj, {"StudyInstanceUID", "RequestedProcedureID"}
        )
        assert str(ds.StudyInstanceUID) == STUDY_UID
        assert str(ds.RequestedProcedureID) == STUDY_UID[-16:]

    # -- request-gated Optional keys ----------------------------------------

    def test_optional_keys_only_when_requested(self):
        """AccessionNumber/StudyInstanceUID/StationName follow the request."""
        bare = _json_to_dataset_mwl(self._workitem_json(), set())
        assert "AccessionNumber" not in bare
        assert "StudyInstanceUID" not in bare
        assert "ScheduledStationName" not in bare.ScheduledProcedureStepSequence[0]

        asked = _json_to_dataset_mwl(
            self._workitem_json(),
            {"AccessionNumber", "StudyInstanceUID", "ScheduledStationName"},
        )
        assert str(asked.AccessionNumber) == "ACC-9"
        assert str(asked.StudyInstanceUID) == STUDY_UID
        assert (
            str(asked.ScheduledProcedureStepSequence[0].ScheduledStationName)
            == "Scanner A"
        )

    def test_sps_description_comes_from_the_step_label(self):
        """(0040,0007) SPS Description <- (0074,1204) Procedure Step Label."""
        ds = _json_to_dataset_mwl(
            self._workitem_json(), {"ScheduledProcedureStepDescription"}
        )
        item = ds.ScheduledProcedureStepSequence[0]
        assert str(item.ScheduledProcedureStepDescription) == "CAD"

    def test_requesting_physician_only_when_requested(self):
        assert "RequestingPhysician" not in _json_to_dataset_mwl(
            self._workitem_json(), set()
        )
        ds = _json_to_dataset_mwl(self._workitem_json(), {"RequestingPhysician"})
        assert str(ds.RequestingPhysician) == "DR^SMITH"

    # -- (0040,4005) DT fallback --------------------------------------------

    def test_start_datetime_dt_is_split_into_da_and_tm(self):
        """A UPS-only DT becomes the MWL DA/TM pair (both Return keys Type 1)."""
        json_obj = self._workitem_json()
        del json_obj["00400002"]
        del json_obj["00400003"]
        json_obj["00404005"] = {"vr": "DT", "Value": ["20260802103000"]}
        item = _json_to_dataset_mwl(json_obj, set()).ScheduledProcedureStepSequence[0]
        assert str(item.ScheduledProcedureStepStartDate) == "20260802"
        assert str(item.ScheduledProcedureStepStartTime) == "103000"

    def test_explicit_da_tm_wins_over_the_dt_fallback(self):
        json_obj = self._workitem_json()
        json_obj["00404005"] = {"vr": "DT", "Value": ["20270101000000"]}
        item = _json_to_dataset_mwl(json_obj, set()).ScheduledProcedureStepSequence[0]
        assert str(item.ScheduledProcedureStepStartDate) == "20260802"
        assert str(item.ScheduledProcedureStepStartTime) == "103000"

    def test_no_start_source_means_no_start_keys(self):
        """Honest skip: a step with neither DA/TM nor DT emits neither."""
        json_obj = self._workitem_json()
        del json_obj["00400002"]
        del json_obj["00400003"]
        item = _json_to_dataset_mwl(json_obj, set()).ScheduledProcedureStepSequence[0]
        assert "ScheduledProcedureStepStartDate" not in item
        assert "ScheduledProcedureStepStartTime" not in item

    # -- charset ------------------------------------------------------------

    def test_charset_is_first(self):
        """PS3.5 6.1.2: SpecificCharacterSet must be the first element."""
        ds = _json_to_dataset_mwl(self._workitem_json(), set())
        assert str(ds.SpecificCharacterSet) == "ISO_IR 192"
        assert list(ds)[0].tag == 0x00080005

    # -- map hygiene --------------------------------------------------------

    def test_map_partition_has_no_overlap(self):
        """No key may be both a Return key and an Optional one."""
        for left, right in (
            (MWL_RETURN_TOP, MWL_OPTIONAL_TOP),
            (MWL_RETURN_TOP, MWL_OPTIONAL_SPS),
            (MWL_RETURN_TOP, MWL_RETURN_SPS),
            (MWL_RETURN_SPS, MWL_OPTIONAL_SPS),
            (MWL_RETURN_SPS, MWL_OPTIONAL_TOP),
            (MWL_OPTIONAL_TOP, MWL_OPTIONAL_SPS),
        ):
            assert not (set(left) & set(right))

    def test_ups_priority_is_absent_from_every_map(self):
        for mapping in (
            MWL_RETURN_TOP,
            MWL_RETURN_SPS,
            MWL_OPTIONAL_TOP,
            MWL_OPTIONAL_SPS,
        ):
            assert "00741200" not in mapping


class TestHandleMwlMapping:
    """_handle_mwl end-to-end through the shared qido path with the MWL converter."""

    def test_mwl_response_is_nested_and_filtered(self, mock_shutdown_not_set):
        """A standard nested request filters, and the row comes back nested."""
        workitem = {
            "00080018": {"vr": "UI", "Value": [WORKITEM_UID]},
            "0040A370": {
                "vr": "SQ",
                "Value": [
                    {
                        "00080050": {"vr": "SH", "Value": ["ACC-9"]},
                        "0020000D": {"vr": "UI", "Value": [STUDY_UID]},
                    }
                ],
            },
            "00080060": {"vr": "CS", "Value": ["CT"]},
            "00404025": {
                "vr": "SQ",
                "Value": [
                    {
                        "00080100": {"vr": "SH", "Value": ["STATION01"]},
                        "00080102": {"vr": "SH", "Value": ["99CLARUS"]},
                        "00080104": {"vr": "LO", "Value": ["AI station"]},
                    }
                ],
            },
            "00400010": {"vr": "SH", "Value": ["Scanner B"]},
            "00100020": {"vr": "LO", "Value": ["PT-2"]},
        }
        # A STANDARD request: the matching key lives inside the SPS sequence.
        ds = Dataset()
        ds.QueryRetrieveLevel = "WORKLIST"
        ds.AccessionNumber = ""
        ds.RequestedProcedureID = ""
        # PatientID is a Return key (PS3.4 Table K.6-1), but the assertion
        # below checks it, so the identifier must NAME it: a row that carries
        # patient identifiers the modality never asked for is not what this
        # test is pinning, and a converter that returns only requested keys
        # is conformant.
        ds.PatientID = ""
        sps = Dataset()
        sps.ScheduledStationAETitle = "STATION01"
        sps.ScheduledProcedureStepID = ""
        sps.ScheduledProcedureStepStartDate = ""
        ds.ScheduledProcedureStepSequence = [sps]

        class Ctx:
            abstract_syntax = "1.2.840.10008.5.1.4.31"

        class Req:
            ae_title = "MWL_SCU"

        class Assoc:
            requestor = Req()

        class Event:
            identifier = ds
            context = Ctx()
            assoc = Assoc()

        with patch(
            "clean_code.handlers.cfind._qido_request", return_value=[workitem]
        ) as mock_qido:
            responses = list(
                _handle_mwl(
                    Event(), "http://127.0.0.1:8024/dicomweb", False, max_responses=50
                )
            )

        # The nested station key reached the server as a filter.
        called_url = mock_qido.call_args[0][0]
        called_params = mock_qido.call_args[0][1]
        assert "workitems" in called_url
        assert called_params == {"00404025.00080100": "STATION01"}

        pendings = [r for r in responses if r[0] == 0xFF00]
        assert pendings, "expected at least one Pending response"
        row = pendings[0][1]
        # PatientID is a Return key (PS3.4 Table K.6-1), and the identifier
        # above names it: the row is checked against a key the modality
        # actually asked for, not one the bridge volunteered.
        assert str(row.PatientID) == "PT-2"
        assert "ScheduledProcedureStepSequence" in row
        item = row.ScheduledProcedureStepSequence[0]
        assert str(item.ScheduledStationAETitle) == "STATION01"
        assert str(item.ScheduledProcedureStepID) == WORKITEM_UID[-16:]
        # The modality asked for these, so they are present.
        assert str(row.RequestedProcedureID) == STUDY_UID[-16:]
