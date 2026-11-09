// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM tag constants used by the streaming parser.
//!
//! Only the 22+ tags listed in the tag inventory (Section 2) are
//! extracted - everything else is ignored.

/// DICOM tag: (group, element) pair.
pub type DicomTag = (u16, u16);

// -- Patient-level tags ---------------------------------------------------

pub const PATIENT_NAME: DicomTag = (0x0010, 0x0010);
pub const PATIENT_ID: DicomTag = (0x0010, 0x0020);
/// IssuerOfPatientID (0010,0021) - LO. Captured so the STOW-triggered
/// workitem can state the issuer AIW-I 4.80.4.1.2.1 requires for a Patient
/// ID.
pub const PATIENT_ID_ISSUER: DicomTag = (0x0010, 0x0021);
pub const PATIENT_BIRTH_DATE: DicomTag = (0x0010, 0x0030);
pub const PATIENT_SEX: DicomTag = (0x0010, 0x0040);
/// ResponsiblePerson (0010,2297) - PN, the person/organization responsible for
/// a (typically non-human) patient. FR-vet stage 1: captured and searchable.
pub const RESPONSIBLE_PERSON: DicomTag = (0x0010, 0x2297);

// -- Character encoding ----------------------------------------------------

/// SpecificCharacterSet - defines the character encoding for all
/// string values in the dataset.
/// DICOM PS3.5 Section 6.1.1: MUST be the first element in the dataset.
pub const SPECIFIC_CHARACTER_SET: DicomTag = (0x0008, 0x0005);

// -- Study-level tags -----------------------------------------------------

pub const STUDY_DATE: DicomTag = (0x0008, 0x0020);
pub const STUDY_TIME: DicomTag = (0x0008, 0x0030);
pub const ACCESSION_NUMBER: DicomTag = (0x0008, 0x0050);
pub const MODALITY: DicomTag = (0x0008, 0x0060);
/// Manufacturer (0008,0070) - LO. 08.09 capture + series search key.
pub const MANUFACTURER: DicomTag = (0x0008, 0x0070);
/// InstitutionName (0008,0080) - LO. 08.09 capture + study search key.
pub const INSTITUTION_NAME: DicomTag = (0x0008, 0x0080);
pub const REFERRING_PHYSICIAN: DicomTag = (0x0008, 0x0090);
pub const STUDY_DESCRIPTION: DicomTag = (0x0008, 0x1030);
/// InstitutionalDepartmentName (0008,1040) - LO. 08.09 capture + series key.
pub const INSTITUTIONAL_DEPARTMENT_NAME: DicomTag = (0x0008, 0x1040);
pub const PERFORMING_PHYSICIAN: DicomTag = (0x0008, 0x1050);
pub const NAME_OF_READING_PHYSICIANS: DicomTag = (0x0008, 0x1060);
/// OperatorsName (0008,1070) - PN. 08.09 capture + series search key.
pub const OPERATORS_NAME: DicomTag = (0x0008, 0x1070);
pub const STUDY_INSTANCE_UID: DicomTag = (0x0020, 0x000D);
/// StudyID (0020,0010) - the study's shorthand identifier.
pub const STUDY_ID: DicomTag = (0x0020, 0x0010);

// -- Procedure-request tags (08.09 capture + study search keys) ----------

/// RequestingPhysician (0032,1032) - PN.
pub const REQUESTING_PHYSICIAN: DicomTag = (0x0032, 0x1032);
/// RequestedProcedureDescription (0032,1060) - LO.
pub const REQUESTED_PROCEDURE_DESCRIPTION: DicomTag = (0x0032, 0x1060);

// -- Series-level tags ----------------------------------------------------

pub const SERIES_DESCRIPTION: DicomTag = (0x0008, 0x103E);
pub const STATION_NAME: DicomTag = (0x0008, 0x1010);
pub const SERIES_DATE: DicomTag = (0x0008, 0x0021);
pub const SERIES_TIME: DicomTag = (0x0008, 0x0031);
/// ProtocolName (0018,1030) - LO. 08.09 capture + series search key.
pub const PROTOCOL_NAME: DicomTag = (0x0018, 0x1030);
pub const BODY_PART_EXAMINED: DicomTag = (0x0018, 0x0015);
pub const SERIES_INSTANCE_UID: DicomTag = (0x0020, 0x000E);
pub const SERIES_NUMBER: DicomTag = (0x0020, 0x0011);
pub const LATERALITY: DicomTag = (0x0020, 0x0060);

