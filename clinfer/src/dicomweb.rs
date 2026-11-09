// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOMweb operations, in the standard's own vocabulary.
//!
//! Each function is one request or a small sequence of them, and each
//! returns either a parsed result or the origin's own status. Nothing here
//! decides policy: whether a 409 means "someone else claimed it" or "the
//! origin is broken" is the caller's reading, and the status is preserved
//! so it can be.
//!
//! The DICOM JSON attribute keys are the standard's tags, written once as
//! constants so a typo cannot become a silently unrecognised field.

use crate::http::{Client, HttpError, Method, Request};
use crate::json::{self, Json};

/// The WADL media type of a Capabilities Description (PS3.18 8.9.3.1.1).
///
/// Annex H fixes WADL as the representation of the Retrieve Capabilities
/// response, and 8.9.3.1.1 names this type for it.
pub const WADL: &str = "application/vnd.sun.wadl+xml";

/// (0008,0060) Modality.
pub const TAG_MODALITY: &str = "00080060";
/// (0008,0061) ModalitiesInStudy.
///
/// The STUDY-level matching attribute for modality (PS3.18 Table 10.6.1-5):
/// a study-level row may carry several modalities, and (0008,0060) is
/// defined at the SERIES level. Multi-valued, so the reader takes the whole
/// `Value` array.
pub const TAG_MODALITIES_IN_STUDY: &str = "00080061";
/// (0008,0050) AccessionNumber.
pub const TAG_ACCESSION_NUMBER: &str = "00080050";
/// (0020,0010) StudyID.
pub const TAG_STUDY_ID: &str = "00200010";
/// (0010,0010) PatientName.
pub const TAG_PATIENT_NAME: &str = "00100010";
/// (0010,0020) PatientID.
pub const TAG_PATIENT_ID: &str = "00100020";
/// (0010,0021) IssuerOfPatientID.
pub const TAG_PATIENT_ID_ISSUER: &str = "00100021";
/// (0010,0030) PatientBirthDate.
pub const TAG_PATIENT_BIRTH_DATE: &str = "00100030";
/// (0010,0040) PatientSex.
pub const TAG_PATIENT_SEX: &str = "00100040";
/// (0008,0090) ReferringPhysicianName.
pub const TAG_REFERRING_PHYSICIAN: &str = "00080090";
/// (0020,000D) StudyInstanceUID.
pub const TAG_STUDY_INSTANCE_UID: &str = "0020000D";
/// (0020,000E) SeriesInstanceUID.
pub const TAG_SERIES_INSTANCE_UID: &str = "0020000E";
pub const TAG_SERIES_DESCRIPTION: &str = "0008103E";
/// (0008,0018) SOPInstanceUID, the instance's own identity.
pub const TAG_SOP_INSTANCE_UID: &str = "00080018";
/// (0008,1030) StudyDescription.
pub const TAG_STUDY_DESCRIPTION: &str = "00081030";
/// (0008,0104) CodeMeaning.
pub const TAG_CODE_MEANING: &str = "00080104";
/// (0018,0015) BodyPartExamined.
pub const TAG_BODY_PART: &str = "00180015";
/// (0008,1195) TransactionUID.
pub const TAG_TRANSACTION_UID: &str = "00081195";
/// (0040,4018) Scheduled Workitem Code Sequence - the code the Task
/// Requester chose for the step. AIW-I 4.84.4.1.2.1: the performed code
/// echoes it when it is present.
pub const TAG_SCHEDULED_WORKITEM_CODE_SEQUENCE: &str = "00404018";
/// (0040,4019) PerformedWorkitemCodeSequence - the code the PERFOMER says
/// it executed. Written on the update that closes the step.
pub const TAG_PERFORMED_WORKITEM_CODE_SEQUENCE: &str = "00404019";
/// (0040,4025) ScheduledStationNameCodeSequence - the assignment attribute
/// a claim compares its own AE Title against (AIW-I 4.82.4.1.1).
pub const TAG_SCHEDULED_STATION_NAME_CODE_SEQUENCE: &str = "00404025";
/// The station-oriented SEARCH key: the Code Value of (0040,4025), spelled as
/// the PS3.18 8.3.4 keyword path.
///
/// The station fact lives in ONE place on the wire - the Code Value of the
/// sequence above, set to the AE Title of the Requestor's UPS SCU (AIW-I
/// 4.81.4.1.2.1 Table 4.81.4.1.2.1-3). PS3.4 Table CC.2.5-3 gives
/// (0040,0001) Scheduled Station AE Title no row, so a UPS-RS origin refuses
/// that key rather than matching it, and the keyword path is the spelling both
/// this server and dcm4chee filter on.
pub const KEY_SCHEDULED_STATION_CODE_VALUE: &str = "ScheduledStationNameCodeSequence.CodeValue";
/// (0040,4026) ScheduledStationClassCodeSequence.
pub const TAG_SCHEDULED_STATION_CLASS_CODE_SEQUENCE: &str = "00404026";
/// (0040,4028) PerformedStationNameCodeSequence - the station that ran the
/// step (AIW-I 4.84.4.1.2.1).
pub const TAG_PERFORMED_STATION_NAME_CODE_SEQUENCE: &str = "00404028";
/// (0040,4036) HumanPerformerOrganization - the assignment attribute that
/// names the organization of a scheduled human performer rather than a
/// station.
pub const TAG_HUMAN_PERFORMER_ORGANIZATION: &str = "00404036";
/// (0040,1001) RequestedProcedureID.
pub const TAG_REQUESTED_PROCEDURE_ID: &str = "00401001";
/// (0040,1002) ReasonForTheRequestedProcedure.
pub const TAG_REASON_FOR_REQUESTED_PROCEDURE: &str = "00401002";
/// (0040,100A) ReasonForRequestedProcedureCodeSequence.
pub const TAG_REASON_FOR_REQUESTED_PROCEDURE_CODE_SEQUENCE: &str = "0040100A";
/// (0032,1060) RequestedProcedureDescription.
pub const TAG_REQUESTED_PROCEDURE_DESCRIPTION: &str = "00321060";
/// (0032,1064) RequestedProcedureCodeSequence.
pub const TAG_REQUESTED_PROCEDURE_CODE_SEQUENCE: &str = "00321064";
/// (0040,4041) InputReadinessState - `INCOMPLETE` forbids a claim
/// (AIW-I 4.82.4.1.1).
pub const TAG_INPUT_READINESS_STATE: &str = "00404041";
/// (0008,0100) CodeValue, inside a code sequence item.
pub const TAG_CODE_VALUE: &str = "00080100";
/// (0008,0102) CodingSchemeDesignator, inside a code sequence item.
pub const TAG_CODING_SCHEME_DESIGNATOR: &str = "00080102";
/// (0074,1000) UnifiedProcedureStepState.
pub const TAG_UPS_STATE: &str = "00741000";
/// (0040,4011) ExpectedCompletionDateTime, the standard's place for a
/// cancellation reason is (0074,1238) ReasonForCancellation.
pub const TAG_REASON_FOR_CANCELLATION: &str = "00741238";

/// (0074,1210) Scheduled Processing Parameters Sequence - the requester's
/// parameter list for the performing system (PS3.3 Table C.30.2-1).
///
/// Its items are Content Items: a Concept Name that says WHICH parameter, a
/// Value Type that says how to read it, and exactly one of the Text / Numeric
/// / Code value attributes. This sidecar does not interpret the meaning - it
/// flattens the list into a name/keyed map and hands it to the script.
pub const TAG_SCHEDULED_PROCESSING_PARAMETERS_SEQUENCE: &str = "00741210";
/// (0040,A043) ConceptNameCodeSequence - the parameter's name.
pub const TAG_CONCEPT_NAME_CODE_SEQUENCE: &str = "0040A043";
/// (0040,A040) ValueType - which value attribute carries the parameter.
pub const TAG_VALUE_TYPE: &str = "0040A040";
/// (0040,A160) TextValue.
pub const TAG_TEXT_VALUE: &str = "0040A160";
/// (0040,A30A) NumericValue, a DS whose FIRST component is the number.
pub const TAG_NUMERIC_VALUE: &str = "0040A30A";
/// (0040,A168) ConceptCodeSequence - the carriage of a CODE value.
///
/// PS3.3 Table 10-2: Value Type CODE selects this sequence, "Only a single
/// Item shall be included", and the item is a Code Sequence Macro. The value
/// is therefore NOT a bare (0008,0100) on the content item itself.
pub const TAG_CONCEPT_CODE_SEQUENCE: &str = "0040A168";
/// (0040,A120) DateTime, the carriage of a DATETIME value (PS3.3 Table 10-2).
pub const TAG_DATETIME_VALUE: &str = "0040A120";

/// (0040,4021) Input Information Sequence - the standard's only route from a
/// workitem to the study it operates on (PS3.4 CC.2.5-3).
pub const TAG_INPUT_INFORMATION_SEQUENCE: &str = "00404021";

/// (0074,1204) Procedure Step Label - a matching key a filtered
/// subscription can name (PS3.18 11.11). UPS has no Modality attribute,
/// so the label is what a subscriber can name for the STEP.
pub const TAG_STEP_LABEL: &str = "00741204";

/// (0040,4018) Scheduled Workitem Code Sequence, as a nested matching key.
///
/// PS3.18 8.3.4.1 names an attribute inside a sequence by joining the outer
/// and inner tags with a dot, so the Code Value of the scheduled code is
/// `00404018.00080100` and its Coding Scheme Designator is
/// `00404018.00080102`. Matched with PS3.4 C.2.2.2.6 Sequence Matching.
pub const TAG_SCHEDULED_WORKITEM_CODE_VALUE: &str = "00404018.00080100";
/// The same sequence's (0008,0102) Coding Scheme Designator.
pub const TAG_SCHEDULED_WORKITEM_CODE_SCHEME: &str = "00404018.00080102";

/// (0040,4033) OutputInformationSequence - what a completed workitem
/// produced (PS3.4 Table CC.2.5-3).
///
/// NOT (0040,4034): that tag is Scheduled Human Performers Sequence. A
/// reader looking for the outputs reads 4033, so writing them under 4034
/// puts the list where nothing can find it.
pub const TAG_OUTPUT_INFORMATION_SEQUENCE: &str = "00404033";
/// (0008,1150) Referenced SOP Class UID, inside an output item.
pub const TAG_REFERENCED_SOP_CLASS_UID: &str = "00081150";
/// (0008,1155) Referenced SOP Instance UID, inside an output item.
pub const TAG_REFERENCED_SOP_INSTANCE_UID: &str = "00081155";
/// (0020,000E) Series Instance UID, inside an output item.
pub const TAG_REFERENCED_SERIES_INSTANCE_UID: &str = "0020000E";

/// One produced object, named by its real SOP Class UID, Series Instance UID
/// and SOP Instance UID.
///
/// The class is what the object carries in (0008,0016) - a Secondary Capture
/// for a derived image, a Segmentation for a SEG, and so on - so the reference
/// in (0040,4033) names the class a consumer will actually read. A single
/// assumed class (GSPS) made the reference disagree with every object that was
/// not of that class.
///
/// The series UID is named too because the Referenced Instances and Access
/// Macro types Series Instance UID as 1C for DICOM objects with a Series IE
/// (PS3.3 Table 10-3b): without it a consumer cannot build the direct
/// retrieve URL `/studies/{study}/series/{series}/instances/{sop}` and Weasis
/// renders the item as "series not named by the performer".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRef {
    /// (0008,1150) the class the referenced object carries in (0008,0016).
    pub sop_class_uid: String,
    /// (0020,000E) the series the referenced object belongs to.
    pub series_instance_uid: String,
    /// (0008,1155) the referenced object's own SOP Instance UID.
    pub sop_instance_uid: String,
}

impl OutputRef {
    /// A produced object, its class and its series given separately.
    #[must_use]
    pub fn new(
        sop_class_uid: impl Into<String>,
        series_instance_uid: impl Into<String>,
        sop_instance_uid: impl Into<String>,
    ) -> Self {
        Self {
            sop_class_uid: sop_class_uid.into(),
            series_instance_uid: series_instance_uid.into(),
            sop_instance_uid: sop_instance_uid.into(),
        }
    }
}
/// (0074,1216) Unified Procedure Step Performed Procedure Sequence.
///
/// The PARENT of the whole performed report: PS3.3 Table C.30.3-1 nests
/// (0040,4019), (0040,4028), (0040,4033) and the two datetimes inside it and
/// places none of them at the top level of the workitem.
pub const TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE: &str = "00741216";
/// (0040,4050) Performed Procedure Step Start DateTime.
pub const TAG_PERFORMED_START_DATETIME: &str = "00404050";
/// (0040,4051) Performed Procedure Step End DateTime.
pub const TAG_PERFORMED_END_DATETIME: &str = "00404051";

/// The Worklist Subscription SOP Class UID (PS3.18 11.10).
pub const WORKLIST_SUBSCRIPTION_UID: &str = "1.2.840.10008.5.1.4.34.5";
/// The Filtered Worklist Subscription SOP Class UID (PS3.18 11.11).
///
/// A separate resource, not a parameter switch on the one above: the `filter`
/// query parameter belongs here (Table 11.10.1-2).
pub const FILTERED_WORKLIST_SUBSCRIPTION_UID: &str = "1.2.840.10008.5.1.4.34.5.1";
/// Which subscription resource a request belongs on.
///
/// PS3.18 Table 11.10.1-2 ties the `filter` query parameter to the Filtered
/// Worklist Subscription resource, so the two forms are separate resources
/// rather than one resource with a flag. A server is entitled to refuse
/// `filter` on the plain resource, and one that merely ignores it turns the
/// filter into a silent no-op - the performer then receives every workitem's
/// events, which is the defect this function exists to prevent.
fn subscription_resource(filter: Option<&WorklistFilter>) -> &'static str {
    if filter.is_some() {
        FILTERED_WORKLIST_SUBSCRIPTION_UID
    } else {
        WORKLIST_SUBSCRIPTION_UID
    }
}

/// A workitem as the sidecar needs it: the UID, the state, and the study
/// attributes a rule matches on.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkItem {
    pub uid: String,
    pub state: String,
    pub study_instance_uid: Option<String>,
    pub modality: String,
    pub study_description: String,
    pub body_part: String,
    /// (0040,4041) InputReadinessState, verbatim. `INCOMPLETE` is the one
    /// value that forbids a claim (AIW-I 4.82.4.1.1).
    pub input_readiness: Option<String>,
    /// Code Values of the sequences that ASSIGN a workitem to a performer:
    /// (0040,4025) Scheduled Station Name (the AE Title of the station),
    /// (0040,4036) Human Performer and (0040,4026) Scheduled Performing
    /// Physician Identification. One entry per populated sequence, in that
    /// order; an empty vector means the workitem names nobody, which
    /// AIW-I reads as "offered to whoever can do it".
    pub performer_code_values: Vec<String>,
    /// (0040,4018) Scheduled Workitem Code Sequence, first item's Code
    /// Value / Coding Scheme Designator / Code Meaning. Carried so the
    /// update can echo it as (0040,4019) per AIW-I 4.84.4.1.2.1.
    pub scheduled_workitem_code: Option<WorkitemCode>,
    /// (0074,1210) Scheduled Processing Parameters Sequence, flattened into
    /// the requester's parameters for this step. An empty vector means the
    /// workitem carries none, which is the state a script reads as "use your
    /// own defaults".
    pub scheduled_processing_parameters: Vec<Parameter>,
    /// The identity attributes AIW-I Appendix V Table V-1 types as
    /// "Source Copy" from the UPS Workitem into the result objects, as the
    /// workitem carries them. `None` means the workitem states nothing
    /// about that attribute, which is the signal for the writer to fall
    /// back on the study the workitem operates on.
    pub identity: WorkitemIdentity,
}

