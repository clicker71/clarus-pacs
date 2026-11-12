// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM parser trait and types.
//!
//! Defines the streaming DICOM parsing abstraction used by STOW handlers.
//! Implementations MUST NOT load pixel data (> 7FE0,0010) into memory.

use std::collections::HashMap;
use std::fmt;

/// DICOM tag: (group, element) pair.
pub type DicomTag = (u16, u16);

/// Errors specific to DICOM parsing.
///
/// Manual `Display`/`Error` impls (thiserror removed, local
/// `Display` per enum). Messages are byte-identical to the previous derive.
#[derive(Debug)]
pub enum DicomError {
    /// DICOM file is malformed or not a valid DICOM.
    InvalidDicom(String),

    /// Required DICOM tag is absent from the file.
    MissingRequiredTag(u16, u16),

    /// Internal parse / format error.
    ParseError(String),

    /// Underlying I/O error.
    Io(std::io::Error),
}

impl fmt::Display for DicomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DicomError::InvalidDicom(s) => write!(f, "Invalid DICOM: {s}"),
            DicomError::MissingRequiredTag(g, e) => {
                write!(f, "Missing required DICOM tag: ({g:04X},{e:04X})")
            }
            DicomError::ParseError(s) => write!(f, "DICOM parse error: {s}"),
            DicomError::Io(e) => write!(f, "DICOM I/O error: {e}"),
        }
    }
}

impl std::error::Error for DicomError {}

impl From<std::io::Error> for DicomError {
    fn from(e: std::io::Error) -> Self {
        DicomError::Io(e)
    }
}

