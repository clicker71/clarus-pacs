// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Core domain entities for Clarus PACS.
//!
//! These models map to the Orthanc ServerIndex data model:
//! Patient -> Study -> Series -> Instance.

/// A DICOM Patient.
#[derive(Debug, Clone, PartialEq)]
pub struct Patient {
    pub id: Option<i64>,
    pub patient_uid: String,                // (0010,0020) PatientID
    pub patient_name: String,               // (0010,0010) PatientName
    pub patient_birth_date: Option<String>, // (0010,0030)
    pub patient_sex: Option<String>,        // (0010,0040)
}

/// Lightweight patient summary for the mini-viewer patient list.
/// Ordered by most recent ingestion (epoch), not StudyDate.
#[derive(Debug, Clone, PartialEq)]
pub struct PatientSummary {
    pub patient_id: i64,
    pub patient_uid: String,       // (0010,0020) PatientID
    pub patient_name: String,      // (0010,0010) PatientName
    pub last_arrival: Option<u64>, // most recent ingestion epoch (seconds)
    pub study_count: i64,          // number of studies for this patient
}

/// A DICOM Study belonging to a Patient.
///
/// Patient-level fields are denormalized into this struct for efficient
/// QIDO-RS responses (DICOMweb study-level search always includes patient
/// demographics per PS3.18 Annex F).
#[derive(Debug, Clone, PartialEq)]
pub struct Study {
    pub id: Option<i64>,
    pub patient_id: i64,
    pub study_instance_uid: String,          // (0020,000D)
    pub study_date: Option<String>,          // (0008,0020)
    pub study_time: Option<String>,          // (0008,0030)
    pub accession_number: Option<String>,    // (0008,0050)
    pub modality: Option<String>,            // (0008,0060) -- composite for study
    pub study_description: Option<String>,   // (0008,1030)
    pub referring_physician: Option<String>, // (0008,0090)
    // -- Patient-level fields (denormalized from patients table) --
    pub patient_name: String,               // (0010,0010)
    pub patient_uid: String,                // (0010,0020) PatientID
    pub patient_birth_date: Option<String>, // (0010,0030)
    pub patient_sex: Option<String>,        // (0010,0040)
    // -- Computed / auxiliary fields --
    pub study_id: Option<String>,            // (0020,0010) StudyID
    pub num_series: Option<i64>,             // (0020,1206) NumberOfStudyRelatedSeries
    pub num_instances: Option<i64>,          // (0020,1208) NumberOfStudyRelatedInstances
    pub modalities_in_study: Option<String>, // (0008,0061) ModalitiesInStudy -- comma-separated
}

/// A DICOM Series belonging to a Study.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub id: Option<i64>,
    pub study_id: i64,
    pub series_instance_uid: String,        // (0020,000E)
    pub modality: String,                   // (0008,0060)
    pub series_number: Option<i32>,         // (0020,0011)
    pub series_description: Option<String>, // (0008,103E)
    pub series_date: Option<String>,        // (0008,0021)
    pub series_time: Option<String>,        // (0008,0031)
    pub station_name: Option<String>,       // (0008,1010)
    pub body_part_examined: Option<String>, // (0018,0015)
    pub laterality: Option<String>,         // (0020,0060)
    pub num_instances: Option<i64>,         // (0020,1209) NumberOfSeriesRelatedInstances
}

/// A DICOM Instance (SOP instance) belonging to a Series.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub id: Option<i64>,
    pub series_id: i64,
    pub sop_instance_uid: String,            // (0008,0018)
    pub sop_class_uid: Option<String>,       // (0008,0016)
    pub instance_number: Option<i32>,        // (0020,0013)
    pub sha256: String,                      // SHA-256 of full DICOM file
    pub file_size: i64,                      // bytes
    pub transfer_syntax_uid: Option<String>, // (0002,0010) TransferSyntaxUID
    pub metadata_blob: Option<Vec<u8>>,      // compressed DICOM JSON (no pixels)
    pub pixel_data_sha256: Option<String>,   // SHA-256 of Pixel Data (7FE0,0010) only
    pub storage_tier: crate::StorageTier,    // where the file lives
}