/// The Appendix V identity of a workitem, field per "Source Copy" row.
///
/// Appendix V Table V-1 lists these as values the Workitem Performer copies
/// UNALTERED from the UPS Workitem into the objects it produces; the
/// accompanying text adds that where the workitem does not carry one, the
/// implementation may take the value from an instance in the Input
/// Information Sequence instead. That "may" is exactly the source-study
/// fallback the writers had before this existed, so the two layers compose:
/// the workitem is asked first, the study second.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkitemIdentity {
    /// (0010,0010) Patient's Name.
    pub patient_name: Option<String>,
    /// (0010,0020) Patient ID.
    pub patient_id: Option<String>,
    /// (0010,0021) Issuer of Patient ID, part of the "Issuer of Patient ID
    /// Macro" row.
    pub patient_id_issuer: Option<String>,
    /// (0010,0030) Patient's Birth Date.
    pub patient_birth_date: Option<String>,
    /// (0010,0040) Patient's Sex.
    pub patient_sex: Option<String>,
    /// (0020,0010) Study ID.
    pub study_id: Option<String>,
    /// (0008,0050) Accession Number.
    pub accession_number: Option<String>,
    /// (0008,0090) Referring Physician's Name. Not an Appendix V row, but
    /// the same "Source Copy" shape and already written by the SR and PR
    /// writers from the study.
    pub referring_physician: Option<String>,
    /// (0040,4028) Performed Station Name Code Sequence, first item's Code
    /// Meaning. Appendix V Note 4 maps it into Station Name (0008,1010);
    /// the SEQUENCE (not the key) is what carries the meaning, so the
    /// naming code is what is read here.
    ///
    /// `None` when the workitem carries no such sequence: the statement
    /// promises a mapping FROM (0040,4028), so there is no fallback and the
    /// writer omits Station Name rather than inventing one.
    pub performed_station_name: Option<String>,
    /// (0040,4018) Scheduled Workitem Code Sequence, one item. Appendix V
    /// Table V-1 maps it into the result's Scheduled Protocol Code Sequence
    /// (0040,0008), which is why the raw code is carried here rather than
    /// only the (0040,4019) echo the update sends.
    pub scheduled_workitem_code: Option<WorkitemCode>,
    /// (0040,1001) Requested Procedure ID.
    pub requested_procedure_id: Option<String>,
    /// (0032,1060) Requested Procedure Description.
    pub requested_procedure_description: Option<String>,
    /// (0032,1064) Requested Procedure Code Sequence, one item.
    pub requested_procedure_code: Option<WorkitemCode>,
    /// (0040,1002) Reason for the Requested Procedure.
    pub reason_for_requested_procedure: Option<String>,
    /// (0040,100A) Reason for Requested Procedure Code Sequence, one item.
    pub reason_for_requested_procedure_code: Option<WorkitemCode>,
}

/// A code as AIW-I uses it: Code Value, Coding Scheme Designator and the
/// human-readable meaning (PS3.3 Table 8.8-1 - `CodeSequenceMacro`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkitemCode {
    pub value: String,
    pub scheme: String,
    pub meaning: String,
}

/// The value of one scheduled processing parameter, as PS3.3 Table C.30.2-1
/// types it.
///
/// The three variants are the Value Types this sidecar can hand to a script
/// without losing what the requester stated. Any other Value Type
/// (PNAME, DATE, TIME, UIDREF, ...) is reported on stderr and SKIPPED rather
/// than coerced: a parameter the sidecar guesses at is worse than one the
/// script never sees.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    /// `TEXT` - (0040,A160) Text Value, verbatim.
    Text(String),
    /// `NUMERIC` - (0040,A30A) Numeric Value. A DS may carry a multi-valued
    /// list (PS3.5 6.2); the first component is the parameter's value.
    Numeric(f64),
    /// `CODE` - (0040,A168) Concept Code Sequence, single item: its Code
    /// Value and Coding Scheme Designator name the code (PS3.3 Table 10-2).
    Code { scheme: String, value: String },
    /// `DATETIME` - (0040,A120) DateTime, verbatim.
    ///
    /// Stored as the string the requester wrote rather than as a parsed
    /// instant: DT carries a precision and an optional range, and a script
    /// that wants a moment can parse it with its own rules. Reformatting here
    /// would discard exactly the part a caller may care about.
    DateTime(String),
}

/// One scheduled processing parameter: the Concept Name's Code Value and the
/// value.
///
/// The name is the FLAT key a script reads - it is the Concept Name Code
/// Value, not the Code Meaning, because a Code Value is a stable machine key
/// while the meaning is display text a site may translate. The Code Meaning
/// is deliberately NOT carried: it is requester-supplied free text that
/// nothing reads and nothing writes, and holding it in memory would grow the
/// sidecar's PHI surface for no output.
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: String,
    pub value: ParamValue,
}

impl WorkitemCode {
    /// Read a `CodeSequenceMacro` item from a sequence's first item.
    ///
    /// A code with no Code Value is not a code: AIW-I matching is on the
    /// value, so an item that carries only a meaning is dropped rather than
    /// echoed as a half-code.
    #[must_use]
    pub fn from_item(item: &Json) -> Option<Self> {
        let value = item.attribute(TAG_CODE_VALUE)?.trim().to_string();
        if value.is_empty() {
            return None;
        }
        Some(Self {
            scheme: item.attribute(TAG_CODING_SCHEME_DESIGNATOR)?.to_string(),
            meaning: item.attribute(TAG_CODE_MEANING).unwrap_or("").to_string(),
            value,
        })
    }
}

/// The Code Value of an assignment sequence's first item, when present.
fn assignment_code_value(value: &Json, tag: &str) -> Option<String> {
    let item = value.attribute_item(tag)?;
    let code_value = item.attribute(TAG_CODE_VALUE)?.trim();
    (!code_value.is_empty()).then(|| code_value.to_string())
}

impl WorkItem {
    /// Read the fields clinfer uses out of a DICOM JSON workitem.
    ///
    /// Identity is (0008,0018) SOP Instance UID - the workitem's own UID, and
    /// the C-FIND unique return key (PS3.4 CC.2.5-3). The study the workitem
    /// operates on is not a top-level attribute at all: it is reachable only
    /// through (0040,4021) Input Information Sequence, so it is read from the
    /// first item's (0020,000D).
    #[must_use]
    pub fn from_json(value: &Json) -> Option<Self> {
        let uid = value.attribute(TAG_SOP_INSTANCE_UID)?.to_string();
        Some(Self {
            uid,
            state: value
                .attribute(TAG_UPS_STATE)
                .unwrap_or("SCHEDULED")
                .to_string(),
            study_instance_uid: input_study_uid(value),
            modality: value.attribute(TAG_MODALITY).unwrap_or("").to_string(),
            study_description: value
                .attribute(TAG_STUDY_DESCRIPTION)
                .unwrap_or("")
                .to_string(),
            body_part: value.attribute(TAG_BODY_PART).unwrap_or("").to_string(),
            input_readiness: value
                .attribute(TAG_INPUT_READINESS_STATE)
                .map(str::to_string),
            performer_code_values: performer_code_values(value),
            scheduled_workitem_code: value
                .attribute_item(TAG_SCHEDULED_WORKITEM_CODE_SEQUENCE)
                .and_then(WorkitemCode::from_item),
            scheduled_processing_parameters: scheduled_processing_parameters(value),
            identity: WorkitemIdentity::from_json(value),
        })
    }
}

impl WorkitemIdentity {
    /// Read the Appendix V identity a workitem states.
    ///
    /// Every field is read with `attribute`, which takes `Value[0]` and
    /// returns `None` for an absent element and `Some("")` for a
    /// zero-length one. That distinction is kept rather than flattened:
    /// Appendix V copies values UNALTERED, so a workitem that says
    /// "present, but empty" is stating something a fallback must not
    /// overwrite.
    #[must_use]
    pub fn from_json(value: &Json) -> Self {
        Self {
            patient_name: value
                .attribute_pn(TAG_PATIENT_NAME)
                .map(str::to_string)
                .or_else(|| value.attribute(TAG_PATIENT_NAME).map(str::to_string)),
            patient_id: value.attribute(TAG_PATIENT_ID).map(str::to_string),
            patient_id_issuer: value.attribute(TAG_PATIENT_ID_ISSUER).map(str::to_string),
            patient_birth_date: value.attribute(TAG_PATIENT_BIRTH_DATE).map(str::to_string),
            patient_sex: value.attribute(TAG_PATIENT_SEX).map(str::to_string),
            study_id: value.attribute(TAG_STUDY_ID).map(str::to_string),
            accession_number: value.attribute(TAG_ACCESSION_NUMBER).map(str::to_string),
            referring_physician: value
                .attribute_pn(TAG_REFERRING_PHYSICIAN)
                .map(str::to_string)
                .or_else(|| value.attribute(TAG_REFERRING_PHYSICIAN).map(str::to_string)),
            performed_station_name: value
                .attribute_item(TAG_PERFORMED_STATION_NAME_CODE_SEQUENCE)
                .and_then(|item| item.attribute(TAG_CODE_MEANING))
                .map(str::to_string),
            // The same sequence the (0040,4019) echo reads: one parse, two
            // consumers (the update's echo and the result's (0040,0008)).
            scheduled_workitem_code: value
                .attribute_item(TAG_SCHEDULED_WORKITEM_CODE_SEQUENCE)
                .and_then(WorkitemCode::from_item),
            requested_procedure_id: value
                .attribute(TAG_REQUESTED_PROCEDURE_ID)
                .map(str::to_string),
            requested_procedure_description: value
                .attribute(TAG_REQUESTED_PROCEDURE_DESCRIPTION)
                .map(str::to_string),
            requested_procedure_code: value
                .attribute_item(TAG_REQUESTED_PROCEDURE_CODE_SEQUENCE)
                .and_then(WorkitemCode::from_item),
            reason_for_requested_procedure: value
                .attribute(TAG_REASON_FOR_REQUESTED_PROCEDURE)
                .map(str::to_string),
            reason_for_requested_procedure_code: value
                .attribute_item(TAG_REASON_FOR_REQUESTED_PROCEDURE_CODE_SEQUENCE)
                .and_then(WorkitemCode::from_item),
        }
    }
}

/// Every assignment Code Value the workitem carries, in sequence order.
///
/// The three attributes name who is scheduled to perform through different
/// doors: the scheduled station by name and by class, and the scheduled
/// human performer by organization. AIW-I 4.82.4.1.1 forbids a performer to
/// claim when any of them names someone else, so all three are read rather
/// than the first that happens to be populated.
#[must_use]
pub fn performer_code_values(value: &Json) -> Vec<String> {
    [
        TAG_SCHEDULED_STATION_NAME_CODE_SEQUENCE,
        TAG_HUMAN_PERFORMER_ORGANIZATION,
        TAG_SCHEDULED_STATION_CLASS_CODE_SEQUENCE,
    ]
    .into_iter()
    .filter_map(|tag| assignment_code_value(value, tag))
    .collect()
}

/// The input study UID: the first item's (0020,000D) inside (0040,4021).
///
/// Returns `None` when the workitem names no study, which is the caller's
/// signal to cancel rather than to look up a study that was never named.
#[must_use]
pub fn input_study_uid(value: &Json) -> Option<String> {
    let first = value.attribute_item(TAG_INPUT_INFORMATION_SEQUENCE)?;
    first.attribute(TAG_STUDY_INSTANCE_UID).map(str::to_string)
}

/// What a call returned when it did not succeed. The status is kept so
/// the caller can tell a conflict from a broken origin.
#[derive(Debug)]
pub struct StatusError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.body.trim())
    }
}

impl std::error::Error for StatusError {}

