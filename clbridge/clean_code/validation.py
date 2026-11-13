# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

"""Clarus Bridge  -  DICOM tag validation and coercion (FR-001).

Bridge sanitizes empty DICOM Type 2 fields (PatientID, PatientName)
BEFORE sending to the server. Records all modifications in
Original Attributes Sequence (0400,0561)  -  the bridge's audit trail.

Server also validates independently  -  two levels of protection.
"""

import hashlib
import secrets
import time

from pydicom.dataset import Dataset
from pydicom.sequence import Sequence


def generate_patient_id(prefix="AUTOPAT_"):
    """Generate unique PatientID: AUTOPAT_YYYYMMDD_HHMMSS_RRRRR.

    Fallback for when StudyInstanceUID is unavailable.
    Prefer generate_patient_id_from_study() for deterministic grouping.
    Uses timestamp + secrets for uniqueness across concurrent C-STORE calls.
    """
    ts = time.strftime("%Y%m%d_%H%M%S")
    rnd = secrets.token_hex(4)  # 8 hex chars  -  sufficient entropy for fallback
    return f"{prefix}{ts}_{rnd}"


def generate_patient_id_from_study(study_uid, prefix="AUTOPAT_"):
    """Generate deterministic PatientID from StudyInstanceUID.

    # All SOPs in one study must map to the same PatientID, otherwise the
    # DICOMweb hierarchy would split a single study across patients.
    SHA-256 truncated to 12 hex chars -> collision probability < 2^-48.
    Deterministic -> same StudyUID always produces same PatientID.
    """
    h = hashlib.sha256(study_uid.encode()).hexdigest()[:12].upper()
    return f"{prefix}{h}"


def generate_patient_name(prefix="Anonymous^"):
    """Generate DICOM PatientName in PN format: Anonymous^Generated.

    Fallback when StudyInstanceUID is unavailable.
    """
    return f"{prefix}Generated"


def generate_patient_name_from_study(study_uid, prefix="Anonymous^"):
    """Generate deterministic PatientName from StudyInstanceUID.

    Format: Anonymous^A1B2C3  -  unique per study, minimal.
    """
    h = hashlib.sha256(study_uid.encode()).hexdigest()[:6].upper()
    return f"{prefix}{h}"


def record_modification(
    ds,
    tag,
    old_value,
    reason="COERCE",
    modifying_operator=None,
    modifying_system=None,
):
    """Write modification to Original Attributes Sequence (0400,0561).

    DICOM PS3.3 C.12.1: Original Attributes Sequence records what was changed,
    by whom, when, and why. Bridge is the single point of audit  -  server does
    NOT write this tag.

    Structure:
      OriginalAttributesSequence (0400,0561)
        Item:
          AttributeModificationDateTime (0400,0562)
          ModifyingSystem (0400,0563)
          SourceOfPreviousValues (0400,0564)  -  Type 2, may be empty
          ReasonForModification (0400,0565)
          ModifiedAttributesSequence (0400,0550)
            Item: the actual changed tag with its OLD value

    Args:
        ds: pydicom Dataset (modified in-place)
        tag: DICOM tag tuple, e.g. (0x0010, 0x0020)
        old_value: original value before modification
        reason: DICOM CID 7050 code (COERCE, CORRECT, etc.)
        modifying_operator: who made the change (DIMSE AE Title for C-STORE, else "DIMSE")
        modifying_system: (0400,0563) system that applied the change - the
            bridge's configured AE title. PS3.3 C.12.1 wants the system
            identified, so the caller supplies it; a literal here would
            disagree with the AE title the bridge actually associates under.
    """
    from datetime import datetime

    from pydicom.dataelem import DataElement

    if not modifying_system:
        # (0400,0563) is Type 1 (PS3.3 C.12.1). An empty value would be a
        # malformed audit entry, and a silent default would hide the miswiring
        # that caused it - refuse the write instead.
        raise ValueError("modifying_system is required for ModifyingSystem (0400,0563)")

    # -- Original Attributes Sequence item (who/when/why) --
    # DICOM PS3.3 Table C.12.1.1.9-1: Original Attributes Macro
    attr_item = Dataset()
    attr_item.SourceOfPreviousValues = ""  # (0400,0564) Type 2  -  may be empty
    attr_item.AttributeModificationDateTime = datetime.now().strftime(
        "%Y%m%d%H%M%S"
    )  # (0400,0562) Type 1
    attr_item.ModifyingSystem = modifying_system  # (0400,0563) Type 1
    attr_item.ReasonForTheAttributeModification = (
        reason  # (0400,0565) Type 1  -  CID 7050: COERCE
    )
    # (0400,0566) ModifyingOperator is NOT a standard attribute: the Attribute
    # Modification macro's "who" is (0400,0562) DateTime, (0400,0563) Modifying
    # System and (0400,0564) Source of Previous Values (PS3.3 C.12.1.1.9.1),
    # and (0400,0566) is absent from the PS3.6 registry (verified against the
    # installed pydicom 3.0.2 dictionary). Group 0400 is even, so it cannot
    # carry a private element either (PS3.5 7.8.1). Writing it produced a tag
    # no reader can resolve, so the operator travels in ModifyingSystem above.

    # -- ModifiedAttributesSequence (0400,0550)  -  WHAT was changed --
    # Type 1. Single Item containing the modified tag with its OLD value.
    # Preserve VR from original element, fallback to LO (Long String).
    vr = ds[tag].VR if tag in ds else "LO"
    mod_attr = Dataset()
    # PS3.3 C.12.1.1.9.1: the item holds the PREVIOUS value. `old_value or ""`
    # replaced every falsy previous value - 0, 0.0, False - with an empty
    # string, so an audit entry for a changed numeric attribute recorded "no
    # previous value" instead of the zero it really held. Only None (no value
    # at all) becomes empty.
    mod_attr[tag] = DataElement(tag, vr, "" if old_value is None else old_value)
    mod_seq = Sequence()
    mod_seq.append(mod_attr)
    attr_item.ModifiedAttributesSequence = mod_seq

    # Add to main dataset
    if (0x0400, 0x0561) not in ds:
        ds.OriginalAttributesSequence = Sequence()
    ds.OriginalAttributesSequence.append(attr_item)


