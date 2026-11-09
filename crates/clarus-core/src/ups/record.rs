// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! UPS workitem CBOR record (`workitems/{uid}.cbor`).
//!
//! the UPS-RS workitem store moves from SQLite to a per-workitem CBOR
//! file, one record per `{uid}.cbor` (C-014). This module defines the record
//! shape and its codec, modelled on the study manifest (`manifest/record.rs`)
//! and using the same zero-allocation custom CBOR codec (`crate::cbor`) -
//! no new crates.
//!
//! # CBOR shape
//!
//! ```cbor
//! {
//!   1: <id i64>,
//!   2: "<workitem_uid>",
//!   3: "<workflow_state>",
//!   4: "<modality>",
//!   5: "<station_aet>",   // present only when Some
//!   ... (optional string keys 5..=16 present only when Some)
//! }
//! ```
//!
//! Required keys 2-4 are always written; `id` (key 1) and optional keys 5-16
//! are written only when their value is `Some`. Decoding is tolerant: a
//! missing optional key yields `None`, an unknown key is skipped. This keeps
//! the payload compact and forwards/backwards tolerant.

use crate::cbor::{CborError, CborReader, CborToken, CborWriter};
use std::fmt;

// Map keys (numeric, following the manifest idiom).
const K_ID: u64 = 1;
const K_WORKITEM_UID: u64 = 2;
const K_WORKFLOW_STATE: u64 = 3;
const K_MODALITY: u64 = 4;
const K_STATION_AET: u64 = 5;
const K_STATION_NAME: u64 = 6;
const K_SPS_START_DATE: u64 = 7;
const K_SPS_START_TIME: u64 = 8;
const K_PATIENT_NAME: u64 = 9;
const K_PATIENT_ID: u64 = 10;
const K_ACCESSION_NUMBER: u64 = 11;
const K_REQUESTING_PHYSICIAN: u64 = 12;
const K_SPS_PRIORITY: u64 = 13;
const K_TRANSACTION_UID: u64 = 14;
const K_CREATED_AT: u64 = 15;
const K_UPDATED_AT: u64 = 16;
// Added with the standard-first UPS shape (1709). Append-only: keys below 17
// keep their meaning, so a record written by an older build still decodes.
const K_STEP_LABEL: u64 = 17;
const K_INPUT_READINESS: u64 = 18;
const K_SPS_START_DATETIME: u64 = 19;
const K_INPUT_STUDY_UID: u64 = 20;
const K_REASON_FOR_CANCELLATION: u64 = 21;
const K_CANCELLATION_DATETIME: u64 = 22;
const K_PATIENT_ID_ISSUER: u64 = 23;
const K_SCHEDULED_WORKITEM_CODE: u64 = 24;
// Added with the UPS Update transaction (2709). Append-only as above.
const K_OUTPUT_INFORMATION: u64 = 25;
const K_PERFORMED_WORKITEM_CODE: u64 = 26;
const K_PERFORMED_STATION_NAME_CODE: u64 = 27;
// Added with the station standard-first pass (0910). Append-only as above.
const K_REQUESTED_PROCEDURE_ID: u64 = 28;
// Added with the performed-procedure nesting pass (1010). Append-only as
// above; these two carry (0040,4050)/(0040,4051), which PS3.3 Table C.30.3-1
// places inside (0074,1216).
const K_PERFORMED_START_DATETIME: u64 = 29;
const K_PERFORMED_END_DATETIME: u64 = 30;
// Added with the task-parameters pass (0811). Append-only as above: the
// decoder walks keys in ascending order and stops when it meets one it does
// not know, so a new key must be written LAST and must never renumber.
const K_SCHEDULED_PROCESSING_PARAMETERS: u64 = 31;
const K_REASON_FOR_REQUESTED_PROCEDURE: u64 = 32;

