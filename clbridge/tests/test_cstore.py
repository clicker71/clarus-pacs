# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Unit tests for clarus_bridge.handlers.cstore — cp1251 charset fixer."""

from clean_code.handlers.cstore import _fix_cp1251_krakozyabry
from pydicom import Dataset


class TestFixCp1251Krakozyabry:
    """Tests for _fix_cp1251_krakozyabry()."""

    def test_valid_utf8_dataset_untouched(self):
        """A dataset with proper UTF-8 charset should pass through unchanged."""
        ds = Dataset()
        ds.PatientName = "Петров^Иван"
        ds.SpecificCharacterSet = "ISO_IR 192"  # UTF-8
        result = _fix_cp1251_krakozyabry(ds)
        assert result.PatientName == "Петров^Иван"

    def test_latin1_dataset_without_charset_fixed(self):
        """A dataset with Latin-1 decoded cp1251 text should be fixed."""
        # Simulate: the original bytes were cp1251 "Привет" but pydicom decoded as Latin-1
        original_cp1251_bytes = "Привет".encode("cp1251")
        garbled = original_cp1251_bytes.decode("latin-1")  # This is the "garbled" text
        ds = Dataset()
        ds.PatientName = garbled
        # No SpecificCharacterSet set → default is Latin-1
        result = _fix_cp1251_krakozyabry(ds)
        # After fix, it should be readable Cyrillic
        assert "Привет" in str(result.PatientName)

    def test_iso_ir_100_with_cp1251_content_fixed(self):
        """ISO_IR 100 (Latin-1) with cp1251 content should be detected and fixed."""
        original_cp1251_bytes = "Исследование".encode("cp1251")
        garbled = original_cp1251_bytes.decode("latin-1")
        ds = Dataset()
        ds.StudyDescription = garbled
        ds.SpecificCharacterSet = "ISO_IR 100"
        result = _fix_cp1251_krakozyabry(ds)
        assert "Исследование" in str(result.StudyDescription)

    def test_ascii_content_untouched(self):
        """Plain ASCII content should not be modified."""
        ds = Dataset()
        ds.PatientName = "Smith^John"
        ds.SpecificCharacterSet = "ISO_IR 100"
        result = _fix_cp1251_krakozyabry(ds)
        assert result.PatientName == "Smith^John"

    def test_fix_sets_utf8_charset(self):
        """After fixing cp1251, SpecificCharacterSet should be set to ISO_IR 192."""
        original_cp1251_bytes = "Тест".encode("cp1251")
        garbled = original_cp1251_bytes.decode("latin-1")
        ds = Dataset()
        ds.PatientName = garbled
        result = _fix_cp1251_krakozyabry(ds)
        assert str(result.SpecificCharacterSet) == "ISO_IR 192"