/// A DICOMweb failure: the transport, or a status the call did not accept.
#[derive(Debug)]
pub enum Error {
    Http(HttpError),
    Status(StatusError),
    /// The body arrived but is not the JSON the operation expects.
    Json(json::JsonError),
    /// The response arrived with a status the call accepts, but carries
    /// something the operation cannot use - a retrieve part labelled with a
    /// transfer syntax the reader cannot decode, for instance. The message
    /// names the condition, because the origin did nothing wrong and the
    /// status code says nothing about it.
    Refused(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(err) => write!(f, "{err}"),
            Self::Status(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::Refused(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<HttpError> for Error {
    fn from(err: HttpError) -> Self {
        Self::Http(err)
    }
}

impl From<json::JsonError> for Error {
    fn from(err: json::JsonError) -> Self {
        Self::Json(err)
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::Refused(message)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The DICOMweb surface of one origin.
#[derive(Debug)]
pub struct Dicomweb {
    client: Client,
    /// The AE title sent as the optional `requester` query parameter on a
    /// Change State request (PS3.18 11.7 Table 11.7.2.1-1: `requester =
    /// aetitle, O`). Empty means "do not send it" - which keeps a bare
    /// `new(client)` origin byte-compatible with the pre-requester wire.
    requester: String,
}

impl Dicomweb {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            requester: String::new(),
        }
    }

    /// Set the Change State `requester` AE title (see [`Self::requester`]).
    #[must_use]
    pub fn with_requester(mut self, requester: impl Into<String>) -> Self {
        self.requester = requester.into();
        self
    }

    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// `GET /dicomweb/` - the origin's capabilities document.
    ///
    /// Sent as `Accept: application/vnd.sun.wadl+xml`. The Retrieve
    /// Capabilities Transaction (PS3.18 8.9) returns a Capabilities
    /// Description, which Annex H defines as a WADL document, and the WADL
    /// media type is the one the standard fixes for it (there is no
    /// DICOM-specific type in PS3.18 8.7). The body is read as the WADL
    /// resource tree; an origin answering something else is a read failure
    /// the caller only warns about.
    pub fn capabilities(&self) -> Result<Wadl> {
        let request = Request::get(self.client.url("/")).accept(WADL);
        let response = self.client.send(&request)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        parse_wadl(&response.body)
    }

    /// `GET /dicomweb/workitems` - UPS Search, all workitems.
    ///
    /// Returns the workitem UIDs in the order the origin sent them. Only
    /// the UID is read: the catch-up path claims each one, and the claim
    /// response carries the attributes.
    pub fn search_workitems(&self) -> Result<Vec<String>> {
        let request = Request::get(self.client.url("/workitems")).accept(crate::http::DICOM_JSON);
        let response = self.client.send(&request)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        let document = json::parse(&response.body)?;
        let items = document.as_array().ok_or_else(|| {
            Error::Status(StatusError {
                status: response.status,
                body: "UPS Search answered a non-array; PS3.18 F.2.1 makes the \
                       result of a search a top-level JSON array"
                    .to_string(),
            })
        })?;
        Ok(items
            .iter()
            .filter_map(|item| item.attribute(TAG_SOP_INSTANCE_UID).map(str::to_string))
            .collect())
    }

    /// `GET /dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue={ae}`
    /// - UPS Search, station-oriented.
    ///
    /// AIW-I 4.81.4.1.2.2 calls the station-oriented search the minimum a
    /// Task Performer shall issue. The station carrier of PS3.4 Table
    /// CC.2.5-3 is (0040,4025) Scheduled Station Name Code Sequence, whose
    /// Code Value, if valued, shall be set to the AE Title of the Requestor's
    /// UPS SCU (AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3) - so that Code Value
    /// IS the station this query asks about, and the key that names it is the
    /// sequence-qualified Code Value of PS3.18 8.3.4.
    ///
    /// The key is EXACT, not wildcarded: a wildcard would ask the origin to
    /// match "any station whose title contains ours", a different question
    /// whose answer is a superset.
    pub fn search_workitems_for_station(&self, station_aet: &str) -> Result<Vec<String>> {
        let path = format!(
            "/workitems?{KEY_SCHEDULED_STATION_CODE_VALUE}={}",
            form_encode(station_aet)
        );
        let request = Request::get(self.client.url(&path)).accept(crate::http::DICOM_JSON);
        let response = self.client.send(&request)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        let document = json::parse(&response.body)?;
        let items = document.as_array().ok_or_else(|| {
            Error::Status(StatusError {
                status: response.status,
                body: "UPS Search answered a non-array; PS3.18 F.2.1 makes the \
                       result of a search a top-level JSON array"
                    .to_string(),
            })
        })?;
        Ok(items
            .iter()
            .filter_map(|item| item.attribute(TAG_SOP_INSTANCE_UID).map(str::to_string))
            .collect())
    }

    /// `GET /dicomweb/workitems/{uid}` - retrieve one workitem.
    pub fn get_workitem(&self, uid: &str) -> Result<WorkItem> {
        let request = Request::get(self.client.url(&format!("/workitems/{uid}")))
            .accept(crate::http::DICOM_JSON);
        let response = self.client.send(&request)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        let document = json::parse(&response.body)?;
        // A single-retrieve answer is one Data Set (PS3.18 F.2.2), and the
        // two conformant spellings of it differ in the field: Clarus sends
        // the bare object, dcm4chee-arc sends an array of one. Reading only
        // the object form left `attribute("00080018")` returning None on the
        // array form, which surfaced as "carries no StudyInstanceUID" under
        // an HTTP 200.
        let dataset = json::single_dataset(&document).ok_or_else(|| {
            Error::Status(StatusError {
                status: response.status,
                body: format!("workitem {uid} carries no StudyInstanceUID"),
            })
        })?;
        WorkItem::from_json(dataset).ok_or_else(|| {
            Error::Status(StatusError {
                status: response.status,
                body: format!("workitem {uid} carries no StudyInstanceUID"),
            })
        })
    }

    /// Update a workitem's attributes: `POST /workitems/{uid}` with the
    /// Transaction UID in the payload.
    ///
    /// PS3.18 11.6.1: Update is a POST on the workitem resource whose payload
    /// is the attribute set to modify; 11.6.1.4 requires that payload to
    /// comply with PS3.4 CC.2.6.2, which for an IN PROGRESS workitem makes the
    /// Transaction UID (0008,1195) an Attribute OF the data set. PS3.18
    /// 11.6.1.2 additionally allows it as a query parameter, and this server
    /// accepts both - but the payload form is what dcm4chee-arc reads, so it
    /// is the interoperable one and the one used here.
    ///
    /// This is the FIRST of the two requests a conformant completion sends
    /// (AIW-I 4.84.4.1.2.1: "all created instances listed in Output
    /// Information Sequence before the completed transition"). The second is
    /// [`Self::update_state`], which changes the state and nothing else.
    /// Splitting them is what puts the output list where a consumer reads it:
    /// the change-state transaction carries only the state and the locking
    /// UID (11.7.1.4), so an output list sent there is discarded.
    ///
    /// `outputs` names the objects this cycle stored, each as
    /// `(sop_class_uid, sop_instance_uid)`: the class is the object's own
    /// (0008,0016), so the reference in (0040,4033) names the class a
    /// consumer will actually read. An EMPTY list is not sent: 11.6 would
    /// replace (0040,4033) with an empty sequence, which claims "produced
    /// nothing" for a step that is about to complete. A caller with nothing
    /// to declare omits the request entirely.
    ///
    /// `performed` carries what the performer says about itself on this same
    /// update (4.84.4.1.2.1): its station code and the echo of the scheduled
    /// workitem code.
    pub fn update_attributes(
        &self,
        uid: &str,
        transaction_uid: &str,
        outputs: &[OutputRef],
        performed: Option<&PerformedReport>,
    ) -> Result<()> {
        if outputs.is_empty() && performed.is_none() {
            // Nothing to declare. Sending an empty payload would be refused
            // (the server requires at least one updatable attribute), and
            // there is no attribute to send.
            return Ok(());
        }
        // The Transaction UID is the FIRST attribute of the payload: it is
        // the locking Attribute CC.2.6.2 requires in the data set, and it
        // travels with the merge attributes in one request.
        let locking = [(TAG_TRANSACTION_UID.to_string(), transaction_uid.to_string())];
        // The performed report and the outputs are ONE (0074,1216) item, so a
        // caller that has outputs but no report has no standard place to put
        // them: (0040,4033) is a child of that sequence (PS3.3 Table
        // C.30.3-1), not a top-level attribute, and emitting it top-level
        // would be the very shape a standard origin discards. The locking UID
        // still has to travel, so the request is sent with it alone and the
        // outputs are reported as undeliverable rather than dropped in
        // silence.
        let body = match performed {
            Some(performed) => encode_performed_report(&locking, outputs, performed),
            None => encode_attributes(&locking),
        };
        // No query parameter: the locking UID is in the payload, per
        // CC.2.6.2, which is the carriage dcm4chee-arc reads.
        let request = Request::json(
            Method::Post,
            self.client.url(&format!("/workitems/{uid}")),
            body,
        );
        let response = self.client.send_retrying(&request, &mut 0)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        Ok(())
    }

    /// Change a workitem's state: `PUT /dicomweb/workitems/{uid}/state`.
    ///
    /// PS3.18 11.7 gives the change-state transaction its own resource; the
    /// bare workitem PUT is a non-standard alias this server also accepts.
    /// Sending to the standard resource is what makes the sidecar work
    /// against any conformant origin.
    ///
    /// The body carries the state and the Transaction UID - both type 1 in a
    /// Change UPS State request (PS3.4 CC.2.1-1) - plus the reason when the
    /// transition is a cancel.
    pub fn update_state(
        &self,
        uid: &str,
        state: &str,
        transaction_uid: Option<&str>,
        reason: Option<&str>,
    ) -> Result<()> {
        self.update_state_with_output(uid, state, transaction_uid, reason, &[])
    }

    /// The same transition, naming the objects it produced.
    ///
    /// `outputs` names each object this cycle stored as an [`OutputRef`] (its
    /// class is the object's own (0008,0016), its series the one it belongs
    /// to). They go into (0040,4033) OutputInformationSequence, which
    /// is where a COMPLETED workitem says what it produced (PS3.4 Table
    /// CC.2.5-3): the downstream consumer that wants the result reads it from
    /// there, so a result object the workitem does not name is one it cannot
    /// find, and one named with the wrong class is one it cannot open.
    ///
    /// An empty list omits the sequence entirely rather than sending an
    /// empty one: the attribute is optional and an empty sequence would
    /// claim "produced nothing" for a transition that is not about outputs.
    ///
    /// `performed` carries what the PERFORMER says about itself on this
    /// update (AIW-I 4.84.4.1.2.1): the performed station name code and the
    /// echo of the scheduled workitem code. `None` omits both, which is the
    /// right shape for a transition that is not the performer's own report
    /// of work done - a cancel, or the claim itself.
    pub fn update_state_with_output(
        &self,
        uid: &str,
        state: &str,
        transaction_uid: Option<&str>,
        reason: Option<&str>,
        outputs: &[OutputRef],
    ) -> Result<()> {
        self.update_state_with_report(uid, state, transaction_uid, reason, outputs, None)
    }

    /// The full update: the transition, its outputs, and the performer's own
    /// report (see [`PerformedReport`]).
    ///
    /// Kept as the CHANGE STATE half of the two-step completion. AIW-I
    /// 4.84.4.1.2.1 places the performed codes and the output list on the
    /// UPDATE transaction (PS3.18 11.6), which is a request of its own - see
    /// [`Self::update_attributes`]. A state change carries the state and the
    /// locking UID and nothing else, which is what 11.7.1.4 says it carries.
    ///
    /// When `outputs` or `performed` are given here they are still written,
    /// so a caller that wants the old single-request shape keeps working; the
    /// sidecar's completion path no longer uses that route (it sends the two
    /// requests the standard describes).
    pub fn update_state_with_report(
        &self,
        uid: &str,
        state: &str,
        transaction_uid: Option<&str>,
        reason: Option<&str>,
        outputs: &[OutputRef],
        performed: Option<&PerformedReport>,
    ) -> Result<()> {
        let mut fields: Vec<(String, String)> =
            vec![(TAG_UPS_STATE.to_string(), state.to_string())];
        if let Some(transaction_uid) = transaction_uid {
            fields.push((TAG_TRANSACTION_UID.to_string(), transaction_uid.to_string()));
        }
        if let Some(reason) = reason {
            fields.push((TAG_REASON_FOR_CANCELLATION.to_string(), reason.to_string()));
        }
        // The same nested (0074,1216) shape as the Update carries, through the
        // same encoder: a state with a report attached is the merged form of
        // the two requests, and a second encoder here would let the two shapes
        // drift apart again - which is the defect this whole change is about.
        let body = match performed {
            Some(performed) => encode_performed_report(&fields, outputs, performed),
            None => encode_attributes(&fields),
        };
        // PS3.18 11.7 Table 11.7.2.1-1: the Change State request carries an
        // OPTIONAL `requester = aetitle` query parameter. It is the Requesting
        // AE (0074,1236) of the event report the transition originates; some
        // origins (dcm4chee-arc) require it and refuse the claim with 400 when
        // it is absent, so a conformant client sends the station's AE title.
        // An empty requester omits the parameter entirely, which is the same
        // optional-absent shape the pre-requester client sent.
        let path = if self.requester.is_empty() {
            format!("/workitems/{uid}/state")
        } else {
            format!(
                "/workitems/{uid}/state?requester={}",
                form_encode(&self.requester)
            )
        };
        let request = Request::json(Method::Put, self.client.url(&path), body);
        // Retried: a 503 here is the origin's transcode pool, not a refusal
        // of the transition. The claim's Transaction UID is ours, so
        // re-sending the same transition is safe. A 409 is an answer and is
        // returned as it is - only 503 is retried.
        let response = self.client.send_retrying(&request, &mut 0)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        Ok(())
    }

    /// `POST .../subscribers/{subscriber}` - Worklist Subscription.
    ///
    /// `endpoint` is the notification URI the origin would POST to. It is
    /// `None` for the channel-only form: the sidecar learns about work with
    /// its own WebSocket Notification Connection, so no reachable URI is
    /// required, and PS3.18 11.10 makes the Notification URI an OPTIONAL
    /// attribute of the subscription. `filter` selects the filtered form
    /// (PS3.18 11.11) when given; the unfiltered form (11.10) is used
    /// otherwise.
    pub fn subscribe(
        &self,
        subscriber: &str,
        endpoint: Option<&str>,
        filter: Option<&WorklistFilter>,
    ) -> Result<()> {
        let body = subscription_body(endpoint);
        // The filter is a QUERY parameter in the standard (PS3.18 11.10.1.1
        // names the Filtered Worklist Subscription resource by its URI
        // template, 8.3.4.1 gives the matching-key syntax, and 11.10.1.4
        // makes the payload "no payload"). The body form this used to send is
        // kept server-side for compatibility, but a conformant client sends
        // the query form.
        //
        // The filter belongs to the FILTERED resource (34.5.1), not to the
        // plain Worklist one (34.5): Table 11.10.1-2 ties the `filter`
        // parameter to that resource, and a server is entitled to refuse it
        // anywhere else. Sending it to 34.5 is what made the filter a no-op
        // against servers that ignore unknown parameters - the performer then
        // received every workitem's events.
        let mut path = format!(
            "/workitems/{}/subscribers/{subscriber}",
            subscription_resource(filter)
        );
        if let Some(filter) = filter {
            push_query(filter, &mut path);
        }
        let request = Request::json(Method::Post, self.client.url(&path), body);
        let response = self.client.send(&request)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        Ok(())
    }

    /// `GET /dicomweb/studies/{uid}/series` and `.../instances` for
    /// `--inspect`, and the study's attributes for rule matching.
    ///
    /// The attributes come from the SearchForStudies transaction
    /// (PS3.18 Table 10.6-1): `GET /studies?StudyInstanceUID={uid}` answers
    /// the small DICOM JSON array of matching studies. The retrieve form
    /// `GET /studies/{uid}` is NOT used: that path answers WADO-RS
    /// multipart - the whole study - regardless of the Accept header, which
    /// is both far more data than a rule match needs and beyond the
    /// sidecar's response cap. The caller reads the array shape.
    pub fn study(&self, uid: &str) -> Result<Json> {
        let path = format!("/studies?StudyInstanceUID={uid}");
        let request = Request::get(self.client.url(&path)).accept(crate::http::DICOM_JSON);
        let response = self.client.send(&request)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        Ok(json::parse(&response.body)?)
    }

    pub fn series(&self, study_uid: &str) -> Result<Json> {
        // The series UID, its modality, its description and its body part:
        // the worker decides per series which of the study's series belong
        // to the matched rule (owner 18.09), so the attributes it needs are
        // requested by name rather than assumed in the default set.
        // BodyPartExamined (0018,0015) is a SERIES-level attribute (PS3.3
        // General Series Module C.7.3.1), so it is requested here and not on
        // the study query, where an origin does not return it.
        let path = format!(
            "/studies/{study_uid}/series?includefield={TAG_SERIES_INSTANCE_UID},{TAG_MODALITY},{TAG_SERIES_DESCRIPTION},{TAG_BODY_PART}"
        );
        let request = Request::get(self.client.url(&path)).accept(crate::http::DICOM_JSON);
        let response = self.client.send(&request)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        Ok(json::parse(&response.body)?)
    }

    pub fn instances(&self, study_uid: &str, series_uid: &str) -> Result<Json> {
        let path = format!("/studies/{study_uid}/series/{series_uid}/instances");
        let request = Request::get(self.client.url(&path)).accept(crate::http::DICOM_JSON);
        let response = self.client.send(&request)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        Ok(json::parse(&response.body)?)
    }

    /// `GET /dicomweb/studies/{study}/series/{series}/instances/{uid}` -
    /// retrieve one instance as bytes, PS3.18 10.4.
    ///
    /// The accept header asks for explicit VR little endian, which is the
    /// one transfer syntax this engine's reader expects. Asking the origin
    /// to convert is the standard's own mechanism for it: PS3.18 8.7.3
    /// makes a transfer syntax parameter on the accept header a request
    /// the origin satisfies by transcoding, and it is the origin's job to
    /// know whether it can. A device that cannot convert answers with a
    /// different syntax, and the reader refuses it by name rather than
    /// misreading compressed bytes as samples.
    ///
    /// The response is a multipart body when the origin returns one and a
    /// bare body otherwise; both occur in the field, so both are taken.
    ///
    /// `attempts` reports how many requests were sent: one when the first
    /// succeeded, up to [`crate::http::MAX_ATTEMPTS`] when the origin was
    /// busy or the transport dropped the exchange. The fetch loop counts
    /// what it paid for, which is the only place that number is useful.
    pub fn instance(
        &self,
        study_uid: &str,
        series_uid: &str,
        instance_uid: &str,
        attempts: &mut u32,
    ) -> Result<Vec<u8>> {
        let path = format!("/studies/{study_uid}/series/{series_uid}/instances/{instance_uid}");
        let request = Request::get(self.client.url(&path)).accept(EXPLICIT_VR_LE_ACCEPT);
        // Retried: this is the call that races a busy transcode pool, and the
        // one a transient NAT or socket drop cuts short. The instance is
        // fetched whole by SOP Instance UID, so re-sending the same GET is
        // idempotent. It also reads under the instance budget: the answer is
        // a whole object with its pixel data inline, not a JSON page.
        let response = self.client.send_retrying_within(
            &request,
            attempts,
            crate::http::BodyBudget::INSTANCE,
        )?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        let part = first_part(response);
        // The part header is the origin's answer about what it sent. A part
        // that names another transfer syntax is a compressed instance, which
        // the reader cannot decode; refusing it here names the actual cause
        // instead of letting the reader fail over bytes it was told to read
        // as samples.
        if let Some(ref syntax) = part.transfer_syntax
            && syntax.as_str() != EXPLICIT_VR_LE
        {
            return Err(format!(
                "origin answered with transfer syntax {syntax}, not {EXPLICIT_VR_LE} \
                 (explicit VR little endian); the instance is compressed and cannot be read"
            )
            .into());
        }
        // No transfer-syntax parameter on the part (PS3.18 8.7.3 makes it
        // optional): the origin did not say how it encoded the instance, so
        // the dataset's own (0002,0010) is what decides. Reading it is the
        // difference between a compliant reader and one that silently
        // treats a compressed part as samples - the dataset is the
        // authority on its own encoding, and it is already in hand.
        if part.transfer_syntax.is_none() {
            match crate::pixels::transfer_syntax_uid(&part.body) {
                Some(uid) if uid == EXPLICIT_VR_LE.as_bytes() => {}
                Some(uid) => {
                    return Err(format!(
                        "the part header named no transfer syntax and the dataset declares \
                         {}, not {EXPLICIT_VR_LE} (explicit VR little endian); the instance \
                         is compressed and cannot be read",
                        String::from_utf8_lossy(uid)
                    )
                    .into());
                }
                None => {
                    return Err(Error::Refused(
                        "the part header named no transfer syntax and the dataset declares \
                         none either ((0002,0010) is absent); the instance's encoding cannot \
                         be established, so it is refused rather than read as samples"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(part.body)
    }

    /// Store one DICOM object: `POST /dicomweb/studies`, PS3.18 10.5.1
    /// (StoreInstances) as `multipart/related`.
    ///
    /// Single-part `application/dicom` is a conformant body (PS3.18 8.7.3.1:
    /// the transfer syntax travels in the DICOM File Meta), but dcm4chee-arc
    /// answers it 415 - it insists on `multipart/related` even for one
    /// Instance. Multipart is accepted by both dcm4chee and Clarus, so it is
    /// the portable form, and the per-part `transfer-syntax` parameter is
    /// named explicitly.
    ///
    /// Retried: this is a write the booth cannot afford to lose, and a 503
    /// means the origin is busy, not that it refused the object. A retry
    /// re-sends the same bytes; the origin is idempotent per SOP Instance
    /// UID (it stores duplicates as duplicates rather than corrupting).
    pub fn store_instance(&self, instance: &[u8]) -> Result<()> {
        const BOUNDARY: &str = "clinfer-stow-20261010";
        let mut body = Vec::with_capacity(instance.len() + 160);
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Type: application/dicom; transfer-syntax={EXPLICIT_VR_LE}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(instance);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        let mut request = Request::json(Method::Post, self.client.url("/studies"), body);
        // The body is a DICOM object, not JSON: `Request::json` sets the
        // JSON content type, which is only correct for the attribute calls.
        request.content_type = Some(format!(
            "multipart/related; type=\"application/dicom\"; boundary={BOUNDARY}"
        ));
        request.accept = Some(crate::http::DICOM_JSON.to_string());
        let response = self.client.send_retrying(&request, &mut 0)?;
        if !response.is_success() {
            return Err(status_error(response));
        }
        Ok(())
    }
}

/// The media type asking for an uncompressed instance, with the transfer
/// syntax parameter the standard defines for it.
pub const EXPLICIT_VR_LE_ACCEPT: &str =
    "multipart/related; type=\"application/dicom\"; transfer-syntax=1.2.840.10008.1.2.1";

/// One part of a retrieve response: its body and the transfer syntax its own
/// header named, when the origin labelled it.
pub struct Part {
    pub body: Vec<u8>,
    pub transfer_syntax: Option<String>,
}

/// Whether `value` starts with `prefix`, ignoring ASCII case.
fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len()
        && value.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// The value of `name` in a `;`-separated header parameter list.
///
/// Parameter NAMES are case-insensitive, the VALUE keeps its case (RFC 2046
/// 5.1.1 makes a MIME boundary case-sensitive). The value ends at the next
/// `;`, and surrounding whitespace and quotes are removed.
fn parameter_of<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    for part in header.split(';').skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            let value = value.trim();
            let value = value.strip_prefix('"').unwrap_or(value);
            let value = value.strip_suffix('"').unwrap_or(value);
            return Some(value);
        }
    }
    None
}

/// Take the first part out of a retrieve response.
///
/// A multipart response is what the standard requires and a bare body is
/// what a simpler origin sends; taking both means a device that skips the
/// multipart wrapper still works. The body is returned verbatim, because the
/// DICOM reader needs the whole dataset and not a re-serialised one.
///
/// A bare body carries no part header, so its transfer syntax is unknown and
/// left `None`: the reader then decides from the dataset itself, which is the
/// only option when the origin said nothing.
///
/// Runs once per retrieved instance, which is once per frame on every path
/// that reads a rendered frame, so the delimiter is built into a stack
/// buffer rather than a `String`: an owned delimiter here is an allocation
/// per frame for a value that lives for the length of one search.
pub fn first_part(response: crate::http::Response) -> Part {
    // The header name is matched case-insensitively (http::Response::header
    // does that), but the VALUE is used as sent. Lowercasing the whole header
    // to find `boundary=` also lowercased the boundary itself, and a boundary
    // is case-SENSITIVE (RFC 2046 5.1.1) - the delimiter then never matched
    // the body and the part came back empty.
    let content_type = response.header("content-type").unwrap_or("");
    if !starts_with_ignore_ascii_case(content_type, "multipart/") {
        return Part {
            body: response.body,
            transfer_syntax: None,
        };
    }
    let Some(boundary) = parameter_of(content_type, "boundary") else {
        return Part {
            body: response.body,
            transfer_syntax: None,
        };
    };
    // "--" plus the boundary plus the terminator, on the stack. A boundary
    // longer than the buffer is not a boundary a conformant origin is
    // allowed to send (PS3.18 caps it well below this), and guessing past
    // the cap would mean searching for a truncated delimiter.
    let boundary = boundary.trim_matches('"').trim();
    let mut delimiter = [0u8; DELIMITER_CAPACITY];
    let Some(delimiter) = delimiter_for(boundary, &mut delimiter) else {
        return Part {
            body: response.body,
            transfer_syntax: None,
        };
    };
    let body = response.body;
    let start = match find_bytes(&body, delimiter) {
        Some(start) => start + delimiter.len(),
        None => {
            return Part {
                body,
                transfer_syntax: None,
            };
        }
    };
    // The part's own headers run to the first blank line. The transfer
    // syntax lives among them, so the headers are read before they are
    // skipped rather than discarded.
    let headers_end = match find_bytes(&body[start..], b"\r\n\r\n") {
        Some(at) => start + at,
        None => {
            return Part {
                body,
                transfer_syntax: None,
            };
        }
    };
    let syntax = part_transfer_syntax(&body[start..headers_end]);
    let after_headers = headers_end + 4;
    // The part ends at the next delimiter. The standard puts a line break
    // before that delimiter, and the break belongs to the framing rather
    // than to the part, so it is trimmed rather than taken as data. Its
    // exact spelling differs between origins - CRLF, or LF alone - so the
    // trim walks back over both characters.
    let end = match find_bytes(&body[after_headers..], delimiter) {
        Some(at) => {
            let mut end = after_headers + at;
            while end > after_headers && (body[end - 1] == b'\n' || body[end - 1] == b'\r') {
                end -= 1;
            }
            end
        }
        None => body.len(),
    };
    Part {
        body: body[after_headers..end].to_vec(),
        transfer_syntax: syntax,
    }
}

/// Room for `--` + a boundary + a NUL terminator.
///
/// PS3.18 does not let a boundary approach this: the parameter is a short
/// token, and the cap exists to keep the delimiter on the stack rather than
/// to accommodate a long one.
const DELIMITER_CAPACITY: usize = 128;

/// Write `--<boundary>` into `buf` and return it as a borrowed slice.
///
/// `None` when the boundary does not fit: the caller then treats the
/// response as one it cannot unwrap, which is the same outcome as a missing
/// boundary and never a silently truncated search.
fn delimiter_for<'a>(boundary: &str, buf: &'a mut [u8; DELIMITER_CAPACITY]) -> Option<&'a [u8]> {
    let bytes = boundary.as_bytes();
    let len = 2 + bytes.len();
    if len > buf.len() {
        return None;
    }
    buf[0] = b'-';
    buf[1] = b'-';
    buf[2..len].copy_from_slice(bytes);
    Some(&buf[..len])
}

/// Read the `transfer-syntax` parameter out of a part's headers.
///
/// The parameter is what PS3.18 section 8.7 defines for a part of a
/// multipart/related response, and it is the only place a stored transfer
/// syntax is stated about the part itself. It is matched case-insensitively
/// because the parameter name is not case-sensitive and origins differ.
fn part_transfer_syntax(headers: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(headers);
    let lower = text.to_ascii_lowercase();
    let at = lower.find("transfer-syntax=")?;
    let value = &text[at + "transfer-syntax=".len()..];
    let value = value
        .split([';', '\r', '\n'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"')
        .to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// Find a byte sequence, without pulling in a search crate for one call.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The transfer syntax the engine retrieves in.
pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";

// -- Capabilities (WADL, PS3.18 8.9 + Annex H) --------------------------

/// One `<resource>` of the Capabilities Description, with its methods and
/// its child resources.
///
/// `path` is the resource's `path` attribute verbatim, so a templated child
/// such as `{subscriptionSOPClassUID}` is told from a literal one by its
/// braces - which is what the subscribers check needs, since the subscribers
/// resource is a child of a templated workitems child, not of the workitems
/// resource itself.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WadlResource {
    pub path: String,
    /// The `name` attribute of every `<method>` child, in document order.
    pub methods: Vec<String>,
    pub children: Vec<WadlResource>,
}

impl WadlResource {
    /// The first child with this exact `path`, if any.
    #[must_use]
    pub fn child(&self, path: &str) -> Option<&WadlResource> {
        self.children.iter().find(|child| child.path == path)
    }

    /// Whether this resource declares a method with this `name`.
    #[must_use]
    pub fn has_method(&self, name: &str) -> bool {
        self.methods.iter().any(|method| method == name)
    }
}

/// A parsed Capabilities Description: the resources under `<resources>`,
/// in document order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Wadl {
    pub resources: Vec<WadlResource>,
}

impl Wadl {
    /// The first top-level resource with this exact `path`, if any.
    #[must_use]
    pub fn resource(&self, path: &str) -> Option<&WadlResource> {
        self.resources.iter().find(|resource| resource.path == path)
    }
}

/// Parse a Capabilities Description as the WADL resource tree (PS3.18 8.9,
/// Annex H).
///
/// The crate is std-first and carries no XML parser, so this is a scan for
/// the two element shapes the checks need - `<resource path="...">` and its
/// `<method name="...">` children - and nothing else. It is enough to
/// reconstruct the nesting, because a resource nests only inside another
/// resource, and it refuses a document whose element nesting does not close,
/// which is the only well-formedness the checks depend on.
///
/// # Errors
/// [`Error::Refused`] when the body is not UTF-8, when a tag is left
/// unclosed, or when a close tag does not match the open one.
pub fn parse_wadl(body: &[u8]) -> Result<Wadl> {
    let text = std::str::from_utf8(body)
        .map_err(|err| Error::Refused(format!("capabilities: response is not UTF-8: {err}")))?;
    let mut document = Wadl::default();
    let mut stack: Vec<WadlResource> = Vec::new();
    let mut index = 0;
    while let Some(open) = text[index..].find('<') {
        let start = index + open;
        let Some(close_rel) = text[start..].find('>') else {
            return Err(Error::Refused(
                "capabilities: unterminated element".to_string(),
            ));
        };
        let close = start + close_rel;
        let element = text[start + 1..close].trim();
        index = close + 1;
        if element.is_empty() || element.starts_with('?') || element.starts_with('!') {
            continue;
        }
        if let Some(name) = element.strip_prefix('/') {
            // Only `<resource>` elements are tracked, so only `</resource>`
            // pops the stack; the closes of `application`, `resources` and
            // the like pass through. A `</resource>` with an empty stack is
            // then the one shape that says the document is not WADL.
            if name.trim() == "resource" {
                let Some(current) = stack.pop() else {
                    return Err(Error::Refused(
                        "capabilities: closing </resource> without an open element".to_string(),
                    ));
                };
                push(&mut document, &mut stack, current);
            }
            continue;
        }
        let self_closing = element.ends_with('/');
        let name = element
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        if name == "resource" {
            let resource = WadlResource {
                path: attribute(element, "path").unwrap_or_default(),
                ..WadlResource::default()
            };
            if self_closing {
                push(&mut document, &mut stack, resource);
            } else {
                stack.push(resource);
            }
        } else if let ("method", Some(method), Some(current)) =
            (name, attribute(element, "name"), stack.last_mut())
        {
            current.methods.push(method);
        }
        // Every other element is ignored: the checks read only resource
        // paths and method names.
    }
    if !stack.is_empty() {
        return Err(Error::Refused(
            "capabilities: an element was left open".to_string(),
        ));
    }
    Ok(document)
}

/// Attach a closed resource to its parent, or to the document root.
fn push(document: &mut Wadl, stack: &mut [WadlResource], resource: WadlResource) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(resource),
        None => document.resources.push(resource),
    }
}

/// The value of one attribute in an element's text, unquoted.
fn attribute(element: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=");
    let at = element.find(&marker)?;
    let rest = element.get(at + marker.len()..)?;
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = rest.get(quote.len_utf8()..)?;
    let end = value.find(quote)?;
    Some(value.get(..end)?.to_string())
}

#[must_use]
pub fn capabilities_has_workitems_and_subscribers(document: &Wadl) -> (bool, bool) {
    let Some(workitems) = document.resource("workitems") else {
        return (false, false);
    };
    // The subscribers resource of PS3.4 Table CC.2.5-1 hangs off the
    // `{subscriptionSOPClassUID}` child of `workitems` and carries the
    // Create Subscription POST (PS3.18 11.10). A `subscribers` path found
    // elsewhere is not the subscription resource, so the child path is part
    // of the check rather than only the name.
    let subscribers = workitems
        .child("{subscriptionSOPClassUID}")
        .and_then(|child| child.child("subscribers"))
        .is_some_and(|subscribers| subscribers.has_method("POST"));
    (true, subscribers)
}

fn status_error(response: crate::http::Response) -> Error {
    Error::Status(StatusError {
        status: response.status,
        body: response.body_text(),
    })
}

/// Build a DICOM JSON object from tag/value string pairs.
///
/// Every field clinfer writes into a workitem is a string value, so this
/// is the only encoder the UPS path needs and there is no reason for a
/// general one.
#[must_use]
pub fn encode_attributes(fields: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(fields.len() * 48 + 2);
    out.push(b'{');
    for (index, (tag, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        out.push(b'"');
        out.extend_from_slice(tag.as_bytes());
        out.extend_from_slice(b"\":{\"Value\":[");
        push_json_string(&mut out, value);
        out.extend_from_slice(b"]}");
    }
    out.push(b'}');
    out
}

/// Encode a body that also states the performed step and names the produced
/// objects.
///
/// The performed report and the output list are ONE item of (0074,1216)
/// Unified Procedure Step Performed Procedure Sequence, because PS3.3 Table
/// C.30.3-1 nests every one of these attributes there and none of them at the
/// top level of the workitem. A standard origin does not merely prefer that
/// shape: a live dcm4chee-arc 5.35.2 answers 200 to the top-level form,
/// SILENTLY DISCARDS the attributes it cannot place, and then refuses the
/// following COMPLETED transition with 409 - so the failure appears one
/// request later than its cause, which is why this was worth getting exactly
/// right rather than approximately.
///
/// The item's children are written in ascending tag order, which is what a
/// DICOM data set requires and what makes the output comparable byte for byte
/// with another writer's:
///
///   (0040,4019) Performed Workitem Code Sequence   - when the workitem had one
///   (0040,4028) Performed Station Name Code Sequence
///   (0040,4033) Output Information Sequence        - when something was stored
///   (0040,4050) Performed Procedure Step Start DateTime
///   (0040,4051) Performed Procedure Step End DateTime
///
/// The sequence is emitted whenever the report is present, and it always is
/// here: the two datetimes are mandatory members, so there is no case where a
/// caller has a report and no children to write.
///
/// `outputs` names each produced object by its real SOP Class UID, Series
/// Instance UID and SOP Instance UID, i.e. the class the object itself carries
/// in (0008,0016) - a Secondary Capture for a derived image, a Segmentation for
/// a SEG, a Grayscale Softcopy Presentation State for a GSPS - and the series
/// it belongs to. The class is NOT
/// assumed: a hardcoded class made the reference disagree with every object
/// that was not of that class, and a consumer that resolves the reference
/// then finds a class it cannot read.
///
/// `outputs` may be EMPTY. A completion that stored nothing is a caller that
/// should have cancelled instead, but the encoder does not invent an empty
/// (0040,4033) to say so: an empty Output Information Sequence claims "ran and
/// produced nothing", and omitting the sequence says the step produced nothing
/// to name. Only the presence of the sequence is a claim.
#[must_use]
pub fn encode_performed_report(
    fields: &[(String, String)],
    outputs: &[OutputRef],
    performed: &PerformedReport,
) -> Vec<u8> {
    let mut out = encode_attributes(fields);
    // Replace the closing brace with the sequence and then close it once.
    out.pop();
    if !fields.is_empty() {
        out.push(b',');
    }
    out.extend_from_slice(b"\"");
    out.extend_from_slice(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE.as_bytes());
    out.extend_from_slice(b"\":{\"vr\":\"SQ\",\"Value\":[{");

    // The step's own name, first in tag order. Appended WITHOUT a leading
    // comma: the item is a fresh object, and `append_code_sequence` writes
    // the separator only when something already precedes it.
    let mut item = Vec::with_capacity(256);
    item.push(b'{');
    if let Some(code) = &performed.workitem_code {
        append_code_sequence(
            &mut item,
            TAG_PERFORMED_WORKITEM_CODE_SEQUENCE,
            &code.value,
            &code.scheme,
            &code.meaning,
        );
    }
    append_code_sequence(
        &mut item,
        TAG_PERFORMED_STATION_NAME_CODE_SEQUENCE,
        &performed.station_aet,
        &performed.station_scheme,
        &performed.station_name,
    );
    if !outputs.is_empty() {
        append_output_information(&mut item, outputs);
    }
    append_datetime_sequence(
        &mut item,
        TAG_PERFORMED_START_DATETIME,
        &performed.started_at,
    );
    append_datetime_sequence(&mut item, TAG_PERFORMED_END_DATETIME, &performed.ended_at);
    item.push(b'}');

    // The item's own braces are the sequence's value; splice it in whole.
    out.extend_from_slice(&item[1..item.len() - 1]);
    out.extend_from_slice(b"}]}}");
    out
}

/// Append (0040,4033) Output Information Sequence: one item per stored
/// object, each carrying (0008,1150) Referenced SOP Class UID, (0020,000E)
/// Series Instance UID and (0008,1155) Referenced SOP Instance UID - the same
/// reference shape a presentation state uses to point at its source image,
/// plus the series the macro types 1C so a retrieve URL can be built.
///
/// Each `outputs` entry names its class (the object's own (0008,0016), so the
/// reference names the class a consumer will actually read rather than one
/// assumed for every object) and its series.
///
/// `out` is the item being built WITHOUT its closing brace.
fn append_output_information(out: &mut Vec<u8>, outputs: &[OutputRef]) {
    if out.last() != Some(&b'{') {
        out.push(b',');
    }
    out.extend_from_slice(b"\"");
    out.extend_from_slice(TAG_OUTPUT_INFORMATION_SEQUENCE.as_bytes());
    out.extend_from_slice(b"\":{\"vr\":\"SQ\",\"Value\":[");
    for (index, output) in outputs.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"");
        out.extend_from_slice(TAG_REFERENCED_SOP_CLASS_UID.as_bytes());
        out.extend_from_slice(b"\":{\"vr\":\"UI\",\"Value\":[");
        push_json_string(out, &output.sop_class_uid);
        out.extend_from_slice(b"]},\"");
        out.extend_from_slice(TAG_REFERENCED_SERIES_INSTANCE_UID.as_bytes());
        out.extend_from_slice(b"\":{\"vr\":\"UI\",\"Value\":[");
        push_json_string(out, &output.series_instance_uid);
        out.extend_from_slice(b"]},\"");
        out.extend_from_slice(TAG_REFERENCED_SOP_INSTANCE_UID.as_bytes());
        out.extend_from_slice(b"\":{\"vr\":\"UI\",\"Value\":[");
        push_json_string(out, &output.sop_instance_uid);
        out.extend_from_slice(b"]}}");
    }
    out.extend_from_slice(b"]}");
}

/// Append a `DT` attribute as a one-value sequence-less object.
///
/// The performed datetimes are typed `DT` scalar in Table C.30.3-1, not a
/// sequence, so they are written as `{"vr":"DT","Value":["..."]}` - the same
/// `Value` array shape every DICOM JSON attribute uses, with one element.
///
/// `out` is the item being built WITHOUT its closing brace.
fn append_datetime_sequence(out: &mut Vec<u8>, tag: &str, value: &str) {
    if out.last() != Some(&b'{') {
        out.push(b',');
    }
    out.extend_from_slice(b"\"");
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(b"\":{\"vr\":\"DT\",\"Value\":[");
    push_json_string(out, value);
    out.extend_from_slice(b"]}");
}

/// What the performer reports about itself on the update that closes a step.
///
/// These are the update-time obligations of the Task Performer named by
/// AIW-I 4.84.4.1.2.1, and PS3.3 Table C.30.3-1 places ALL of them inside
/// (0074,1216) Unified Procedure Step Performed Procedure Sequence rather
/// than at the top level of the workitem:
///
/// - `(0040,4028)` Performed Station Name Code Sequence - the station that
///   ran the step. This is the SAME station identity the query and the
///   claim use, told a third time because the standard asks for it on this
///   transaction; it is not a second, independent setting.
/// - `(0040,4019)` Performed Workitem Code Sequence - "the same code as
///   Scheduled, if present". The performer executes the step the requester
///   scheduled, so the honest report echoes (0040,4018); a workitem that
///   scheduled no code has nothing to echo and the sequence is omitted
///   rather than invented.
/// - `(0040,4050)` / `(0040,4051)` Performed Procedure Step Start/End
///   DateTime - when this performer ran the step. A standard origin requires
///   both before it accepts the COMPLETED transition (verified against
///   live dcm4chee-arc 5.35.2), so they are not decoration: their absence is
///   what makes a completion fail after the update was accepted.
///
/// `scheme` is the Coding Scheme Designator of the station code. A station
/// AE Title is not drawn from a published vocabulary, so it is declared as
/// a local scheme (`99CLARUS`, the same private-scheme convention the
/// scheduled code uses) instead of pretending to be a DICOM-defined code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerformedReport {
    pub station_aet: String,
    pub station_name: String,
    pub station_scheme: String,
    /// The scheduled workitem code to echo, when the workitem carried one.
    pub workitem_code: Option<WorkitemCode>,
    /// `(0040,4050)` Performed Procedure Step Start DateTime, as DICOM `DT`.
    pub started_at: String,
    /// `(0040,4051)` Performed Procedure Step End DateTime, as DICOM `DT`.
    pub ended_at: String,
}

/// The coding scheme a locally-invented code value is declared under.
///
/// PS3.3 Table 8.8-1 requires Coding Scheme Designator `99<SITE>` for a
/// private scheme, and AIW-I 50.4.1.2 leaves the code values themselves to
/// deployment agreement. This is the deployment's scheme, written once.
pub const LOCAL_CODING_SCHEME: &str = "99CLARUS";

/// Append a `CodeSequenceMacro` attribute holding one code.
///
/// `out` is the object being built WITHOUT its closing brace. The separator
/// is a comma unless the object is still empty (a bare `{`), which is the
/// case when this is the only attribute in the payload.
fn append_code_sequence(out: &mut Vec<u8>, tag: &str, value: &str, scheme: &str, meaning: &str) {
    if out.last() != Some(&b'{') {
        out.push(b',');
    }
    out.extend_from_slice(b"\"");
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(b"\":{\"vr\":\"SQ\",\"Value\":[{\"");
    out.extend_from_slice(TAG_CODE_VALUE.as_bytes());
    out.extend_from_slice(b"\":{\"vr\":\"SH\",\"Value\":[");
    push_json_string(out, value);
    out.extend_from_slice(b"]},\"");
    out.extend_from_slice(TAG_CODING_SCHEME_DESIGNATOR.as_bytes());
    out.extend_from_slice(b"\":{\"vr\":\"SH\",\"Value\":[");
    push_json_string(out, scheme);
    out.extend_from_slice(b"]},\"");
    out.extend_from_slice(TAG_CODE_MEANING.as_bytes());
    out.extend_from_slice(b"\":{\"vr\":\"LO\",\"Value\":[");
    push_json_string(out, meaning);
    out.extend_from_slice(b"]}}]}");
}

/// The matching keys of a Filtered Worklist Subscription (PS3.18 11.11).
///
/// A filter is a list of `tag=value` pairs in the QUERY string, in the
/// matching-key syntax of PS3.18 8.3.4.1: a top-level attribute is named by
/// its tag, and an attribute inside a sequence by the sequence tag and the
/// inner tag joined with a dot.
///
/// # Why the scheme alone is the performer's filter
///
/// PS3.18 8.3.4.1 states "Each attribute shall not be repeated", so ONE
/// subscription cannot carry several code values - a filter listing every
/// code the rules know would be illegal. The legal form that still removes
/// the noise is the SCHEME: every code this deployment mints is under
/// `99CLARUS`, so filtering on (0040,4018) > (0008,0102) = 99CLARUS drops
/// workitems coded under any foreign scheme while leaving the per-code
/// decision where it belongs - in the rules.
///
/// The consequence, stated so it is not a surprise: a workitem coded under
/// another scheme is invisible to this performer BY DESIGN of the filter.
/// That is AIW-I 50.4.1.2's deployment agreement ("Task Requester, Task
/// Manager, and Task Performer must agree upon the codes used") applied to
/// the scheme as well as to the values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorklistFilter {
    /// One `tag=value` pair per matching key.
    pub keys: Vec<(String, String)>,
}

