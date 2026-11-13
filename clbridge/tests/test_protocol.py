# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for clarus_bridge.protocol — QIDO/C-FIND/N-ACTION protocol helpers."""

import pytest
from clean_code.protocol import (
    QIDO_KEY_TO_TAG,
    QIDO_SERIES_KEY_TO_TAG,
    _build_n_action_rsp,
    _build_qido_params,
    _extract_request_keywords,
    _json_to_dataset,
    _json_to_dataset_series,
    _parse_n_action_rq,
    _split_qido_params,
)
from pydicom import Dataset


class TestBuildQidoParams:
    """Tests for _build_qido_params()."""

    def test_empty_dataset_returns_empty_params(self):
        ds = Dataset()
        params = _build_qido_params(ds)
        assert params == {}

    def test_patient_name_maps_correctly(self):
        ds = Dataset()
        ds.PatientName = "Smith^John"
        params = _build_qido_params(ds)
        assert params == {"PatientName": "Smith^John"}

    def test_empty_value_is_skipped(self):
        """Empty C-FIND values are skipped — absent param = match all per DICOMweb."""
        ds = Dataset()
        ds.StudyDescription = ""
        params = _build_qido_params(ds)
        assert params == {}

    def test_empty_patient_id_is_not_wildcarded(self):
        ds = Dataset()
        ds.PatientID = ""
        params = _build_qido_params(ds)
        # PatientID with empty value should NOT become wildcard
        assert "PatientID" not in params or params.get("PatientID") != "*"

    def test_sq_element_skipped(self):
        ds = Dataset()
        # Add an SQ element — should be skipped
        ds.add_new((0x0008, 0x1111), "SQ", [Dataset()])
        ds.PatientName = "Test"
        params = _build_qido_params(ds)
        assert params == {"PatientName": "Test"}

    def test_none_value_skipped_all_vrs(self):
        """31.08 field bug: an empty IS (SeriesNumber) decodes to None in
        pydicom; str(None) == 'None' was sent to the server -> 400."""
        ds = Dataset()
        ds.PatientName = "Mastyaeva*"
        ds.add_new((0x0020, 0x0011), "IS", None)  # SeriesNumber empty
        ds.add_new((0x0008, 0x0021), "DA", None)  # SeriesDate empty
        ds.add_new((0x0008, 0x0060), "CS", None)  # Modality empty
        params = _build_qido_params(ds)
        assert params == {"PatientName": "Mastyaeva*"}
        assert "SeriesNumber" not in params
        assert "SeriesDate" not in params
        assert "Modality" not in params

    def test_whitespace_value_skipped(self):
        ds = Dataset()
        ds.PatientName = "Test*"
        ds.Modality = "   "
        ds.StudyDate = ""
        params = _build_qido_params(ds)
        assert params == {"PatientName": "Test*"}


class TestSplitQidoParams:
    """Tests for _split_qido_params() - study vs series query sets."""

    def test_study_and_series_split(self):
        ds = Dataset()
        ds.PatientName = "Mastyaeva*"
        ds.StudyInstanceUID = "1.2.3.4"
        ds.Modality = "DX"
        ds.SeriesNumber = "3"
        ds.SeriesDate = "20170203"
        study_params, series_params = _split_qido_params(ds)
        # study-level keys (server /studies rejects series keys with 400)
        assert study_params == {
            "PatientName": "Mastyaeva*",
            "StudyInstanceUID": "1.2.3.4",
        }
        # series step gets series-only keys + Modality (legal at both
        # levels, applied on the SERIES step only - the study-level
        # Modality filter matches the first series only, server deviation)
        assert series_params == {
            "Modality": "DX",
            "SeriesNumber": "3",
            "SeriesDate": "20170203",
        }

    def test_empty_identifier_both_empty(self):
        ds = Dataset()
        study_params, series_params = _split_qido_params(ds)
        assert study_params == {}
        assert series_params == {}

    def test_empty_series_number_not_in_either_set(self):
        ds = Dataset()
        ds.PatientName = "Mastyaeva*"
        ds.add_new((0x0020, 0x0011), "IS", None)
        study_params, series_params = _split_qido_params(ds)
        assert study_params == {"PatientName": "Mastyaeva*"}
        assert series_params == {}