/// A single UPS workitem, as persisted in `workitems/{uid}.cbor`.
///
/// Field set mirrors `clarus_ports::models::WorkItem`. The server
/// converts between this core record and the ports `WorkItem` at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkItemRecord {
    /// Legacy row identifier carried in the on-disk record.
    ///
    /// The CBOR codec still writes and reads this as key 1 when present, so
    /// the field is part of the persisted format and must stay. New records
    /// are written with `None` - the workitem UID is the real key.
    pub id: Option<i64>,
    /// (0020,000D) Study Instance UID - the record key and file stem.
    pub workitem_uid: String,
    /// PS3.18 11.3 workflow state (SCHEDULED/IN PROGRESS/COMPLETED/CANCELED/SUSPENDED).
    pub workflow_state: String,
    /// (0008,0060) Modality.
    ///
    /// The modality of the input study, written when the workitem is created
    /// from a stored study; empty for an externally created workitem and for
    /// the pre-catalog creation path, where no modality is in scope.
    ///
    /// UPS itself has no Modality attribute (PS3.4 Table CC.2.5-3 has no
    /// Modality row), so the value is a vendor extension declared in the
    /// conformance statements. Key 4 is append-only: a record written by a
    /// build that always left this empty still decodes.
    pub modality: String,
    /// (0040,0001) Scheduled Station AE Title.
    pub station_aet: Option<String>,
    /// (0040,0010) Scheduled Station Name.
    pub station_name: Option<String>,
    /// (0040,0002) Scheduled Procedure Step Start Date.
    pub procedure_step_start_date: Option<String>,
    /// (0040,0003) Scheduled Procedure Step Start Time.
    pub procedure_step_start_time: Option<String>,
    /// (0010,0010) Patient Name.
    pub patient_name: Option<String>,
    /// (0010,0020) Patient ID.
    pub patient_id: Option<String>,
    /// (0008,0050) Accession Number.
    pub accession_number: Option<String>,
    /// (0032,1032) Requesting Physician.
    pub requesting_physician: Option<String>,
    /// (0074,1200) Scheduled Procedure Step Priority.
    pub sps_priority: Option<String>,
    /// (0008,1195) Transaction UID for optimistic locking (G-08 / C-014 CAS).
    pub transaction_uid: Option<String>,
    /// Ingestion timestamp (ISO string; SQLite `datetime('now')` fmt).
    pub created_at: Option<String>,
    /// Last-update timestamp (ISO string).
    pub updated_at: Option<String>,
    /// (0074,1204) Procedure Step Label - type 1 at N-CREATE, and a matching
    /// key for UPS-RS search (PS3.4 Table CC.2.5-3).
    pub step_label: Option<String>,
    /// (0040,4041) Input Readiness State - type 1 at N-CREATE.
    pub input_readiness: Option<String>,
    /// (0040,4005) Scheduled Procedure Step Start DateTime - type 1 at
    /// N-CREATE. Carried in one field because the VR is DT, not the DA/TM
    /// pair the station attributes use.
    pub sps_start_datetime: Option<String>,
    /// (0020,000D) Study Instance UID of the input study.
    ///
    /// The real study the workitem refers to. Distinct from
    /// `workitem_uid`, which is the identity of the workitem itself - the two
    /// were conflated before 1709 and the study UID was unreachable.
    pub input_study_uid: Option<String>,
    /// (0074,1238) Reason For Cancellation, as carried by the Change State
    /// request that canceled the workitem (PS3.4 Table CC.2.5-3).
    pub reason_for_cancellation: Option<String>,
    /// (0040,4052) Procedure Step Cancellation DateTime - SCP-filled when the
    /// workitem enters CANCELED (PS3.4 CC.2.5-3).
    pub cancellation_datetime: Option<String>,
    /// (0010,0021) Issuer of Patient ID.
    ///
    /// AIW-I 4.80.4.1.2.1 requires an issuer for every Patient ID a request
    /// carries. The record keeps `None` for "the site declares no issuer"
    /// and the wire shape renders that as a zero-length element, which is
    /// the standard's own way to say the same thing (PS3.3 C.30.1): an
    /// absent issuer and an empty one are one statement, not two.
    pub patient_id_issuer: Option<String>,
    /// (0040,4018) Scheduled Workitem Code Sequence, one item.
    ///
    /// The code the requester chose for the step. AIW-I 50.4.1.2 leaves the
    /// values to deployment agreement, and 4.84.4.1.2.1 has the performer
    /// echo the code back as (0040,4019) "if present" - which is why the
    /// requester has to send one at all.
    pub scheduled_workitem_code: Option<WorkItemCode>,
    /// (0040,4033) Output Information Sequence - the objects the performer
    /// produced, each item a (0008,1150)/(0008,1155) reference pair.
    ///
    /// This is where a COMPLETED workitem states its results (PS3.4 Table
    /// CC.2.5-3): a consumer that wants the output reads the references here
    /// rather than searching the study for it. Written by the Update
    /// transaction (PS3.18 11.6), which is AIW-I 4.84.4.1.2.1's "all created
    /// instances listed ... before the completed transition".
    ///
    /// `None` and an empty list are DIFFERENT statements, and the codec keeps
    /// them apart: `None` means the sequence was never written (the update
    /// did not mention outputs, or no update happened), while an empty list
    /// would mean the performer wrote one and put nothing in it. The update
    /// handler refuses the empty list for exactly that reason, so in practice
    /// this is either `None` or non-empty.
    pub output_information: Option<Vec<OutputReference>>,
    /// (0040,4019) Performed Workitem Code Sequence, one item.
    ///
    /// AIW-I 4.84.4.1.2.1: the performer echoes the scheduled code "if
    /// present". Separate from `scheduled_workitem_code` because they are
    /// different statements - what was ASKED for and what was DONE - and a
    /// performer may report a code of its own.
    pub performed_workitem_code: Option<WorkItemCode>,
    /// (0040,4028) Performed Station Name Code Sequence, one item.
    ///
    /// The station that ran the step, as the performer names it.
    pub performed_station_name_code: Option<WorkItemCode>,
    /// (0040,A370) Referenced Request Sequence > (0040,1001) Requested
    /// Procedure ID - a REQUIRED matching key (PS3.4 Table CC.2.5-3, R).
    ///
    /// PS3.4 writes the key as a path through the Referenced Request Sequence
    /// and the sequence segment lives in the parameter NAME on the wire; the
    /// record keeps the one scalar it carries. Written at key 28, so a record
    /// from a build that predates this field still decodes - the same
    /// append-only discipline every K_* key above follows.
    pub requested_procedure_id: Option<String>,
    /// (0040,4050) Performed Procedure Step Start DateTime - when the
    /// performer began, as it reports it (PS3.3 Table C.30.3-1, inside
    /// (0074,1216)).
    ///
    /// Written by the Update transaction from the same item that carries the
    /// output list, so the two always describe one performance. `None` means
    /// no performer has reported a start, which is the state of every
    /// workitem before its first Update.
    pub performed_start_datetime: Option<String>,
    /// (0040,4051) Performed Procedure Step End DateTime - when the performer
    /// finished. Same item and same discipline as the start above.
    pub performed_end_datetime: Option<String>,
    /// (0074,1210) Scheduled Processing Parameters Sequence, stored as the
    /// canonical JSON of the element (key 31).
    ///
    /// The value is OPAQUE here: the record keeps what the Create or Update
    /// sent, and the JSON render returns it byte-for-byte. PS3.3 Table
    /// C.30.2-1 makes the sequence a list of Content Items, whose meaning
    /// belongs to the performing system - the server has no vocabulary for
    /// their Concept Names and must not invent one. Storing the serialized
    /// element is what makes "store as-is, return unchanged" true without a
    /// typed model of every future parameter.
    pub scheduled_processing_parameters: Option<String>,
    /// (0040,1002) Reason for the Requested Procedure (key 32).
    ///
    /// PS3.4 Table CC.2.5-3 nests this inside (0040,A370) Referenced Request
    /// Sequence, and the SEQUENCE itself is "Not allowed" at N-SET while this
    /// one nested attribute is typed 3/3 - so the Update transaction can
    /// change the reason and nothing else about the request.
    pub reason_for_requested_procedure: Option<String>,
}