impl WorklistFilter {
    /// The performer's filter: every code of this deployment's scheme.
    ///
    /// Derived from the scheme CONSTANT rather than from the rule set: the
    /// scheme is what marks a workitem as this deployment's business, and a
    /// filter built from the rules would have to be rebuilt whenever they
    /// changed - a second source of truth for the same fact.
    #[must_use]
    pub fn own_scheme() -> Self {
        Self {
            keys: vec![(
                TAG_SCHEDULED_WORKITEM_CODE_SCHEME.to_string(),
                LOCAL_CODING_SCHEME.to_string(),
            )],
        }
    }
}

/// Append `?filter=tag=value(&filter=...)` to a subscription path.
///
/// The value is percent-encoded (RFC 3986): a code value is a machine key and
/// normally needs nothing, but the encoder is what makes an unusual one -
/// a printable non-ASCII value from a foreign scheme - arrive as written
/// instead of corrupting the query string.
fn push_query(filter: &WorklistFilter, path: &mut String) {
    for (index, (tag, value)) in filter.keys.iter().enumerate() {
        path.push(if index == 0 { '?' } else { '&' });
        path.push_str("filter=");
        path.push_str(tag);
        path.push('=');
        push_percent_encoded(path, value);
    }
}

/// Percent-encode one query value, appending to `out`.
fn push_percent_encoded(out: &mut String, value: &str) {
    for byte in value.as_bytes() {
        match byte {
            // Unreserved (RFC 3986 2.3): A-Z a-z 0-9 - . _ ~
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            other => {
                out.push('%');
                out.push(hex_char(other >> 4));
                out.push(hex_char(other & 0x0F));
            }
        }
    }
}