class TestJsonToDataset:
    """Tests for _json_to_dataset()."""

    def test_converts_patient_name_correctly(self):
        json_obj = {"00100010": {"Value": [{"Alphabetic": "Smith^John"}]}}
        request_keywords = {"PatientName"}
        ds = _json_to_dataset(json_obj, request_keywords)
        assert ds.PatientName == "Smith^John"

    def test_converts_study_instance_uid(self):
        json_obj = {"0020000D": {"Value": ["1.2.3.4.5.6.7.8.9"]}}
        request_keywords = set()
        ds = _json_to_dataset(json_obj, request_keywords)
        # StudyInstanceUID is always included
        assert ds.StudyInstanceUID == "1.2.3.4.5.6.7.8.9"

    def test_unknown_json_key_ignored(self):
        json_obj = {"99999999": {"Value": ["bad"]}}
        request_keywords = set()
        ds = _json_to_dataset(json_obj, request_keywords)
        # Only SpecificCharacterSet (0008,0005) is always added
        assert len(list(ds)) == 1

    def test_charset_stays_utf8_when_study_json_has_iso_ir_100(self):
        """INC-004 class: a study row carrying its own charset must not
        overwrite the forced UTF-8, or a Cyrillic value becomes unencodable.

        The series map already omits 0008,0005 for this reason; the study map
        mapped it, so the server's Latin-1 declaration replaced the forced
        UTF-8 and pydicom then wrote '?' replacement characters on the wire.
        """
        import io

        from pydicom import dcmwrite
        from pydicom.dataset import FileMetaDataset
        from pydicom.uid import ExplicitVRLittleEndian

        study_json = {
            "00080005": {"vr": "CS", "Value": ["ISO_IR 100"]},
            "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Петров^Иван"}]},
            "0020000D": {"vr": "UI", "Value": ["1.2.3"]},
        }
        ds = _json_to_dataset(study_json, {"PatientName"})

        assert str(ds.SpecificCharacterSet) == "ISO_IR 192"
        assert (
            "00080005" not in QIDO_KEY_TO_TAG
        ), "mapping 0008,0005 lets the server charset overwrite the forced UTF-8"
        assert "?" not in str(ds.PatientName)

        # The declaration is only useful if the value ENCODES: with the wrong
        # charset pydicom warns "using replacement characters" and writes '?'.
        ds.file_meta = FileMetaDataset()
        ds.file_meta.TransferSyntaxUID = ExplicitVRLittleEndian
        ds.file_meta.MediaStorageSOPClassUID = "1.2.840.10008.5.1.4.1.1.2"
        ds.file_meta.MediaStorageSOPInstanceUID = "1.2.3.4.5"
        buf = io.BytesIO()
        dcmwrite(buf, ds, enforce_file_format=True)
        assert (
            "Петров^Иван".encode("utf-8") in buf.getvalue()
        ), "the Cyrillic name was not encoded as UTF-8 on the wire"