def sanitize_dataset(
    ds,
    fix_patient_id=True,
    fix_patient_name=True,
    id_prefix="AUTOPAT_",
    name_prefix="Anonymous^",
    write_audit=True,
    study_uid=None,
    modifying_system=None,
):
    """Sanitize empty DICOM Type 2 fields. Returns list of modifications.

    Args:
        ds: pydicom Dataset (modified in-place)
        fix_patient_id: generate PatientID if empty
        fix_patient_name: generate PatientName if empty
        id_prefix: prefix for generated PatientID
        name_prefix: prefix for generated PatientName
        write_audit: if True, record modification in (0400,0561) now.
                     If False, skip  -  caller must apply record_modification later
                     (used to defer audit trail writing off C-STORE hot-path).
        study_uid: if provided, generates deterministic PatientID/Name from
                   StudyInstanceUID. All SOPs in the same study get the same
                   PatientID  -  preserving the DICOM patient-study relationship
                   and preventing SQLite performance degradation.
        modifying_system: (0400,0563) value for the audit entry - the bridge's
                   configured AE title, threaded to record_modification().

    Returns:
        list of (tag, old_value, new_value) tuples for logging
    """
    if write_audit and not modifying_system:
        # Validated BEFORE any field is overwritten: mutating first and failing
        # inside record_modification would leave the dataset changed with no
        # audit entry carrying the original empty value, so a caller retrying
        # with a valid title would record a false "old" value.
        raise ValueError("modifying_system is required for ModifyingSystem (0400,0563)")

    modifications = []

    # PatientID (0010,0020)  -  Type 2
    pid = ds.get((0x0010, 0x0020))
    if not pid or not pid.value:
        # Two consumers with different jobs: the audit entry records what the
        # attribute ACTUALLY held - the empty string for an absent or empty
        # Type 2 field (PS3.3 C.2.2) - so the tuple carries `old`, never a
        # placeholder. The "(empty)" substitution belongs to the LOG line in
        # cstore.py, which needs a readable placeholder the audit must not
        # record. Recording "(empty)" in the Modified Attributes Sequence
        # would state a value the attribute never had.
        old = str(pid.value) if (pid and pid.value) else ""
        if fix_patient_id:
            if study_uid:
                new_id = generate_patient_id_from_study(study_uid, id_prefix)
            else:
                new_id = generate_patient_id(id_prefix)
            ds.PatientID = new_id
            if write_audit:
                record_modification(
                    ds, (0x0010, 0x0020), old, modifying_system=modifying_system
                )
            modifications.append(((0x0010, 0x0020), old, new_id))
        else:
            raise ValueError("PatientID is empty and fix_patient_id is disabled")

    # PatientName (0010,0010)  -  Type 2
    pname = ds.get((0x0010, 0x0010))
    if not pname or not pname.value:
        old = str(pname.value) if (pname and pname.value) else ""
        if fix_patient_name:
            if study_uid:
                new_name = generate_patient_name_from_study(study_uid, name_prefix)
            else:
                new_name = generate_patient_name(name_prefix)
            ds.PatientName = new_name
            if write_audit:
                record_modification(
                    ds, (0x0010, 0x0010), old, modifying_system=modifying_system
                )
            modifications.append(((0x0010, 0x0010), old, new_name))
        else:
            raise ValueError("PatientName is empty and fix_patient_name is disabled")

    return modifications