/// A UPS-RS Workitem - Unified Procedure Step (PS3.18 Section 11).
///
/// States per PS3.4 Table CC.1.1-1: SCHEDULED -> IN PROGRESS ->
/// COMPLETED / CANCELED. There is no SUSPENDED state in the 2026c standard.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkItem {
    pub id: Option<i64>,
    pub workitem_uid: String, // (0008,0018) SOP Instance UID - the workitem's own identity
    pub workflow_state: String, // (0074,1000) Procedure Step State
    pub station_aet: Option<String>, // (0040,0001) Scheduled Station AE Title
    pub station_name: Option<String>, // (0040,0010) Scheduled Station Name
    pub procedure_step_start_date: Option<String>, // (0040,0002)
    pub procedure_step_start_time: Option<String>, // (0040,0003)
    pub patient_name: Option<String>, // (0010,0010)
    pub patient_id: Option<String>, // (0010,0020)
    pub accession_number: Option<String>, // (0008,0050)
    pub requesting_physician: Option<String>, // (0032,1032)
    pub sps_priority: Option<String>, // (0074,1200) ScheduledProcedureStepPriority
    /// (0008,0060) Modality - a VENDOR EXTENSION, not a UPS attribute.
    ///
    /// PS3.4 Table CC.2.5-3 (the UPS IOD) defines no Modality row, so a
    /// standard UPS workitem cannot carry one. It is rendered anyway because
    /// the DIMSE MWL service requires Modality as a Return key Type 1
    /// (PS3.4 Table K.6-1), which is what the bridge maps this workitem onto.
    ///
    /// The value is the input study's Modality, known when the workitem is
    /// created by a STOW. `None` means no honest value exists - an externally
    /// created workitem, or a study that declared no modality - and the wire
    /// shape then omits (0008,0060) rather than inventing one.
    pub modality: Option<String>,
    pub transaction_uid: Option<String>, // (0008,1195) Transaction UID for optimistic locking (G-08)
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// (0074,1204) Procedure Step Label - type 1 at N-CREATE.
    pub step_label: Option<String>,
    /// (0040,4041) Input Readiness State - type 1 at N-CREATE.
    pub input_readiness: Option<String>,
    /// (0040,4005) Scheduled Procedure Step Start DateTime - type 1 at
    /// N-CREATE; VR is DT, so it is one value and not a DA/TM pair.
    pub sps_start_datetime: Option<String>,
    /// The Study Instance UID of the input study (the first item's
    /// (0020,000D) inside (0040,4021) Input Information Sequence).
    pub input_study_uid: Option<String>,
    /// (0074,1238) Reason For Cancellation.
    pub reason_for_cancellation: Option<String>,
    /// (0040,4052) Procedure Step Cancellation DateTime.
    pub cancellation_datetime: Option<String>,
    /// (0010,0021) Issuer of Patient ID. `None` means the site declares no
    /// issuer, which the wire shape renders as a zero-length element.
    pub patient_id_issuer: Option<String>,
    /// (0040,4018) Scheduled Workitem Code Sequence, one item.
    pub scheduled_workitem_code: Option<WorkItemCode>,
    /// (0040,4033) Output Information Sequence - the objects the performer
    /// produced, each item a reference pair.
    ///
    /// `None` means the sequence was never written; `Some(vec![])` means it
    /// was written and is empty. Those are different statements and the
    /// update handler keeps them apart (PS3.4 Table CC.2.5-3).
    pub output_information: Option<Vec<OutputReference>>,
    /// (0040,4019) Performed Workitem Code Sequence, one item.
    pub performed_workitem_code: Option<WorkItemCode>,
    /// (0040,4028) Performed Station Name Code Sequence, one item.
    pub performed_station_name_code: Option<WorkItemCode>,
    /// (0040,A370) Referenced Request Sequence > (0040,1001) Requested
    /// Procedure ID - a REQUIRED matching key (PS3.4 Table CC.2.5-3, R).
    ///
    /// The sequence segment lives in the parameter NAME and in the tagging of
    /// the render; the model carries the one scalar.
    pub requested_procedure_id: Option<String>,
    /// (0040,4050) Performed Procedure Step Start DateTime - when the
    /// performer began, as it reports it.
    ///
    /// PS3.3 Table C.30.3-1 places this inside (0074,1216) Unified Procedure
    /// Step Performed Procedure Sequence, so it is written and read as a child
    /// of that item and never as a bare top-level attribute. `None` means no
    /// performer has reported a start.
    pub performed_start_datetime: Option<String>,
    /// (0040,4051) Performed Procedure Step End DateTime - when the performer
    /// finished; the same item and the same discipline as the start above.
    pub performed_end_datetime: Option<String>,
    /// (0074,1210) Scheduled Processing Parameters Sequence, as opaque JSON.
    ///
    /// The element belongs to the performing system (PS3.3 Table C.30.2-1:
    /// its items are Content Items whose Concept Names the server has no
    /// vocabulary for), so it is carried rather than modelled.
    pub scheduled_processing_parameters: Option<String>,
    /// (0040,1002) Reason for the Requested Procedure, nested in
    /// (0040,A370) - the one attribute of that sequence an N-SET may change
    /// (PS3.4 Table CC.2.5-3).
    pub reason_for_requested_procedure: Option<String>,
}

/// One item of (0040,4033) Output Information Sequence: the reference pair
/// PS3.3 C.12.1 defines for pointing at another SOP instance.
///
/// The Referenced Instances and Access Macro (PS3.3 Table 10-3b) also types
/// Series Instance UID as 1C for an object with a Series IE, so the series is
/// carried when the performer names it: without it a consumer cannot build the
/// direct retrieve URL and a viewer renders the item as "series not named by
/// the performer".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputReference {
    /// (0008,1150) Referenced SOP Class UID.
    pub sop_class_uid: String,
    /// (0020,000E) Series Instance UID, empty when the performer named none.
    pub series_instance_uid: String,
    /// (0008,1155) Referenced SOP Instance UID.
    pub sop_instance_uid: String,
}

/// One code from a `CodeSequenceMacro` (PS3.3 Table 8.8-1): the Code Value,
/// its Coding Scheme Designator, and the Code Meaning an operator reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkItemCode {
    pub value: String,
    pub scheme: String,
    pub meaning: String,
}
