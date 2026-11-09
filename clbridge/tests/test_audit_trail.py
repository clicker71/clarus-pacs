# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Audit trail (0400,0561): the bridge records WHO changed what.

PS3.3 C.12.1 - ModifyingSystem (0400,0563) names the system that applied the
change. That is the bridge itself, so the value must be its configured AE
title, never a literal that disagrees with the title it associates under.
"""

import pytest
from clean_code.validation import record_modification, sanitize_dataset
from pydicom import Dataset


def _empty_patient():
    """A dataset with the Type 2 fields empty, as a modality may send them."""
    ds = Dataset()
    ds.PatientID = ""
    ds.PatientName = ""
    return ds


def test_record_modification_uses_the_given_system():
    ds = Dataset()
    ds.PatientID = "RAW"
    record_modification(ds, (0x0010, 0x0020), "OLD", modifying_system="TEST_BRIDGE")
    assert ds.OriginalAttributesSequence[0].ModifyingSystem == "TEST_BRIDGE"


def test_sanitize_dataset_threads_the_system_into_every_audit_item():
    ds = _empty_patient()
    sanitize_dataset(ds, write_audit=True, modifying_system="TEST_BRIDGE")
    systems = [item.ModifyingSystem for item in ds.OriginalAttributesSequence]
    assert systems == ["TEST_BRIDGE", "TEST_BRIDGE"], systems


def test_record_modification_rejects_a_missing_system():
    """(0400,0563) is Type 1: writing it empty is a malformed audit entry, so
    a miswired caller fails fast instead of producing one silently."""
    ds = Dataset()
    ds.PatientID = "RAW"
    with pytest.raises(ValueError, match="modifying_system"):
        record_modification(ds, (0x0010, 0x0020), "OLD")


def test_sanitize_dataset_audit_without_system_raises():
    """write_audit=True needs the AE title; only write_audit=False defers it."""
    ds = _empty_patient()
    with pytest.raises(ValueError, match="modifying_system"):
        sanitize_dataset(ds, write_audit=True)


def test_sanitize_dataset_rejects_before_mutating_anything():
    """The title is validated BEFORE the first field is written.

    Mutating first and failing in record_modification would fill PatientID
    with the generated value and lose the original empty one from the audit
    trail, so a caller that retries with a valid title records a false "old".
    """
    ds = _empty_patient()
    with pytest.raises(ValueError, match="modifying_system"):
        sanitize_dataset(ds, write_audit=True)

    assert str(ds.get((0x0010, 0x0020)).value or "") == ""
    assert str(ds.get((0x0010, 0x0010)).value or "") == ""
    assert (0x0400, 0x0561) not in ds


def test_sanitize_dataset_without_audit_needs_no_system():
    """write_audit=False defers the entry to the outbox worker - no write here."""
    ds = _empty_patient()
    mods = sanitize_dataset(ds, write_audit=False)
    assert len(mods) == 2
    assert (0x0400, 0x0561) not in ds


@pytest.mark.parametrize("value", ["CLBRIDGE", "TEST_BRIDGE", "A" * 16])
def test_system_value_is_written_verbatim(value):
    ds = Dataset()
    record_modification(ds, (0x0010, 0x0020), "OLD", modifying_system=value)
    assert str(ds.OriginalAttributesSequence[0].ModifyingSystem) == value