fn hex_char(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + nibble - 10) as char,
    }
}

/// The subscription body: the optional endpoint, and nothing else.
///
/// With no endpoint the body is `{}`: the connection is what delivers
/// notifications, and the filter travels in the QUERY string (PS3.18
/// 11.10.1.4: the subscription payload is "no payload"). The body therefore
/// never carries a filter.
#[must_use]
pub fn subscription_body(endpoint: Option<&str>) -> Vec<u8> {
    let mut out = Vec::with_capacity(endpoint.map_or(2, str::len) + 16);
    match endpoint.filter(|value| !value.is_empty()) {
        Some(endpoint) => {
            out.extend_from_slice(b"{\"endpoint\":");
            push_json_string(&mut out, endpoint);
            out.push(b'}');
        }
        None => out.extend_from_slice(b"{}"),
    }
    out
}

/// Percent-encode one query-string value (RFC 3986, the form-urlencoded
/// subset PS3.18 uses for matching keys).
///
/// An AE Title is a restricted character repertoire, but it may still carry
/// a space or a `+`, both of which are significant in a form-encoded query;
/// encoding the unreserved set only is the safe form because it never
/// changes a value that needed no encoding.
#[must_use]
pub fn form_encode(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0F) as usize] as char);
            }
        }
    }
    out
}