/// One item of (0040,4033) Output Information Sequence.
///
/// The reference pair PS3.3 C.12.1 defines for pointing at another SOP
/// instance: (0008,1150) Referenced SOP Class UID and (0008,1155)
/// Referenced SOP Instance UID. A reference with an empty instance UID names
/// nothing and is not stored. The Referenced Instances and Access Macro
/// (PS3.3 Table 10-3b) also types Series Instance UID as 1C, so it is carried
/// when the performer names it and empty otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputReference {
    pub sop_class_uid: String,
    pub series_instance_uid: String,
    pub sop_instance_uid: String,
}

/// One code from a `CodeSequenceMacro`: value, scheme, meaning.
///
/// PS3.3 Table 8.8-1 makes Coding Scheme Designator type 1 and Coding Scheme
/// Version type 1C (required only for a versioned scheme); this type carries
/// the two the requester always knows. A code with an empty value is not a
/// code and is not stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemCode {
    pub value: String,
    pub scheme: String,
    pub meaning: String,
}

/// Errors produced while encoding/decoding a workitem record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItemError {
    /// Underlying CBOR codec failure.
    Cbor(CborError),
    /// A value did not match the shape expected for its map key.
    UnexpectedValue(&'static str),
    /// The required record key is missing or the envelope is malformed.
    Malformed(&'static str),
}

impl fmt::Display for WorkItemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkItemError::Cbor(e) => write!(f, "workitem CBOR error: {e}"),
            WorkItemError::UnexpectedValue(expected) => {
                write!(f, "unexpected workitem value, expected {expected}")
            }
            WorkItemError::Malformed(why) => write!(f, "malformed workitem: {why}"),
        }
    }
}

impl std::error::Error for WorkItemError {}

impl From<CborError> for WorkItemError {
    fn from(e: CborError) -> Self {
        WorkItemError::Cbor(e)
    }
}

