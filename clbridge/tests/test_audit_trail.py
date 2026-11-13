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


def test_the_deferred_tuple_records_the_empty_string():
    """The .mods sidecar tuple must carry what the attribute ACTUALLY held -
    the empty string - so the audit entry the worker writes later states the
    real old value. "(empty)" is a LOG placeholder, never audit data."""
    ds = _empty_patient()
    mods = sanitize_dataset(ds, write_audit=False)
    olds = {tag: old for tag, old, _new in mods}
    assert olds[(0x0010, 0x0020)] == "", (
        "PatientID old value must be the empty string, not '(empty)': "
        f"{olds[(0x0010, 0x0020)]!r}"
    )
    assert olds[(0x0010, 0x0010)] == "", (
        "PatientName old value must be the empty string: " f"{olds[(0x0010, 0x0010)]!r}"
    )


def test_sync_and_deferred_paths_record_the_same_old_value():
    """The synchronous audit entry and the deferred tuple must agree on what
    the attribute held before the fix."""
    # Deferred path: the tuple travels to the worker via the .mods sidecar.
    ds = _empty_patient()
    deferred_olds = [old for _tag, old, _new in sanitize_dataset(ds, write_audit=False)]

    # Synchronous path: the entry is written immediately.
    ds_sync = _empty_patient()
    sanitize_dataset(ds_sync, write_audit=True, modifying_system="TEST_BRIDGE")
    sync_olds = []
    for item in ds_sync.OriginalAttributesSequence:
        mod_item = item.ModifiedAttributesSequence[0]
        for tag in ((0x0010, 0x0020), (0x0010, 0x0010)):
            if tag in mod_item:
                sync_olds.append(str(mod_item[tag].value))

    assert deferred_olds == ["", ""], deferred_olds
    assert deferred_olds == sync_olds, (
        f"the two paths must record the same old value: deferred={deferred_olds} "
        f"sync={sync_olds}"
    )


@pytest.mark.parametrize("value", ["CLBRIDGE", "TEST_BRIDGE", "A" * 16])
def test_system_value_is_written_verbatim(value):
    ds = Dataset()
    record_modification(ds, (0x0010, 0x0020), "OLD", modifying_system=value)
    assert str(ds.OriginalAttributesSequence[0].ModifyingSystem) == value