/// Escape and quote a JSON string. Every value written here is an
/// identifier or a keyword, so the escapes are the two that can occur.
fn push_json_string(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    for byte in text.bytes() {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x00..=0x1F => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(byte >> 4) as usize]);
                out.push(HEX[(byte & 0x0F) as usize]);
            }
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// Flatten (0074,1210) Scheduled Processing Parameters Sequence into the
/// parameter list a script reads.
///
/// PS3.3 Table C.30.2-1 makes every item a Content Item: the Concept Name
/// sequence names the parameter, (0040,A040) says how to read the value, and
/// exactly one of Text / Numeric / Code carries it. The sidecar cannot know
/// whether the requester meant "threshold" or "window", so it does not
/// interpret - it hands the name and the value through.
///
/// An item the sidecar cannot read is SKIPPED with a WARN naming the
/// parameter's NAME only. The value never reaches the log: a parameter list
/// is requester-supplied text and may carry anything the site typed into it,
/// and a diagnostic is not a reason to copy that into a log file.
#[must_use]
pub fn scheduled_processing_parameters(value: &Json) -> Vec<Parameter> {
    let Some(items) = value
        .get(TAG_SCHEDULED_PROCESSING_PARAMETERS_SEQUENCE)
        .and_then(|entry| entry.get("Value"))
        .and_then(Json::as_array)
    else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Some(parameter) = read_parameter(item) {
            out.push(parameter);
        }
    }
    out
}