// -- Procedure-step tags ---------------------------------------------------

/// PerformedProcedureStepStartDate (0040,0244) - DA.
pub const PPS_START_DATE: DicomTag = (0x0040, 0x0244);
/// PerformedProcedureStepStartTime (0040,0245) - TM.
pub const PPS_START_TIME: DicomTag = (0x0040, 0x0245);
/// RequestAttributesSequence (0040,0275) - SQ.
///
/// The scheduled step identity, carried per performed step. PS3.18
/// Table 10.6.1-5 types two of its nested keys as REQUIRED series matching
/// attributes (`00400275.00400009` and `00400275.00401001`), so the sequence
/// is walked for those two scalars in its first item.
pub const REQUEST_ATTRIBUTES_SEQUENCE: DicomTag = (0x0040, 0x0275);
/// ScheduledProcedureStepID (0040,0009) - SH, nested in (0040,0275).
pub const SCHEDULED_PROCEDURE_STEP_ID: DicomTag = (0x0040, 0x0009);
/// RequestedProcedureID (0040,1001) - SH, nested in (0040,0275).
pub const REQUESTED_PROCEDURE_ID: DicomTag = (0x0040, 0x1001);
/// OtherPatientIDsSequence (0010,1002) - SQ.
///
/// MULTI-VALUED: every item carries a Patient ID (0010,0020), and PS3.18
/// uses the path form `00101002.00100020` as a matching key.
pub const OTHER_PATIENT_IDS_SEQUENCE: DicomTag = (0x0010, 0x1002);

// -- Coded concepts (rejection notes) -------------------------------------

/// ConceptNameCodeSequence (0040,A043) - SQ.
///
/// The root content item's coded name. For a Key Object Selection it carries
/// the Document Title as a CODE (PS3.3 C.17.2), which is what the rejection
/// detector matches - the (0042,0010) ST string is the human-readable form and
/// is not reliably populated.
pub const CONCEPT_NAME_CODE_SEQUENCE: DicomTag = (0x0040, 0xA043);
/// CurrentRequestedProcedureEvidenceSequence (0040,A375) - SQ.
///
/// PS3.3 C.17.6.2 Table C.17.6-2 types it Type 1: the ONLY sequence listing
/// the instances a key object document applies to. Items follow the
/// Table C.17-3 hierarchical reference macro.
pub const CURRENT_REQUESTED_PROCEDURE_EVIDENCE_SEQUENCE: DicomTag = (0x0040, 0xA375);
/// CodeValue (0008,0100) - SH, inside a code sequence item.
pub const CODE_VALUE: DicomTag = (0x0008, 0x0100);
/// CodingSchemeDesignator (0008,0102) - SH, inside a code sequence item.
pub const CODING_SCHEME_DESIGNATOR: DicomTag = (0x0008, 0x0102);
/// ReferencedSeriesSequence (0008,1115) - SQ, Table C.17-3 macro.
pub const REFERENCED_SERIES_SEQUENCE: DicomTag = (0x0008, 0x1115);
/// ReferencedSOPSequence (0008,1199) - SQ, Table C.17-3 macro.
pub const REFERENCED_SOP_SEQUENCE: DicomTag = (0x0008, 0x1199);
/// ReferencedSOPClassUID (0008,1150) - UI, inside ReferencedSOPSequence.
pub const REFERENCED_SOP_CLASS_UID: DicomTag = (0x0008, 0x1150);

// -- Instance-level tags --------------------------------------------------

pub const SOP_CLASS_UID: DicomTag = (0x0008, 0x0016);
pub const SOP_INSTANCE_UID: DicomTag = (0x0008, 0x0018);
pub const INSTANCE_NUMBER: DicomTag = (0x0020, 0x0013);

// -- Meta-header tags (group 0002) ----------------------------------------

pub const TRANSFER_SYNTAX_UID: DicomTag = (0x0002, 0x0010);

// -- Image geometry -------------------------------------------------------

/// Rows - number of rows in the image (0028,0010).
pub const ROWS: DicomTag = (0x0028, 0x0010);