/// Metadata extracted from a single DICOM SOP instance.
///
/// Contains the union of Patient / Study / Series / Instance-level
/// tags required for QIDO search and storage indexing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstanceMeta {
    pub patient_name: String,
    pub patient_id: String,
    /// (0010,0021) IssuerOfPatientID - LO, captured when present.
    ///
    /// AIW-I 4.80.4.1.2.1 requires an issuer for every Patient ID a UPS
    /// request states, and the STOW-triggered requestor's only source for it
    /// is the study it has just stored.
    pub patient_id_issuer: Option<String>,
    pub patient_birth_date: Option<String>,
    pub patient_sex: Option<String>,
    /// (0010,2297) ResponsiblePerson - PN. FR-vet stage 1: captured when present.
    pub responsible_person: Option<String>,
    /// (0010,1002) OtherPatientIDsSequence > (0010,0020) PatientID - LO.
    ///
    /// MULTI-VALUED: the sequence lists every other identifier the patient is
    /// known by, and PS3.18 reaches any of them through the path key
    /// `00101002.00100020`. Stored comma-joined (the `modalities_in_study`
    /// shape), because a set of identifiers is what the sequence states.
    pub other_patient_ids: Vec<String>,
    pub study_instance_uid: String,
    /// (0020,0010) StudyID - the study's shorthand ID, captured when present.
    pub study_id: Option<String>,
    pub study_date: Option<String>,
    pub study_time: Option<String>,
    pub study_description: Option<String>,
    pub accession_number: Option<String>,
    pub referring_physician: Option<String>,
    pub performing_physician: Option<String>,
    pub name_of_reading_physicians: Option<String>,
    /// (0008,0080) InstitutionName - LO. 08.09 capture + study search key.
    pub institution_name: Option<String>,
    /// (0032,1032) RequestingPhysician - PN. 08.09 capture + study key.
    pub requesting_physician: Option<String>,
    /// (0032,1060) RequestedProcedureDescription - LO. 08.09 capture + key.
    pub requested_procedure_description: Option<String>,
    pub series_instance_uid: String,
    pub series_number: Option<i32>,
    pub series_description: Option<String>,
    pub series_date: Option<String>,
    pub series_time: Option<String>,
    /// (0040,0244) PerformedProcedureStepStartDate - DA, captured from source.
    pub pps_start_date: Option<String>,
    /// (0040,0245) PerformedProcedureStepStartTime - TM, captured from source.
    pub pps_start_time: Option<String>,
    /// (0040,0275) > (0040,0009) ScheduledProcedureStepID - SH.
    ///
    /// PS3.18 Table 10.6.1-5 lists `00400275.00400009` as a REQUIRED series
    /// matching attribute, so the scheduled step identity is captured from the
    /// first item of Request Attributes Sequence and becomes a stored series
    /// field - a key a search accepts has to match a value the store holds.
    pub scheduled_procedure_step_id: Option<String>,
    /// (0040,0275) > (0040,1001) RequestedProcedureID - SH.
    ///
    /// The second REQUIRED nested key of the same table. The same sequence
    /// item carries both, so they are read in one walk.
    pub requested_procedure_id: Option<String>,
    pub station_name: Option<String>,
    pub modality: String,
    /// (0008,0070) Manufacturer - LO. 08.09 capture + series search key.
    pub manufacturer: Option<String>,
    /// (0008,1040) InstitutionalDepartmentName - LO. 08.09 capture + key.
    pub institutional_department_name: Option<String>,
    /// (0008,1070) OperatorsName - PN. 08.09 capture + series search key.
    pub operators_name: Option<String>,
    pub body_part_examined: Option<String>,
    /// (0018,1030) ProtocolName - LO. 08.09 capture + series search key.
    pub protocol_name: Option<String>,
    pub laterality: Option<String>,
    pub sop_instance_uid: String,
    pub sop_class_uid: Option<String>,
    pub instance_number: Option<i32>,
    pub transfer_syntax_uid: Option<String>, // (0002,0010) TransferSyntaxUID
    /// Byte offset of the (7FE0,0010) PIXEL DATA element's TAG, from the start
    /// of the file - the seam where header attributes may still be inserted.
    ///
    /// A caller that must add a missing patient identifier to the stored blob
    /// has to write it BEFORE PixelData: the metadata reader stops at the pixel
    /// data, so anything appended to the tail is invisible to WADO-RS
    /// `/metadata`. The position comes from the parse walk, not from a byte
    /// search, so pixel data whose bytes happen to look like the tag cannot
    /// move it. `None` when the file has no PixelData before EOF.
    pub pixel_data_offset: Option<u64>,
    /// Number of rows in the image (0028,0010). `None` for non-image SOPs.
    pub rows: Option<u16>,
    /// Number of columns in the image (0028,0011). `None` for non-image SOPs.
    pub cols: Option<u16>,
    /// 3008 hidden-PHI: the file's own (0400,0550)/(0400,0561) sequences carry
    /// identifying values (PN/PID/BirthDate/Sex) while the patient fields were
    /// empty - the sanitizer must excise them before serving.
    pub hidden_identifying_attrs: bool,
    /// 3008 hidden-PHI: raw byte ranges (element start..element end incl.
    /// pad) of every TOP-LEVEL (0400,0550)/(0400,0561) element, including
    /// those after PixelData (our own Updated-file shape). Empty when absent
    /// (no allocation on the normal path).
    pub original_attrs_ranges: Vec<(usize, usize)>,
    /// Referenced Series Instance UIDs, for a derived object: a presentation
    /// state names the series it applies to in `ReferencedSeriesSequence`
    /// (0008,1115), and a segmentation names its segments' series in the same
    /// sequence. Captured so the deletion cascade can find the object by
    /// where it points, without reading DICOM bytes at delete time.
    pub referenced_series_uids: Vec<String>,
    /// Referenced SOP Instance UIDs, for a derived object: a presentation
    /// state names individual images in `ReferencedImageSequence` (0008,1140)
    /// and a segmentation names its source images in `SourceImageSequence`
    /// (0008,2112).
    pub referenced_sop_instance_uids: Vec<String>,
    /// Instances an IOCM rejection note names, as `(sop_class_uid,
    /// sop_instance_uid)`.
    ///
    /// Populated ONLY for a Key Object Selection (88.59) whose Document Title
    /// is a recognised rejection reason (see
    /// [`crate::rejection::REASONS`]); empty otherwise and for every ordinary
    /// image. Empty is the common case and the fast path: the walk runs once at
    /// STOW and the vector allocates only when the note states a reference.
    ///
    /// The SOP class travels with the UID because the marking step needs it to
    /// apply the per-reason scope without re-reading the referenced instance.
    pub rejection_referenced_sops: Vec<(String, String)>,
    /// The rejection-note reason, as the persisted integer code (113001 /
    /// 113037 / 113038 / 113039), or [`crate::rejection::NOT_REJECTED`].
    ///
    /// The marking step needs the REASON, not just the references: Quality and
    /// the IOCM AnyClass reasons differ in two ways that outlive the request -
    /// whether the hide may be lifted by a review surface, and whether a later
    /// STOW of the same instance is acceptable at all. Keeping only the
    /// references forced both decisions to be re-derived from a table every
    /// time, which is exactly where the two paths would drift apart.
    pub rejection_reason_code: u32,
}

/// Streaming DICOM parser interface.
///
/// Implementations extract DICOM tags from raw byte streams
/// **without loading pixel data** (tag `7FE0,0010`) into memory.
pub trait DicomParser: Send + Sync {
    /// Extract full instance metadata from a DICOM byte slice.
    ///
    /// # Invariants
    /// - Pixel data (7FE0,0010) MUST NOT be decoded or loaded.
    /// - Missing required tags -> `Err(DicomError::MissingRequiredTag)`.
    fn extract_metadata(&self, data: &[u8]) -> Result<InstanceMeta, DicomError>;

    /// Extract only the tags needed for QIDO search filtering.
    ///
    /// Returns a map of (group, element) -> string value for each
    /// search-relevant tag found in the data.
    fn extract_search_tags(&self, data: &[u8]) -> Result<HashMap<DicomTag, String>, DicomError>;
}