/// Read one Content Item of the parameter sequence.
///
/// PS3.3 Table 10-2 selects ONE value attribute per item by its Value Type;
/// this reader understands TEXT, NUMERIC, CODE and DATETIME. DATE, TIME,
/// UIDREF and PNAME are reported and skipped rather than coerced: a parameter
/// the sidecar guesses at is worse than one a script never sees.
///
/// `None` is returned after the WARN has been printed, so the caller keeps one
/// skip rule instead of repeating it per failure.
fn read_parameter(item: &Json) -> Option<Parameter> {
    let name_item = item.get(TAG_CONCEPT_NAME_CODE_SEQUENCE)?;
    let name_first = name_item.get("Value")?.as_array()?.first()?;
    let name = name_first.attribute(TAG_CODE_VALUE)?.trim();
    if name.is_empty() {
        eprintln!(
            "WARN scheduled processing parameter without a Concept Name Code Value \
             value type {:?}: skipped",
            item.attribute(TAG_VALUE_TYPE).unwrap_or("")
        );
        return None;
    }
    let value = match item.attribute(TAG_VALUE_TYPE).unwrap_or("") {
        "TEXT" => match item.attribute(TAG_TEXT_VALUE) {
            Some(text) => ParamValue::Text(text.to_string()),
            None => {
                eprintln!(
                    "WARN scheduled processing parameter '{name}' has no Text Value: skipped"
                );
                return None;
            }
        },
        "NUMERIC" => match item.attribute(TAG_NUMERIC_VALUE) {
            Some(ds) => {
                // A DS is a backslash-separated list (PS3.5 6.2); the
                // parameter's value is its first component.
                let first = ds.split('\\').next().unwrap_or("").trim();
                match first.parse::<f64>() {
                    Ok(number) if number.is_finite() => ParamValue::Numeric(number),
                    _ => {
                        eprintln!(
                            "WARN scheduled processing parameter '{name}' has a Numeric \
                             Value that is not a finite number: skipped"
                        );
                        return None;
                    }
                }
            }
            None => {
                eprintln!(
                    "WARN scheduled processing parameter '{name}' has no Numeric Value: skipped"
                );
                return None;
            }
        },
        "CODE" => {
            // PS3.3 Table 10-2: Value Type CODE selects Concept Code
            // Sequence (0040,A168), a SINGLE item carrying the Code Sequence
            // Macro. The value is not a bare (0008,0100) on the content item,
            // so a workitem carrying one is not read as a code at all - the
            // item is skipped with a WARN naming the missing sequence.
            let Some(nested) = item.attribute_item(TAG_CONCEPT_CODE_SEQUENCE) else {
                eprintln!(
                    "WARN scheduled processing parameter '{name}' has no Concept Code Sequence \
                     (0040,A168) for its CODE type: skipped"
                );
                return None;
            };
            let code_value = nested.attribute(TAG_CODE_VALUE).unwrap_or("").trim();
            if code_value.is_empty() {
                eprintln!(
                    "WARN scheduled processing parameter '{name}' has no Code Value in its \
                     Concept Code Sequence: skipped"
                );
                return None;
            }
            ParamValue::Code {
                scheme: nested
                    .attribute(TAG_CODING_SCHEME_DESIGNATOR)
                    .unwrap_or("")
                    .to_string(),
                value: code_value.to_string(),
            }
        }
        "DATETIME" => match item.attribute(TAG_DATETIME_VALUE) {
            Some(dt) if !dt.trim().is_empty() => ParamValue::DateTime(dt.to_string()),
            _ => {
                eprintln!(
                    "WARN scheduled processing parameter '{name}' has no DateTime Value: skipped"
                );
                return None;
            }
        },
        other => {
            eprintln!(
                "WARN scheduled processing parameter '{name}' has Value Type '{other}', which \
                 this sidecar does not handle: skipped"
            );
            return None;
        }
    };
    Some(Parameter {
        name: name.to_string(),
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `filter` query parameter belongs to the FILTERED resource
    /// (34.5.1), not to the plain Worklist one (34.5) - PS3.18 Table 11.10.1-2.
    /// Putting it on the plain resource is what made the filter a silent no-op:
    /// the performer then received every workitem's events.
    ///
    /// This asserts the DECISION (`subscription_resource`), not the constants:
    /// a test that only checks the UID literals passes just as happily when the
    /// call site always picks the plain resource, which is exactly the bug.
    #[test]
    fn the_filtered_form_targets_the_filtered_subscription_resource() {
        let filter = WorklistFilter::own_scheme();
        assert_eq!(
            subscription_resource(Some(&filter)),
            FILTERED_WORKLIST_SUBSCRIPTION_UID,
            "a filter must select the filtered resource"
        );
        assert_eq!(
            subscription_resource(None),
            WORKLIST_SUBSCRIPTION_UID,
            "no filter must select the plain resource"
        );
        // The two are distinct resources, not one with a flag.
        assert_ne!(
            FILTERED_WORKLIST_SUBSCRIPTION_UID,
            WORKLIST_SUBSCRIPTION_UID
        );
        assert_eq!(WORKLIST_SUBSCRIPTION_UID, "1.2.840.10008.5.1.4.34.5");
        assert_eq!(
            FILTERED_WORKLIST_SUBSCRIPTION_UID,
            "1.2.840.10008.5.1.4.34.5.1"
        );

        // And the query only rides on the filtered form.
        let mut path = format!(
            "/workitems/{}/subscribers/PERF",
            subscription_resource(Some(&filter))
        );
        push_query(&filter, &mut path);
        assert!(
            path.starts_with("/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/PERF"),
            "a filtered subscription must target the filtered resource: {path}"
        );
        assert!(path.contains("filter="), "and carry the filter: {path}");
    }

    #[test]
    fn attributes_encode_as_a_dicom_json_object() {
        let body = encode_attributes(&[
            (TAG_UPS_STATE.to_string(), "IN PROGRESS".to_string()),
            (TAG_TRANSACTION_UID.to_string(), "1.2.3".to_string()),
        ]);
        let text = String::from_utf8(body).expect("utf8");
        assert_eq!(
            text,
            r#"{"00741000":{"Value":["IN PROGRESS"]},"00081195":{"Value":["1.2.3"]}}"#
        );
        // And it parses back, which is the property that matters.
        let parsed = json::parse(text.as_bytes()).expect("parses");
        assert_eq!(parsed.attribute(TAG_UPS_STATE), Some("IN PROGRESS"));
    }

    #[test]
    fn encoding_escapes_a_quote_in_a_value() {
        let body = encode_attributes(&[("00081030".to_string(), "a\"b".to_string())]);
        let parsed = json::parse(&body).expect("parses");
        assert_eq!(parsed.attribute("00081030"), Some("a\"b"));
    }

    #[test]
    fn a_subscription_body_is_the_unfiltered_form_without_a_filter() {
        let body = subscription_body(Some("http://127.0.0.1:8124/ups"));
        assert_eq!(
            String::from_utf8(body).expect("utf8"),
            r#"{"endpoint":"http://127.0.0.1:8124/ups"}"#
        );
    }

    #[test]
    fn a_subscription_without_an_endpoint_carries_no_endpoint_key() {
        // The channel-only form: the origin must not be told to POST
        // anywhere, and a subscriber that omitted the URI must not be
        // sent an empty one either, which would read as a malformed URI.
        let body = subscription_body(None);
        let text = String::from_utf8(body).expect("utf8");
        assert_eq!(text, "{}");
        assert!(!text.contains("endpoint"), "no endpoint key at all: {text}");
    }

    #[test]
    fn an_empty_endpoint_reads_as_no_endpoint() {
        // `Some("")` is the same request as `None`: an empty URI is not a
        // target, and emitting `"endpoint":""` would ask the origin to POST
        // to nothing.
        assert_eq!(
            String::from_utf8(subscription_body(Some(""))).expect("utf8"),
            String::from_utf8(subscription_body(None)).expect("utf8")
        );
    }

    /// The body never carries a filter: the filter is a query parameter
    /// (PS3.18 11.10.1.4 makes the subscription payload "no payload"), so a
    /// channel-only filtered subscription still sends `{}`.
    #[test]
    fn the_subscription_body_never_carries_a_filter() {
        let text = String::from_utf8(subscription_body(None)).expect("utf8");
        assert_eq!(text, "{}");
        assert!(
            !text.contains("filter"),
            "the filter is not in the body: {text}"
        );
    }

    /// The performer's filter is the scheme key, in the query syntax of
    /// PS3.18 8.3.4.1: `00404018.00080102=99CLARUS`.
    #[test]
    fn the_own_scheme_filter_is_the_single_nested_scheme_key() {
        let filter = WorklistFilter::own_scheme();
        assert_eq!(
            filter.keys.len(),
            1,
            "one key: an attribute shall not repeat"
        );
        assert_eq!(
            filter.keys[0],
            (
                TAG_SCHEDULED_WORKITEM_CODE_SCHEME.to_string(),
                LOCAL_CODING_SCHEME.to_string()
            )
        );
        assert_eq!(TAG_SCHEDULED_WORKITEM_CODE_SCHEME, "00404018.00080102");

        let mut path = "/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLINFER".to_string();
        push_query(&filter, &mut path);
        assert_eq!(
            path,
            "/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLINFER\
             ?filter=00404018.00080102=99CLARUS"
        );
    }

    /// Several keys are joined with `&filter=`, and each value is encoded.
    #[test]
    fn several_filter_keys_are_joined_as_repeated_parameters() {
        let filter = WorklistFilter {
            keys: vec![
                ("00741204".to_string(), "CAD STEP".to_string()),
                ("00404018.00080100".to_string(), "CXR-AI".to_string()),
            ],
        };
        let mut path = String::from("/subscribers/X");
        push_query(&filter, &mut path);
        assert_eq!(
            path, "/subscribers/X?filter=00741204=CAD%20STEP&filter=00404018.00080100=CXR-AI",
            "a space is encoded; the unreserved characters are not"
        );
    }

    #[test]
    fn a_workitem_reads_its_attributes() {
        // Identity is (0008,0018); the study is reachable only through the
        // Input Information Sequence, whose first item carries it.
        let document = json::parse(
            br#"{"00080018":{"Value":["1.2.3"]},
                 "00741000":{"Value":["SCHEDULED"]},
                 "00404021":{"vr":"SQ","Value":[{"0040E020":{"Value":["DICOM"]},
                                                     "0020000D":{"Value":["9.9.9"]}}]},
                 "00080060":{"Value":["DX"]},
                 "00081030":{"Value":["CHEST PA"]},
                 "00180015":{"Value":["CHEST"]}}"#,
        )
        .expect("parses");
        let item = WorkItem::from_json(&document).expect("workitem");
        assert_eq!(item.uid, "1.2.3");
        assert_eq!(item.state, "SCHEDULED");
        assert_eq!(item.study_instance_uid.as_deref(), Some("9.9.9"));
        assert_eq!(item.modality, "DX");
        assert_eq!(item.study_description, "CHEST PA");
        assert_eq!(item.body_part, "CHEST");
    }

    #[test]
    fn a_workitem_without_the_input_sequence_names_no_study() {
        // The sequence is the only route to the study, so without it there
        // is nothing for the worker to look up - it cancels instead of
        // searching for a study the workitem never named.
        let document =
            json::parse(br#"{"00080018":{"Value":["1.2.3"]},"00741000":{"Value":["SCHEDULED"]}}"#)
                .expect("parses");
        let item = WorkItem::from_json(&document).expect("workitem");
        assert_eq!(item.uid, "1.2.3");
        assert_eq!(item.study_instance_uid, None);
    }

    #[test]
    fn a_workitem_without_a_sop_instance_uid_is_not_a_workitem() {
        // The identity is (0008,0018); a body without it has no UID to
        // address, so it is not a workitem.
        let document = json::parse(br#"{"00741000":{"Value":["SCHEDULED"]}}"#).expect("parses");
        assert!(WorkItem::from_json(&document).is_none());
    }

    #[test]
    fn both_single_workitem_spellings_parse_to_the_same_workitem() {
        // The failure this closes: `get_workitem` fed the response straight
        // to `from_json`, so the array form dcm4chee-arc sends made every
        // attribute lookup miss and the workitem read as carrying no
        // StudyInstanceUID under an HTTP 200. The two spellings must now be
        // indistinguishable to the caller.
        let body = r#"{"00080018":{"Value":["1.2.3"]},
                       "00741000":{"Value":["SCHEDULED"]},
                       "00404021":{"vr":"SQ","Value":[{"0040E020":{"Value":["DICOM"]},
                                                       "0020000D":{"Value":["9.9.9"]}}]},
                       "00080060":{"Value":["DX"]}}"#;
        let object = json::parse(body.as_bytes()).expect("parses");
        let array = json::parse(format!("[{body}]").as_bytes()).expect("parses");

        let from_object = WorkItem::from_json(
            json::single_dataset(&object).expect("the bare object is a single dataset"),
        )
        .expect("workitem");
        let from_array = WorkItem::from_json(
            json::single_dataset(&array).expect("an array of one is a single dataset"),
        )
        .expect("workitem");

        assert_eq!(from_object.uid, "1.2.3");
        assert_eq!(from_object.uid, from_array.uid);
        assert_eq!(from_object.state, from_array.state);
        assert_eq!(
            from_object.study_instance_uid, from_array.study_instance_uid,
            "including the study the array form used to lose"
        );
        assert_eq!(from_object.study_instance_uid.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn an_array_form_without_the_unwrap_names_no_study() {
        // The control for the test above, on the OLD path: `from_json` on
        // the array itself finds no (0008,0018) and no study, which is the
        // exact silent failure the unwrap removes. Pinning it here means a
        // future edit that drops the unwrap reddens a test rather than
        // returning a wrong answer.
        let array = json::parse(
            br#"[{"00080018":{"Value":["1.2.3"]},
                  "00404021":{"vr":"SQ","Value":[{"0020000D":{"Value":["9.9.9"]}}]}}]"#,
        )
        .expect("parses");
        assert!(
            WorkItem::from_json(&array).is_none(),
            "an array is not a workitem, which is why the unwrap is required"
        );
    }

    #[test]
    fn capabilities_are_recognised_by_their_resource_paths() {
        let present = parse_wadl(
            br#"<application xmlns="http://wadl.dev.java.net/2009/02">
                  <resources base="/dicomweb">
                    <resource path="workitems">
                      <resource path="{subscriptionSOPClassUID}">
                        <resource path="subscribers">
                          <method name="POST" id="Create Subscription"/>
                        </resource>
                      </resource>
                    </resource>
                  </resources>
                </application>"#,
        )
        .expect("parses");
        assert_eq!(
            capabilities_has_workitems_and_subscribers(&present),
            (true, true)
        );

        let absent = parse_wadl(
            br#"<application xmlns="http://wadl.dev.java.net/2009/02">
                  <resources base="/dicomweb">
                    <resource path="studies"><method name="GET" id="Search for Studies"/></resource>
                  </resources>
                </application>"#,
        )
        .expect("parses");
        assert_eq!(
            capabilities_has_workitems_and_subscribers(&absent),
            (false, false)
        );
    }

    #[test]
    fn capabilities_nested_deeper_still_count() {
        // The resource tree is not fixed in depth by the standard, so the
        // walk must not be either.
        let document = parse_wadl(
            br#"<application xmlns="http://wadl.dev.java.net/2009/02">
                  <resources base="/dicomweb">
                    <resource path="workitems">
                      <resource path="{subscriptionSOPClassUID}">
                        <resource path="subscribers">
                          <method name="POST" id="Create Subscription"/>
                        </resource>
                      </resource>
                    </resource>
                  </resources>
                </application>"#,
        )
        .expect("parses");
        assert_eq!(
            capabilities_has_workitems_and_subscribers(&document),
            (true, true)
        );
    }

    #[test]
    fn a_part_header_names_the_transfer_syntax() {
        let headers = b"Content-Type: application/dicom; transfer-syntax=1.2.840.10008.1.2.1\r\n\
                        Content-Length: 12";
        assert_eq!(
            part_transfer_syntax(headers).as_deref(),
            Some("1.2.840.10008.1.2.1")
        );
    }

    #[test]
    fn a_quoted_transfer_syntax_reads_without_its_quotes() {
        // The parameter is a media type parameter, so it may be quoted.
        let headers =
            b"Content-Type: application/dicom; transfer-syntax=\"1.2.840.10008.1.2.4.70\"";
        assert_eq!(
            part_transfer_syntax(headers).as_deref(),
            Some("1.2.840.10008.1.2.4.70")
        );
    }

    #[test]
    fn a_part_without_a_transfer_syntax_says_nothing() {
        // An unlabelled part is the origin declining to state the syntax;
        // the reader then decides from the dataset itself.
        let headers = b"Content-Type: application/dicom";
        assert_eq!(part_transfer_syntax(headers), None);
    }

    /// The exact WADL skeleton Clarus serves (PS3.18 8.9 + Annex H), so the
    /// check is exercised against the real document rather than a reduced
    /// one. The `subscribers` resource sits under the templated
    /// `{subscriptionSOPClassUID}` workitems child and carries the Create
    /// Subscription POST.
    #[test]
    fn the_clarus_wadl_skeleton_reports_both_resources() {
        let document = parse_wadl(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<application xmlns="http://wadl.dev.java.net/2009/02">
  <resources base="http://host:8024/dicomweb">
    <resource path="studies">
      <method name="GET" id="Search for Studies"/>
      <method name="POST" id="Store Instances"/>
    </resource>
    <resource path="workitems">
      <method name="GET" id="Search for Workitems"/>
      <method name="POST" id="Create Workitem"/>
      <resource path="{workitem}">
        <method name="GET" id="Retrieve Workitem"/>
        <resource path="state"><method name="PUT" id="Change Workitem State"/></resource>
      </resource>
      <resource path="{subscriptionSOPClassUID}">
        <resource path="subscribers">
          <method name="POST" id="Create Subscription"/>
          <resource path="{subscriber}"><method name="DELETE" id="Delete Subscription"/></resource>
        </resource>
      </resource>
    </resource>
  </resources>
</application>"#,
        )
        .expect("parses");
        assert_eq!(
            capabilities_has_workitems_and_subscribers(&document),
            (true, true)
        );
    }

    /// A `subscribers` resource under the workitems resource directly, i.e.
    /// not under the templated SOP Class child, is not the subscription
    /// resource of PS3.4 Table CC.2.5-1.
    #[test]
    fn subscribers_must_hang_off_the_templated_sop_class_child() {
        let document = parse_wadl(
            br#"<application xmlns="http://wadl.dev.java.net/2009/02">
                  <resources base="/dicomweb">
                    <resource path="workitems">
                      <resource path="subscribers">
                        <method name="POST" id="Create Subscription"/>
                      </resource>
                    </resource>
                  </resources>
                </application>"#,
        )
        .expect("parses");
        assert_eq!(
            capabilities_has_workitems_and_subscribers(&document),
            (true, false)
        );
    }

    /// A subscribers resource without the Create Subscription POST is not a
    /// usable subscription endpoint.
    #[test]
    fn subscribers_without_the_post_is_not_counted() {
        let document = parse_wadl(
            br#"<application xmlns="http://wadl.dev.java.net/2009/02">
                  <resources base="/dicomweb">
                    <resource path="workitems">
                      <resource path="{subscriptionSOPClassUID}">
                        <resource path="subscribers">
                          <method name="DELETE" id="Delete Subscription"/>
                        </resource>
                      </resource>
                    </resource>
                  </resources>
                </application>"#,
        )
        .expect("parses");
        assert_eq!(
            capabilities_has_workitems_and_subscribers(&document),
            (true, false)
        );
    }

    #[test]
    fn a_body_that_is_not_wadl_is_a_refused_read() {
        // The observed failure mode: an origin answering JSON. There is no
        // element to read, so the tree is empty rather than an error. A
        // document whose resources do not close is the case that is refused.
        assert!(parse_wadl(b"{\"resources\":{}}").is_ok());
        assert!(parse_wadl(b"<application><resource path=\"a\">").is_err());
        assert!(parse_wadl(b"</resource>").is_err());
        assert!(parse_wadl(&[0xFF, 0xFE, 0xFD]).is_err());
    }

    /// The performed report is ONE (0074,1216) item, and everything the
    /// standard nests lives INSIDE it.
    ///
    /// This is the shape PS3.3 Table C.30.3-1 defines, and the difference is
    /// not cosmetic: a live dcm4chee-arc 5.35.2 answers 200 to the top-level
    /// form, discards what it cannot place, and then refuses the COMPLETED
    /// transition with 409. The failure therefore appears one request after
    /// its cause, which is why the assertion below checks the PARENT and the
    /// absence of siblings rather than only the presence of the tags.
    #[test]
    fn the_performed_report_is_one_ups_performed_procedure_item() {
        let performed = performed_fixture(Some("CXR-AI"));
        let fields = vec![(TAG_TRANSACTION_UID.to_string(), "2.25.9".to_string())];
        let body = encode_performed_report(
            &fields,
            &[OutputRef::new(
                "1.2.840.10008.5.1.4.1.1.11.1",
                "2.25.100",
                "2.25.1",
            )],
            &performed,
        );
        let document = json::parse(&body).expect("the update body is valid JSON");

        // The locking UID leads, at the top level, as CC.2.6.2 requires.
        assert_eq!(document.attribute(TAG_TRANSACTION_UID), Some("2.25.9"));

        // The parent exists and is a sequence with exactly one item.
        let item = document
            .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .expect("(0074,1216) is present and holds an item");
        assert!(
            document
                .get(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
                .and_then(|seq| seq.get("Value"))
                .and_then(Json::as_array)
                .is_some_and(|items| items.len() == 1),
            "one performed procedure item"
        );

        // Every child is INSIDE the item...
        let children = [
            TAG_PERFORMED_WORKITEM_CODE_SEQUENCE,
            TAG_PERFORMED_STATION_NAME_CODE_SEQUENCE,
            TAG_OUTPUT_INFORMATION_SEQUENCE,
            TAG_PERFORMED_START_DATETIME,
            TAG_PERFORMED_END_DATETIME,
        ];
        for tag in children {
            assert!(
                item.get(tag).is_some(),
                "{tag} must be a child of (0074,1216)"
            );
        }

        // ...and NONE of them is a sibling of the locking UID, which is the
        // exact bug: the top-level form is the one dcm4chee drops.
        for tag in children {
            assert!(
                document.get(tag).is_none(),
                "{tag} must NOT also sit at the top level"
            );
        }
    }

    #[test]
    fn the_output_sequence_names_each_produced_object() {
        let performed = performed_fixture(None);
        let body = encode_performed_report(
            &[],
            &[OutputRef::new(
                "1.2.840.10008.5.1.4.1.1.11.1",
                "2.25.100",
                "2.25.1",
            )],
            &performed,
        );
        let document = json::parse(&body).expect("the body is valid JSON");
        let item = document
            .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .expect("the performed procedure item");
        let output = item
            .attribute_item(TAG_OUTPUT_INFORMATION_SEQUENCE)
            .expect("the output sequence, nested");

        assert_eq!(
            output.attribute(TAG_REFERENCED_SOP_INSTANCE_UID),
            Some("2.25.1")
        );
        assert_eq!(
            output.attribute(TAG_REFERENCED_SERIES_INSTANCE_UID),
            Some("2.25.100")
        );
        assert_eq!(
            output.attribute(TAG_REFERENCED_SOP_CLASS_UID),
            Some("1.2.840.10008.5.1.4.1.1.11.1"),
            "the output names the SOP class of what it produced"
        );
    }

    #[test]
    fn an_output_names_the_class_of_the_object_it_references() {
        // A CLAHE/upscale workitem stores a Secondary Capture, not a GSPS: the
        // (0040,4033) item must reference the derived image's own class. A
        // hard-coded GSPS class here mislabels every derived-image workitem.
        let derived_class = crate::derived::SECONDARY_CAPTURE_SOP_CLASS_UID;
        let outputs = [
            OutputRef::new(derived_class, "2.25.200", "2.25.10"),
            OutputRef::new(crate::pr::GSPS_SOP_CLASS_UID, "2.25.201", "2.25.11"),
        ];
        let body = encode_performed_report(&[], &outputs, &performed_fixture(None));
        let document = json::parse(&body).expect("the body is valid JSON");
        let items = document
            .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .expect("the performed procedure item")
            .get(TAG_OUTPUT_INFORMATION_SEQUENCE)
            .and_then(|seq| seq.get("Value"))
            .and_then(Json::as_array)
            .expect("the output sequence is an array");
        assert_eq!(items.len(), 2, "one item per output");

        assert_eq!(
            items[0].attribute(TAG_REFERENCED_SOP_CLASS_UID),
            Some(derived_class),
            "the derived image keeps its own class"
        );
        assert_eq!(
            items[0].attribute(TAG_REFERENCED_SERIES_INSTANCE_UID),
            Some("2.25.200"),
            "the derived image names its series"
        );
        assert_eq!(
            items[0].attribute(TAG_REFERENCED_SOP_INSTANCE_UID),
            Some("2.25.10")
        );
        assert_eq!(
            items[1].attribute(TAG_REFERENCED_SOP_CLASS_UID),
            Some(crate::pr::GSPS_SOP_CLASS_UID),
            "the presentation state keeps the GSPS class"
        );
        assert_eq!(
            items[1].attribute(TAG_REFERENCED_SERIES_INSTANCE_UID),
            Some("2.25.201")
        );
    }

    #[test]
    fn several_outputs_each_get_their_own_item() {
        let performed = performed_fixture(None);
        let body = encode_performed_report(
            &[],
            &[
                OutputRef::new("1.2.840.10008.5.1.4.1.1.11.1", "2.25.100", "2.25.1"),
                OutputRef::new("1.2.840.10008.5.1.4.1.1.11.1", "2.25.101", "2.25.2"),
            ],
            &performed,
        );
        let document = json::parse(&body).expect("parses");
        let items = document
            .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .and_then(|item| item.get(TAG_OUTPUT_INFORMATION_SEQUENCE))
            .and_then(|seq| seq.get("Value"))
            .and_then(Json::as_array)
            .expect("an array of items");
        assert_eq!(items.len(), 2, "one item per produced object");
    }

    #[test]
    fn both_datetimes_are_written_as_dt_values() {
        let performed = performed_fixture(None);
        let body = encode_performed_report(&[], &[], &performed);
        let document = json::parse(&body).expect("parses");
        let item = document
            .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .expect("the performed procedure item");

        assert_eq!(
            item.attribute(TAG_PERFORMED_START_DATETIME),
            Some("20261010120000")
        );
        assert_eq!(
            item.attribute(TAG_PERFORMED_END_DATETIME),
            Some("20261010123456")
        );
        // `DT` carries seconds resolution as 14 digits.
        assert_eq!("20261010120000".len(), 14, "DT is 14 characters");
    }

    #[test]
    fn a_scheduled_code_is_echoed_and_an_absent_one_is_omitted() {
        let with_code = encode_performed_report(&[], &[], &performed_fixture(Some("CXR-AI")));
        let document = json::parse(&with_code).expect("parses");
        let item = document
            .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .expect("the item");
        assert_eq!(
            item.attribute_item(TAG_PERFORMED_WORKITEM_CODE_SEQUENCE)
                .expect("the echoed code")
                .attribute(TAG_CODE_VALUE),
            Some("CXR-AI")
        );

        // A workitem that scheduled no code has nothing to echo; inventing
        // one would misreport what ran.
        let without = encode_performed_report(&[], &[], &performed_fixture(None));
        let document = json::parse(&without).expect("parses");
        let item = document
            .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .expect("the item");
        assert!(
            item.attribute_item(TAG_PERFORMED_WORKITEM_CODE_SEQUENCE)
                .is_none(),
            "no code to echo, so no sequence"
        );
        assert!(
            item.attribute_item(TAG_PERFORMED_STATION_NAME_CODE_SEQUENCE)
                .is_some(),
            "but the station is always reported"
        );
    }

    #[test]
    fn an_empty_output_list_omits_the_sequence_rather_than_emptying_it() {
        // An empty Output Information Sequence claims "ran and produced
        // nothing"; omitting it says the step named nothing. The two are not
        // the same statement, and only the second is true here.
        let body = encode_performed_report(&[], &[], &performed_fixture(None));
        let document = json::parse(&body).expect("parses");
        let item = document
            .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .expect("the item");
        assert!(
            item.get(TAG_OUTPUT_INFORMATION_SEQUENCE).is_none(),
            "no empty output sequence is invented"
        );
    }

    /// The completion payloads still carry disjoint attribute sets.
    ///
    /// PS3.18 11.7.1.4 reads only the state and the locking UID from a change
    /// state, so an output list sent there is discarded; and 11.6 is not a
    /// transition, so a state sent there is an attribute the server refuses.
    /// The nesting does not change that, and this holds it.
    #[test]
    fn the_completion_payloads_do_not_overlap() {
        let update = encode_performed_report(
            &[],
            &[OutputRef::new(
                "1.2.840.10008.5.1.4.1.1.11.1",
                "2.25.100",
                "2.25.1",
            )],
            &performed_fixture(None),
        );
        let update = json::parse(&update).expect("the update body is valid JSON");
        assert!(
            update
                .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
                .is_some(),
            "the Update carries the performed report"
        );
        assert!(
            update.attribute(TAG_UPS_STATE).is_none(),
            "the Update carries NO Procedure Step State"
        );

        let state_body = encode_attributes(&[
            (TAG_UPS_STATE.to_string(), "COMPLETED".to_string()),
            (TAG_TRANSACTION_UID.to_string(), "2.25.9".to_string()),
        ]);
        let state_body = json::parse(&state_body).expect("the state body is valid JSON");
        assert_eq!(state_body.attribute(TAG_UPS_STATE), Some("COMPLETED"));
        assert_eq!(state_body.attribute(TAG_TRANSACTION_UID), Some("2.25.9"));
        assert!(
            state_body
                .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
                .is_none(),
            "the Change State names NO performed procedure"
        );
    }

    /// Every body an Update can produce parses, including the shapes whose
    /// base object is EMPTY.
    ///
    /// An empty base is the normal case here - 11.6 carries attributes and no
    /// state - so the only thing that may make the object well-formed is the
    /// separator logic in the appenders. A leading comma would produce JSON
    /// that parses as nothing, which the live path would hit only against a
    /// real server.
    #[test]
    fn an_update_body_is_valid_json_whatever_it_carries() {
        let cases: Vec<Vec<u8>> = vec![
            // The report alone, on an empty base.
            encode_performed_report(&[], &[], &performed_fixture(None)),
            // Report with outputs, on an empty base.
            encode_performed_report(
                &[],
                &[OutputRef::new(
                    "1.2.840.10008.5.1.4.1.1.11.1",
                    "2.25.100",
                    "2.25.1",
                )],
                &performed_fixture(None),
            ),
            // Report with outputs and a preceding attribute.
            encode_performed_report(
                &[(TAG_TRANSACTION_UID.to_string(), "2.25.9".to_string())],
                &[OutputRef::new(
                    "1.2.840.10008.5.1.4.1.1.11.1",
                    "2.25.100",
                    "2.25.1",
                )],
                &performed_fixture(Some("CXR-AI")),
            ),
        ];
        for body in cases {
            let document = json::parse(&body).unwrap_or_else(|e| {
                panic!("body is not JSON ({e}): {}", String::from_utf8_lossy(&body))
            });
            let item = document
                .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
                .unwrap_or_else(|| {
                    panic!(
                        "no performed procedure item: {}",
                        String::from_utf8_lossy(&body)
                    )
                });
            assert!(
                item.get(TAG_PERFORMED_STATION_NAME_CODE_SEQUENCE).is_some(),
                "the station code is present"
            );
        }
    }

    /// A report fixture with both datetimes set, so an assertion that reads
    /// one cannot pass on an empty string.
    fn performed_fixture(code: Option<&str>) -> PerformedReport {
        PerformedReport {
            station_aet: "STATION01".to_string(),
            station_name: "Scanner One".to_string(),
            station_scheme: LOCAL_CODING_SCHEME.to_string(),
            workitem_code: code.map(|value| WorkitemCode {
                value: value.to_string(),
                scheme: LOCAL_CODING_SCHEME.to_string(),
                meaning: "AI inference".to_string(),
            }),
            started_at: "20261010120000".to_string(),
            ended_at: "20261010123456".to_string(),
        }
    }

    /// A (0074,1210) with one item per Value Type, plus one type this
    /// sidecar does not handle and one item with no Concept Name.
    ///
    /// The unknown types sit in the MIDDLE of the sequence so a reader that
    /// stops at the first skip is caught: the entries after them still have
    /// to come through.
    fn parameters_document() -> Json {
        let text = r#"{
            "00741210": {"vr": "SQ", "Value": [
                {"0040A043": {"vr": "SQ", "Value": [{"00080100": {"vr": "SH", "Value": ["CLIP_LIMIT"]}, "00080102": {"vr": "SH", "Value": ["99CLARUS"]}, "00080104": {"vr": "LO", "Value": ["Clip limit"]}}]},
                 "0040A040": {"vr": "CS", "Value": ["NUMERIC"]},
                 "0040A30A": {"vr": "DS", "Value": ["2.5\\1.0"]}},
                {"0040A043": {"vr": "SQ", "Value": [{"00080100": {"vr": "SH", "Value": ["ALGORITHM"]}, "00080102": {"vr": "SH", "Value": ["99CLARUS"]}, "00080104": {"vr": "LO", "Value": ["Algorithm"]}}]},
                 "0040A040": {"vr": "CS", "Value": ["TEXT"]},
                 "0040A160": {"vr": "UT", "Value": ["clahe"]}},
                {"0040A043": {"vr": "SQ", "Value": [{"00080100": {"vr": "SH", "Value": ["BODY_SITE"]}, "00080102": {"vr": "SH", "Value": ["99CLARUS"]}, "00080104": {"vr": "LO", "Value": ["Body site"]}}]},
                 "0040A040": {"vr": "CS", "Value": ["CODE"]},
                 "0040A168": {"vr": "SQ", "Value": [{"00080100": {"vr": "SH", "Value": ["T-D0050"]}, "00080102": {"vr": "SH", "Value": ["SRT"]}, "00080104": {"vr": "LO", "Value": ["Body site"]}}]}},
                {"0040A043": {"vr": "SQ", "Value": [{"00080100": {"vr": "SH", "Value": ["OPERATOR"]}, "00080102": {"vr": "SH", "Value": ["99CLARUS"]}, "00080104": {"vr": "LO", "Value": ["Operator"]}}]},
                 "0040A040": {"vr": "CS", "Value": ["PNAME"]},
                 "0040A123": {"vr": "PN", "Value": [{"Alphabetic": "Someone"}]}},
                {"0040A040": {"vr": "CS", "Value": ["TEXT"]},
                 "0040A160": {"vr": "UT", "Value": ["nameless"]}}
            ]}
        }"#;
        crate::json::parse(text.as_bytes()).expect("fixture parses")
    }

    /// A TEXT, a NUMERIC and a CODE come through; the unhandled Value Type
    /// and the item with no Concept Name are skipped without losing the ones
    /// that follow.
    #[test]
    fn parameters_are_flattened_by_value_type() {
        let parameters = scheduled_processing_parameters(&parameters_document());
        assert_eq!(
            parameters.len(),
            3,
            "three readable of four items: {parameters:?}"
        );

        assert_eq!(parameters[0].name, "CLIP_LIMIT");
        // The DS is a backslash-separated list; the FIRST component is the
        // parameter's value.
        assert_eq!(parameters[0].value, ParamValue::Numeric(2.5));

        assert_eq!(parameters[1].name, "ALGORITHM");
        assert_eq!(parameters[1].value, ParamValue::Text("clahe".to_string()));

        assert_eq!(parameters[2].name, "BODY_SITE");
        assert_eq!(
            parameters[2].value,
            ParamValue::Code {
                scheme: "SRT".to_string(),
                value: "T-D0050".to_string()
            }
        );
    }

    /// A workitem without the sequence carries no parameters, which is what a
    /// script reads as "use your own defaults".
    #[test]
    fn an_absent_parameter_sequence_is_empty() {
        let document =
            crate::json::parse(br#"{"00080018": {"Value": ["1.2.3"]}}"#).expect("parses");
        assert!(scheduled_processing_parameters(&document).is_empty());
    }

    /// A NUMERIC whose DS does not start with a finite number is skipped
    /// rather than carried as a zero the script would act on.
    #[test]
    fn a_numeric_parameter_that_is_not_a_number_is_skipped() {
        let text = r#"{"00741210": {"vr": "SQ", "Value": [
            {"0040A043": {"vr": "SQ", "Value": [{"00080100": {"vr": "SH", "Value": ["SCALE"]}}]},
             "0040A040": {"vr": "CS", "Value": ["NUMERIC"]},
             "0040A30A": {"vr": "DS", "Value": ["not a number"]}}
        ]}}"#;
        let document = crate::json::parse(text.as_bytes()).expect("parses");
        assert!(scheduled_processing_parameters(&document).is_empty());
    }

    /// `WorkItem::from_json` carries the flattened list, so a claim hands
    /// the performer its parameters without a second read.
    #[test]
    fn a_workitem_carries_its_parameters() {
        let text = r#"{
            "00080018": {"vr": "UI", "Value": ["1.2.826.0.1.3680043.10.1234.9001"]},
            "00741210": {"vr": "SQ", "Value": [
                {"0040A043": {"vr": "SQ", "Value": [{"00080100": {"vr": "SH", "Value": ["SCALE"]}}]},
                 "0040A040": {"vr": "CS", "Value": ["NUMERIC"]},
                 "0040A30A": {"vr": "DS", "Value": ["2"]}}
            ]}
        }"#;
        let workitem = WorkItem::from_json(&crate::json::parse(text.as_bytes()).expect("parses"))
            .expect("a workitem with a UID");
        assert_eq!(workitem.scheduled_processing_parameters.len(), 1);
        assert_eq!(workitem.scheduled_processing_parameters[0].name, "SCALE");
        assert_eq!(
            workitem.scheduled_processing_parameters[0].value,
            ParamValue::Numeric(2.0)
        );
    }

    /// One parameter item, built so a test can state only the carriage it is
    /// about.
    fn one_parameter(item: &str) -> Vec<Parameter> {
        let text = format!(
            r#"{{"00741210": {{"vr": "SQ", "Value": [
                {{"0040A043": {{"vr": "SQ", "Value": [{{"00080100": {{"vr": "SH", "Value": ["P"]}}}}]}},
                 {item}}}
            ]}}}}"#
        );
        scheduled_processing_parameters(&crate::json::parse(text.as_bytes()).expect("parses"))
    }

    /// PS3.3 Table 10-2: Value Type CODE selects Concept Code Sequence
    /// (0040,A168), a single item carrying the Code Sequence Macro. That is
    /// the carriage every product minting a compliant workitem uses - the
    /// bare (0008,0100) on the content item is NOT it.
    #[test]
    fn a_code_parameter_is_read_from_the_concept_code_sequence() {
        let parameters = one_parameter(
            r#""0040A040": {"vr": "CS", "Value": ["CODE"]},
              "0040A168": {"vr": "SQ", "Value": [
                  {"00080100": {"vr": "SH", "Value": ["T-D0050"]},
                   "00080102": {"vr": "SH", "Value": ["SRT"]}}
              ]}"#,
        );
        assert_eq!(parameters.len(), 1, "{parameters:?}");
        assert_eq!(
            parameters[0].value,
            ParamValue::Code {
                scheme: "SRT".to_string(),
                value: "T-D0050".to_string()
            }
        );
    }

    /// A bare (0008,0100) on the content item is NOT the CODE carriage, so
    /// it is skipped rather than honoured: one value attribute per item
    /// (PS3.3 Table 10-2) and the selected one for CODE is the sequence.
    #[test]
    fn a_bare_code_value_without_the_sequence_is_skipped() {
        let parameters = one_parameter(
            r#""0040A040": {"vr": "CS", "Value": ["CODE"]},
              "00080100": {"vr": "SH", "Value": ["T-D0051"]},
              "00080102": {"vr": "SH", "Value": ["SRT"]}"#,
        );
        assert!(parameters.is_empty(), "{parameters:?}");
    }

    /// Neither carriage present is a CODE with no code: skipped, not invented.
    #[test]
    fn a_code_parameter_without_any_carriage_is_skipped() {
        let parameters = one_parameter(r#""0040A040": {"vr": "CS", "Value": ["CODE"]}"#);
        assert!(parameters.is_empty(), "{parameters:?}");
    }

    /// Value Type DATETIME selects (0040,A120), carried verbatim. The DT
    /// precision and any range are the requester's statement, so they are
    /// handed on unchanged rather than parsed into an instant.
    #[test]
    fn a_datetime_parameter_is_carried_verbatim() {
        let parameters = one_parameter(
            r#""0040A040": {"vr": "CS", "Value": ["DATETIME"]},
              "0040A120": {"vr": "DT", "Value": ["20261109101530.123+0300"]}"#,
        );
        assert_eq!(parameters.len(), 1, "{parameters:?}");
        assert_eq!(
            parameters[0].value,
            ParamValue::DateTime("20261109101530.123+0300".to_string())
        );
    }

    /// DATE, TIME, UIDREF and PNAME are documented as unsupported and are
    /// skipped rather than coerced into one of the supported shapes.
    #[test]
    fn the_unsupported_value_types_are_skipped() {
        for (value_type, attribute) in [
            ("DATE", r#""0040A121": {"vr": "DA", "Value": ["20261109"]}"#),
            ("TIME", r#""0040A122": {"vr": "TM", "Value": ["101530"]}"#),
            ("UIDREF", r#""0040A124": {"vr": "UI", "Value": ["1.2.3"]}"#),
            (
                "PNAME",
                r#""0040A123": {"vr": "PN", "Value": [{"Alphabetic": "Someone"}]}"#,
            ),
        ] {
            let parameters = one_parameter(&format!(
                r#""0040A040": {{"vr": "CS", "Value": ["{value_type}"]}}, {attribute}"#
            ));
            assert!(
                parameters.is_empty(),
                "{value_type} must be skipped, got {parameters:?}"
            );
        }
    }
}