/// Columns - number of columns in the image (0028,0011).
pub const COLUMNS: DicomTag = (0x0028, 0x0011);

// -- Sentinel -------------------------------------------------------------

/// PixelData tag - parser must stop BEFORE reading its value.
///
/// Multi-frame files may contain an offset table and per-frame
/// metadata after this tag. Clarus v1.0.0 stops here.
/// See `#P7-MULTIFRAME` for future multi-frame support.
pub const PIXEL_DATA: DicomTag = (0x7FE0, 0x0010);

// -- 3008 hidden-PHI capture ----------------------------------------------

/// (0400,0550) ModifiedAttributesSequence - top-level occurrences carry the
/// raw byte range the sanitizer excises.
pub const MODIFIED_ATTRIBUTES_SEQ: DicomTag = (0x0400, 0x0550);
/// (0400,0561) OriginalAttributesSequence - same capture treatment (both
/// top-level shapes exist in the wild; the excise union covers each once).
pub const ORIGINAL_ATTRIBUTES_SEQ: DicomTag = (0x0400, 0x0561);

// -- Derived-object reference capture -------------------------------------

/// (0008,1115) ReferencedSeriesSequence - a presentation state or a
/// segmentation names the series it annotates here.
pub const REFERENCED_SERIES_SEQ: DicomTag = (0x0008, 0x1115);
/// (0008,1140) ReferencedImageSequence - a presentation state names the
/// individual images it applies to here.
pub const REFERENCED_IMAGE_SEQ: DicomTag = (0x0008, 0x1140);
/// (0008,2112) SourceImageSequence - a segmentation names its source images
/// here.
pub const SOURCE_IMAGE_SEQ: DicomTag = (0x0008, 0x2112);

/// (0008,1155) ReferencedSOPInstanceUID - the instance a reference sequence
/// entry points at.
pub const REFERENCED_SOP_INSTANCE_UID: DicomTag = (0x0008, 0x1155);

/// (FFFE,E000) Item - the delimiter that opens each sequence item.
pub const ITEM: DicomTag = (0xFFFE, 0xE000);
/// (FFFE,E00D) ItemDelimitationItem - closes an undefined-length item.
pub const ITEM_DELIMITATION: DicomTag = (0xFFFE, 0xE00D);
/// (FFFE,E0DD) SequenceDelimitationItem - closes an undefined-length
/// sequence.
pub const SEQUENCE_DELIMITATION: DicomTag = (0xFFFE, 0xE0DD);

/// Undefined Length (PS3.5 Section 7.5) - the length encoding that means the
/// extent is terminated by a delimiter item instead of by a count.
pub const UNDEFINED_LENGTH: u32 = 0xFFFF_FFFF;

/// The sequence tags a derived object's references are read from.
pub const REFERENCE_SEQUENCES: &[DicomTag] = &[
    REFERENCED_SERIES_SEQ,
    REFERENCED_IMAGE_SEQ,
    SOURCE_IMAGE_SEQ,
];

// -- QIDO-relevant subset -------------------------------------------------

/// All tags extracted by `extract_search_tags()`.
pub const QIDO_TAGS: &[DicomTag] = &[
    PATIENT_NAME,
    PATIENT_ID,
    PATIENT_BIRTH_DATE,
    PATIENT_SEX,
    STUDY_DATE,
    STUDY_TIME,
    ACCESSION_NUMBER,
    MODALITY,
    MANUFACTURER,
    INSTITUTION_NAME,
    REFERRING_PHYSICIAN,
    STUDY_DESCRIPTION,
    INSTITUTIONAL_DEPARTMENT_NAME,
    OPERATORS_NAME,
    STUDY_INSTANCE_UID,
    REQUESTING_PHYSICIAN,
    REQUESTED_PROCEDURE_DESCRIPTION,
    SERIES_DESCRIPTION,
    SERIES_DATE,
    SERIES_TIME,
    STATION_NAME,
    PROTOCOL_NAME,
    BODY_PART_EXAMINED,
    SERIES_INSTANCE_UID,
    SERIES_NUMBER,
    LATERALITY,
    SOP_CLASS_UID,
    SOP_INSTANCE_UID,
    INSTANCE_NUMBER,
];