class TestJsonToDatasetSeries:
    """Tests for _json_to_dataset_series() - series rows + study identity."""

    def test_series_attrs_and_study_identity_injected(self):
        series_json = {
            "00080060": {"Value": ["DX"]},
            "00080021": {"Value": ["20170203"]},
            "0020000E": {"Value": ["2.25.5555666677778888"]},
            "00200011": {"Value": [1]},
        }
        study_json = {
            "00100010": {"Value": [{"Alphabetic": "Mastyaeva^Anna"}]},
            "00100020": {"Value": ["auto6853"]},
            "0020000D": {
                "Value": ["1.2.826.0.1.3680043.2.634.0.34696.201723.125318.153"]
            },
        }
        ds = _json_to_dataset_series(series_json, set(), study_json)
        assert str(ds.SeriesInstanceUID) == "2.25.5555666677778888"
        assert ds.SeriesNumber == 1
        assert str(ds.SeriesDate) == "20170203"
        assert str(ds.Modality) == "DX"
        # study identity injected from the owning study row
        assert str(ds.PatientName) == "Mastyaeva^Anna"
        assert str(ds.PatientID) == "auto6853"
        assert (
            str(ds.StudyInstanceUID)
            == "1.2.826.0.1.3680043.2.634.0.34696.201723.125318.153"
        )

    def test_series_number_zero_kept(self):
        series_json = {"00200011": {"Value": [0]}}
        ds = _json_to_dataset_series(series_json, set(), None)
        assert ds.SeriesNumber == 0

    def test_charset_stays_utf8_when_series_json_has_iso_ir_100(self):
        # Live server emits 00080005 ISO_IR 100 in series rows; the
        # converter must NOT map it (Cyrillic would break).
        series_json = {"00080005": {"Value": ["ISO_IR 100"]}}
        ds = _json_to_dataset_series(series_json, set(), None)
        assert str(ds.SpecificCharacterSet) == "ISO_IR 192"
        assert "00080005" not in QIDO_SERIES_KEY_TO_TAG

    def test_no_study_json_no_identity(self):
        series_json = {"0020000E": {"Value": ["2.25.5555666677778888"]}}
        ds = _json_to_dataset_series(series_json, set(), None)
        assert str(ds.SeriesInstanceUID) == "2.25.5555666677778888"
        assert "PatientName" not in ds
        assert "StudyInstanceUID" not in ds


class TestJsonKeyToKeyword:
    """Tests for QIDO_KEY_TO_TAG keyword mapping."""

    def test_known_key_maps_correctly(self):
        assert QIDO_KEY_TO_TAG["00100010"][3] == "PatientName"
        assert QIDO_KEY_TO_TAG["0020000D"][3] == "StudyInstanceUID"
        assert QIDO_KEY_TO_TAG["00080060"][3] == "Modality"

    def test_unknown_key_returns_empty(self):
        assert "99999999" not in QIDO_KEY_TO_TAG


class TestExtractRequestKeywords:
    """Tests for _extract_request_keywords()."""

    def test_extracts_keywords_from_dataset(self):
        ds = Dataset()
        ds.PatientName = "Test"
        ds.StudyDate = "20260101"
        keywords = _extract_request_keywords(ds)
        assert keywords == {"PatientName", "StudyDate"}

    def test_skips_sq_elements(self):
        ds = Dataset()
        ds.PatientName = "Test"
        ds.add_new((0x0008, 0x1111), "SQ", [Dataset()])
        keywords = _extract_request_keywords(ds)
        # `"SQ" not in keywords` would pass even if the sequence leaked in
        # under its own keyword; the point is that ONLY the non-SQ element is
        # extracted, and the sequence is absent under any name.
        assert keywords == {"PatientName"}, {
            "a sequence must contribute no matching key: " f"{keywords}"
        }


class TestParseNActionRq:
    """Tests for _parse_n_action_rq()."""

    def test_missing_transaction_uid_raises_value_error(self):
        class FakeEvent:
            dataset = Dataset()

        with pytest.raises(ValueError, match="TransactionUID"):
            _parse_n_action_rq(FakeEvent())

    def test_parses_transaction_uid_correctly(self):
        class FakeEvent:
            dataset = Dataset()

        FakeEvent.dataset.TransactionUID = "1.2.3.4.5"

        tx_uid, sop_list = _parse_n_action_rq(FakeEvent())
        assert tx_uid == "1.2.3.4.5"
        assert sop_list == []


class TestBuildNActionRsp:
    """Tests for _build_n_action_rsp()."""

    def test_builds_success_response(self):
        rsp = _build_n_action_rsp(
            "1.2.3",
            [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4")],
            [],
        )
        assert rsp.TransactionUID == "1.2.3"
        assert len(rsp.ReferencedSOPSequence) == 1

    def test_builds_failed_response(self):
        rsp = _build_n_action_rsp(
            "1.2.3",
            [],
            [("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4", 0xC201)],
        )
        assert rsp.TransactionUID == "1.2.3"
        assert len(rsp.FailedSOPSequence) == 1