impl WorkItemRecord {
    /// Encode this record into `buf`, returning the number of bytes written.
    pub fn encode_to<'a>(&self, writer: &mut CborWriter<'a>) -> Result<(), WorkItemError> {
        // Count the optional string keys that are Some.
        let optional_present = [
            self.station_aet.is_some(),
            self.station_name.is_some(),
            self.procedure_step_start_date.is_some(),
            self.procedure_step_start_time.is_some(),
            self.patient_name.is_some(),
            self.patient_id.is_some(),
            self.accession_number.is_some(),
            self.requesting_physician.is_some(),
            self.sps_priority.is_some(),
            self.transaction_uid.is_some(),
            self.created_at.is_some(),
            self.updated_at.is_some(),
            self.step_label.is_some(),
            self.input_readiness.is_some(),
            self.sps_start_datetime.is_some(),
            self.input_study_uid.is_some(),
            self.reason_for_cancellation.is_some(),
            self.cancellation_datetime.is_some(),
            self.patient_id_issuer.is_some(),
            self.scheduled_workitem_code.is_some(),
            self.output_information.is_some(),
            self.performed_workitem_code.is_some(),
            self.performed_station_name_code.is_some(),
            self.requested_procedure_id.is_some(),
            self.performed_start_datetime.is_some(),
            self.performed_end_datetime.is_some(),
            self.scheduled_processing_parameters.is_some(),
            self.reason_for_requested_procedure.is_some(),
        ]
        .iter()
        .filter(|p| **p)
        .count();

        // Fields: optional `id`, 3 required strings, plus the optional strings.
        let id_present = self.id.is_some();
        let entry_count = (id_present as u64) + 3 + optional_present as u64;
        writer.write_map_header(entry_count)?;

        // id (optional, written only when present).
        if let Some(id) = self.id {
            writer.write_u64(K_ID)?;
            writer.write_u64(id as u64)?;
        }

        // Required keys.
        writer.write_u64(K_WORKITEM_UID)?;
        writer.write_text(&self.workitem_uid)?;
        writer.write_u64(K_WORKFLOW_STATE)?;
        writer.write_text(&self.workflow_state)?;
        writer.write_u64(K_MODALITY)?;
        writer.write_text(&self.modality)?;

        // Optional string keys.
        write_opt_str(writer, K_STATION_AET, self.station_aet.as_deref())?;
        write_opt_str(writer, K_STATION_NAME, self.station_name.as_deref())?;
        write_opt_str(
            writer,
            K_SPS_START_DATE,
            self.procedure_step_start_date.as_deref(),
        )?;
        write_opt_str(
            writer,
            K_SPS_START_TIME,
            self.procedure_step_start_time.as_deref(),
        )?;
        write_opt_str(writer, K_PATIENT_NAME, self.patient_name.as_deref())?;
        write_opt_str(writer, K_PATIENT_ID, self.patient_id.as_deref())?;
        write_opt_str(writer, K_ACCESSION_NUMBER, self.accession_number.as_deref())?;
        write_opt_str(
            writer,
            K_REQUESTING_PHYSICIAN,
            self.requesting_physician.as_deref(),
        )?;
        write_opt_str(writer, K_SPS_PRIORITY, self.sps_priority.as_deref())?;
        write_opt_str(writer, K_TRANSACTION_UID, self.transaction_uid.as_deref())?;
        write_opt_str(writer, K_CREATED_AT, self.created_at.as_deref())?;
        write_opt_str(writer, K_UPDATED_AT, self.updated_at.as_deref())?;
        write_opt_str(writer, K_STEP_LABEL, self.step_label.as_deref())?;
        write_opt_str(writer, K_INPUT_READINESS, self.input_readiness.as_deref())?;
        write_opt_str(
            writer,
            K_SPS_START_DATETIME,
            self.sps_start_datetime.as_deref(),
        )?;
        write_opt_str(writer, K_INPUT_STUDY_UID, self.input_study_uid.as_deref())?;
        write_opt_str(
            writer,
            K_REASON_FOR_CANCELLATION,
            self.reason_for_cancellation.as_deref(),
        )?;
        write_opt_str(
            writer,
            K_CANCELLATION_DATETIME,
            self.cancellation_datetime.as_deref(),
        )?;
        write_opt_str(
            writer,
            K_PATIENT_ID_ISSUER,
            self.patient_id_issuer.as_deref(),
        )?;
        // The code sequence is written as its Code Value followed by the
        // other two parts, so an older reader that skips the unknown key
        // skips one value rather than a nested map. The separator guard lives
        // in `write_opt_code`: doing the join here bypassed it, so a Code
        // Meaning containing the unit separator decoded into a different code
        // than the caller wrote - silently, in the audit trail.
        write_opt_code(
            writer,
            K_SCHEDULED_WORKITEM_CODE,
            self.scheduled_workitem_code.as_ref(),
        )?;
        // The performed report is the same three-part join as the scheduled
        // code, so an older reader skips one value rather than a nested map.
        write_opt_code(
            writer,
            K_PERFORMED_WORKITEM_CODE,
            self.performed_workitem_code.as_ref(),
        )?;
        write_opt_code(
            writer,
            K_PERFORMED_STATION_NAME_CODE,
            self.performed_station_name_code.as_ref(),
        )?;
        // The output list is written as ONE text value with the reference
        // pairs separated by the unit separator: a single value keeps the
        // append-only, one-key-one-value shape every other key here has, so a
        // reader that does not know the key skips exactly one value.
        if let Some(outputs) = &self.output_information {
            writer.write_u64(K_OUTPUT_INFORMATION)?;
            let mut joined = String::new();
            for (index, out) in outputs.iter().enumerate() {
                // The separators ARE the structure of this value: a UID
                // carrying one would split into extra fields on read, so the
                // record would decode as something the writer never wrote.
                // Check the COMPONENTS, never `joined` - that buffer holds the
                // separators by construction and would always trip. That is a
                // caller error, not a storage condition, and it must not be
                // stored silently.
                if has_unit_separator(&out.sop_class_uid)
                    || has_unit_separator(&out.series_instance_uid)
                    || has_unit_separator(&out.sop_instance_uid)
                {
                    return Err(WorkItemError::UnexpectedValue(
                        "unit separator inside an output reference",
                    ));
                }
                if index > 0 {
                    joined.push('\u{1e}');
                }
                joined.push_str(&out.sop_class_uid);
                joined.push('\u{1f}');
                joined.push_str(&out.series_instance_uid);
                joined.push('\u{1f}');
                joined.push_str(&out.sop_instance_uid);
            }
            writer.write_text(&joined)?;
        }
        // Key 28 goes LAST so the map keys stay in ascending order, which is
        // the shape this codec's reader walks (and the manifest's
        // binary-search convention). Appending a key in the middle would emit
        // 28 before 25 and break the decode. Keys 29 and 30 were added later
        // still, so they follow 28 and keep the ascending order the reader
        // relies on.
        write_opt_str(
            writer,
            K_REQUESTED_PROCEDURE_ID,
            self.requested_procedure_id.as_deref(),
        )?;
        write_opt_str(
            writer,
            K_PERFORMED_START_DATETIME,
            self.performed_start_datetime.as_deref(),
        )?;
        write_opt_str(
            writer,
            K_PERFORMED_END_DATETIME,
            self.performed_end_datetime.as_deref(),
        )?;
        // Keys 31 and 32 close the set. They are written here rather than
        // beside their conceptual neighbours for the same reason as the rest:
        // the map must ascend.
        write_opt_str(
            writer,
            K_SCHEDULED_PROCESSING_PARAMETERS,
            self.scheduled_processing_parameters.as_deref(),
        )?;
        write_opt_str(
            writer,
            K_REASON_FOR_REQUESTED_PROCEDURE,
            self.reason_for_requested_procedure.as_deref(),
        )?;

        Ok(())
    }

    /// Encode into a freshly allocated `Vec<u8>`.
    ///
    /// The buffer doubles from small until the record fits, bounded by
    /// [`MAX_RECORD_BYTES`]. A fixed 4096-byte buffer was a silent ceiling: a
    /// record with ~20 output references, or a couple of long free-text fields,
    /// exceeded it, and `write_atomic` then failed for that workitem forever.
    pub fn encode(&self) -> Result<Vec<u8>, WorkItemError> {
        let mut cap = 512;
        loop {
            let mut buf = vec![0u8; cap];
            let written = {
                let mut writer = CborWriter::new(&mut buf);
                match self.encode_to(&mut writer) {
                    Ok(()) => writer.written(),
                    Err(WorkItemError::Cbor(CborError::BufferFull)) if cap < MAX_RECORD_BYTES => {
                        cap = cap.saturating_mul(2).min(MAX_RECORD_BYTES);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            };
            buf.truncate(written);
            return Ok(buf);
        }
    }

    /// Parse a workitem record from `buf`.
    pub fn decode(buf: &[u8]) -> Result<WorkItemRecord, WorkItemError> {
        let mut reader = CborReader::new(buf);
        let token = reader.next()?;
        let CborToken::MapStart(n) = token else {
            return Err(WorkItemError::Malformed("top-level record is not a map"));
        };

        let mut rec = WorkItemRecord::default();
        // Keys 2 and 3 are the record's identity: without them the decoded
        // value is a nameless workitem in no state, which every caller would
        // then have to re-validate. Reject it here, where the cause is known.
        // Key 4 (modality) stays optional - it is a vendor extension.
        let mut seen_uid = false;
        let mut seen_state = false;
        for _ in 0..n {
            let key = read_key(&mut reader)?;
            match key {
                K_ID => {
                    let CborToken::UInt(v) = reader.next()? else {
                        return Err(WorkItemError::UnexpectedValue("u64 id"));
                    };
                    rec.id = Some(v as i64);
                }
                K_WORKITEM_UID => {
                    rec.workitem_uid = read_text(&mut reader, "workitem_uid")?;
                    seen_uid = true;
                }
                K_WORKFLOW_STATE => {
                    rec.workflow_state = read_text(&mut reader, "workflow_state")?;
                    seen_state = true;
                }
                K_MODALITY => {
                    rec.modality = read_text(&mut reader, "modality")?;
                }
                K_STATION_AET => rec.station_aet = Some(read_text(&mut reader, "station_aet")?),
                K_STATION_NAME => rec.station_name = Some(read_text(&mut reader, "station_name")?),
                K_SPS_START_DATE => {
                    rec.procedure_step_start_date = Some(read_text(&mut reader, "start_date")?)
                }
                K_SPS_START_TIME => {
                    rec.procedure_step_start_time = Some(read_text(&mut reader, "start_time")?)
                }
                K_PATIENT_NAME => rec.patient_name = Some(read_text(&mut reader, "patient_name")?),
                K_PATIENT_ID => rec.patient_id = Some(read_text(&mut reader, "patient_id")?),
                K_ACCESSION_NUMBER => {
                    rec.accession_number = Some(read_text(&mut reader, "accession_number")?)
                }
                K_REQUESTING_PHYSICIAN => {
                    rec.requesting_physician = Some(read_text(&mut reader, "requesting_physician")?)
                }
                K_SPS_PRIORITY => rec.sps_priority = Some(read_text(&mut reader, "sps_priority")?),
                K_TRANSACTION_UID => {
                    rec.transaction_uid = Some(read_text(&mut reader, "transaction_uid")?)
                }
                K_CREATED_AT => rec.created_at = Some(read_text(&mut reader, "created_at")?),
                K_UPDATED_AT => rec.updated_at = Some(read_text(&mut reader, "updated_at")?),
                K_STEP_LABEL => rec.step_label = Some(read_text(&mut reader, "step_label")?),
                K_INPUT_READINESS => {
                    rec.input_readiness = Some(read_text(&mut reader, "input_readiness")?)
                }
                K_SPS_START_DATETIME => {
                    rec.sps_start_datetime = Some(read_text(&mut reader, "sps_start_datetime")?)
                }
                K_INPUT_STUDY_UID => {
                    rec.input_study_uid = Some(read_text(&mut reader, "input_study_uid")?)
                }
                K_REASON_FOR_CANCELLATION => {
                    rec.reason_for_cancellation =
                        Some(read_text(&mut reader, "reason_for_cancellation")?)
                }
                K_CANCELLATION_DATETIME => {
                    rec.cancellation_datetime =
                        Some(read_text(&mut reader, "cancellation_datetime")?)
                }
                K_PATIENT_ID_ISSUER => {
                    rec.patient_id_issuer = Some(read_text(&mut reader, "patient_id_issuer")?)
                }
                K_SCHEDULED_WORKITEM_CODE => {
                    let joined = read_text(&mut reader, "scheduled_workitem_code")?;
                    rec.scheduled_workitem_code = decode_code(&joined);
                }
                K_PERFORMED_WORKITEM_CODE => {
                    let joined = read_text(&mut reader, "performed_workitem_code")?;
                    rec.performed_workitem_code = decode_code(&joined);
                }
                K_PERFORMED_STATION_NAME_CODE => {
                    let joined = read_text(&mut reader, "performed_station_name_code")?;
                    rec.performed_station_name_code = decode_code(&joined);
                }
                K_REQUESTED_PROCEDURE_ID => {
                    rec.requested_procedure_id =
                        Some(read_text(&mut reader, "requested_procedure_id")?)
                }
                K_PERFORMED_START_DATETIME => {
                    rec.performed_start_datetime =
                        Some(read_text(&mut reader, "performed_start_datetime")?)
                }
                K_PERFORMED_END_DATETIME => {
                    rec.performed_end_datetime =
                        Some(read_text(&mut reader, "performed_end_datetime")?)
                }
                K_SCHEDULED_PROCESSING_PARAMETERS => {
                    rec.scheduled_processing_parameters =
                        Some(read_text(&mut reader, "scheduled_processing_parameters")?)
                }
                K_REASON_FOR_REQUESTED_PROCEDURE => {
                    rec.reason_for_requested_procedure =
                        Some(read_text(&mut reader, "reason_for_requested_procedure")?)
                }
                K_OUTPUT_INFORMATION => {
                    let joined = read_text(&mut reader, "output_information")?;
                    // An empty value decodes to an EMPTY list, not `None`:
                    // the key being present is the statement "the performer
                    // wrote an output list", and losing that distinction
                    // would make "wrote nothing" and "wrote never" the same
                    // record.
                    let mut outputs = Vec::new();
                    for item in joined.split('\u{1e}') {
                        if item.is_empty() {
                            continue;
                        }
                        let parts: Vec<&str> = item.split('\u{1f}').collect();
                        // Three fields is the current shape (class, series,
                        // instance); two is the pre-series record shape
                        // (class, instance) with the series unknown. The
                        // count, not the emptiness of a field, tells them
                        // apart: a current record legitimately carries an
                        // empty series when the performer named none.
                        let (class, series, instance) = match parts.as_slice() {
                            [class, series, instance] => (*class, *series, *instance),
                            [class, instance, ..] => (*class, "", *instance),
                            _ => ("", "", ""),
                        };
                        outputs.push(OutputReference {
                            sop_class_uid: class.to_string(),
                            series_instance_uid: series.to_string(),
                            sop_instance_uid: instance.to_string(),
                        });
                    }
                    rec.output_information = Some(outputs);
                }
                // Unknown key: skip the value (tolerant decode).
                _ => reader.skip_value()?,
            }
        }

        if !reader.is_empty() {
            return Err(WorkItemError::Cbor(CborError::TrailingData));
        }
        if !seen_uid {
            return Err(WorkItemError::Malformed("missing workitem_uid (key 2)"));
        }
        if !seen_state {
            return Err(WorkItemError::Malformed("missing workflow_state (key 3)"));
        }
        Ok(rec)
    }
}

/// Ceiling on one encoded workitem record.
///
/// Well above any real record (a workitem is a few KiB) and still bounded, so
/// a record whose writer keeps reporting `BufferFull` fails instead of growing
/// until the allocator does.
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// Write an optional string map entry (key only when `Some`).
fn write_opt_str(
    writer: &mut CborWriter<'_>,
    key: u64,
    value: Option<&str>,
) -> Result<(), WorkItemError> {
    if let Some(v) = value {
        writer.write_u64(key)?;
        writer.write_text(v)?;
    }
    Ok(())
}

/// Whether a text component carries one of the join separators.
///
/// Only the COMPONENTS are ever tested: the joined buffer holds the separators
/// by construction, so testing it would reject every record.
fn has_unit_separator(s: &str) -> bool {
    s.contains('\u{1e}') || s.contains('\u{1f}')
}

/// Write an optional code sequence, joined with unit separators.
///
/// One key, one text value - the same shape [`write_opt_str`] writes, so a
/// reader that does not know the key skips a single value.
fn write_opt_code(
    writer: &mut CborWriter<'_>,
    key: u64,
    code: Option<&WorkItemCode>,
) -> Result<(), WorkItemError> {
    if let Some(code) = code {
        // Same rule as the output list: the unit separators are the structure
        // of this VALUE. A Code Meaning carrying one would decode into a
        // different code than the caller wrote - silently, which is the worst
        // kind of data corruption in an audit trail.
        if [&code.value, &code.scheme, &code.meaning]
            .iter()
            .any(|s| has_unit_separator(s))
        {
            return Err(WorkItemError::UnexpectedValue(
                "unit separator inside a code sequence value",
            ));
        }
        writer.write_u64(key)?;
        let joined = format!("{}\u{1f}{}\u{1f}{}", code.value, code.scheme, code.meaning);
        writer.write_text(&joined)?;
    }
    Ok(())
}

/// Split a joined code value back into a [`WorkItemCode`].
///
/// A value with no Code Value is not a code, which is the same rule the wire
/// render applies, so it decodes to `None` rather than an empty code.
fn decode_code(joined: &str) -> Option<WorkItemCode> {
    let mut parts = joined.split('\u{1f}');
    let value = parts.next().unwrap_or("");
    if value.is_empty() {
        return None;
    }
    Some(WorkItemCode {
        value: value.to_string(),
        scheme: parts.next().unwrap_or("").to_string(),
        meaning: parts.next().unwrap_or("").to_string(),
    })
}

/// Read a map key as an unsigned integer.
fn read_key(reader: &mut CborReader<'_>) -> Result<u64, WorkItemError> {
    match reader.next()? {
        CborToken::UInt(k) => Ok(k),
        _ => Err(WorkItemError::UnexpectedValue("u64 map key")),
    }
}

/// Read a text value, mapping a non-text token to an error.
fn read_text(reader: &mut CborReader<'_>, what: &'static str) -> Result<String, WorkItemError> {
    match reader.next()? {
        CborToken::Text(s) => Ok(s.to_string()),
        _ => Err(WorkItemError::UnexpectedValue(what)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WorkItemRecord {
        WorkItemRecord {
            id: Some(7),
            workitem_uid: "1.2.826.0.1.3680043.10.1234.8101".to_string(),
            workflow_state: "SCHEDULED".to_string(),
            modality: "CT".to_string(),
            station_aet: Some("STATION01".to_string()),
            patient_name: Some("UPS^PATIENT".to_string()),
            patient_id: Some("UPSPAT01".to_string()),
            transaction_uid: Some("tuid-abc".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let rec = sample();
        let bytes = rec.encode().expect("encode");
        let decoded = WorkItemRecord::decode(&bytes).expect("decode");
        assert_eq!(decoded, rec);
    }

    #[test]
    fn roundtrip_empty_optionals() {
        let rec = WorkItemRecord {
            id: None,
            workitem_uid: "1.2.3".to_string(),
            workflow_state: "COMPLETED".to_string(),
            modality: "MR".to_string(),
            ..Default::default()
        };
        let bytes = rec.encode().expect("encode");
        let decoded = WorkItemRecord::decode(&bytes).expect("decode");
        assert_eq!(decoded, rec);
    }

    #[test]
    fn rejects_garbage() {
        assert!(WorkItemRecord::decode(b"\x00\x01\x02").is_err());
    }

    /// Both task-parameter fields survive the CBOR round trip.
    ///
    /// They live in a file per workitem, so "stored" and "in memory" are the
    /// same thing only if this passes: a field that the encoder wrote but the
    /// decoder dropped would be invisible until a restart.
    #[test]
    fn roundtrip_preserves_task_parameters() {
        let params = r#"{"Value":[{"0040A040":{"vr":"CS","Value":["TEXT"]},"0040A160":{"vr":"UT","Value":["window 400/40"]}}],"vr":"SQ"}"#;
        let rec = WorkItemRecord {
            scheduled_processing_parameters: Some(params.to_string()),
            reason_for_requested_procedure: Some("rule out pneumonia".to_string()),
            ..sample()
        };
        let bytes = rec.encode().expect("encode");
        let decoded = WorkItemRecord::decode(&bytes).expect("decode");
        assert_eq!(
            decoded.scheduled_processing_parameters.as_deref(),
            Some(params),
            "the parameters element is stored verbatim"
        );
        assert_eq!(
            decoded.reason_for_requested_procedure.as_deref(),
            Some("rule out pneumonia")
        );
        assert_eq!(decoded, rec);
    }

    /// The two new keys are written AFTER key 30, so the map ascends.
    ///
    /// The reader walks keys in order and stops at the first unknown one, so
    /// a key emitted out of order would make the record unreadable by its own
    /// decoder - and by any newer server reading an older file.
    #[test]
    fn the_new_keys_are_the_last_two() {
        let keys = [
            K_SCHEDULED_PROCESSING_PARAMETERS,
            K_REASON_FOR_REQUESTED_PROCEDURE,
        ];
        let mut previous = K_PERFORMED_END_DATETIME;
        for key in keys {
            assert!(
                key > previous,
                "key {key} has to follow {previous} so the map ascends"
            );
            previous = key;
        }
    }

    /// An output list must survive encode: the separators it joins with are
    /// structure, not data, so a guard that scans the joined buffer rejects
    /// every record that has one.
    #[test]
    fn encodes_an_output_list() {
        let mut rec = sample();
        rec.output_information = Some(vec![
            OutputReference {
                sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
                series_instance_uid: "1.2.3.4".to_string(),
                sop_instance_uid: "1.2.3.4.5".to_string(),
            },
            OutputReference {
                sop_class_uid: "1.2.840.10008.5.1.4.1.1.4".to_string(),
                series_instance_uid: "1.2.3.6".to_string(),
                sop_instance_uid: "1.2.3.6.7".to_string(),
            },
        ]);
        let bytes = rec.encode().expect("encode");
        assert_eq!(WorkItemRecord::decode(&bytes).expect("decode"), rec);
    }

    /// The guard fires on the COMPONENT, before the join hides it.
    #[test]
    fn rejects_a_separator_inside_an_output_reference() {
        let mut rec = sample();
        rec.output_information = Some(vec![OutputReference {
            sop_class_uid: "1.2.3".to_string(),
            series_instance_uid: "4.5\u{1f}6".to_string(),
            sop_instance_uid: "7".to_string(),
        }]);
        assert!(matches!(
            rec.encode(),
            Err(WorkItemError::UnexpectedValue(_))
        ));
    }
}
