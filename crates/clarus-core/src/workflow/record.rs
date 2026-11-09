// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Modality Worklist (MWL) and MPPS CBOR records.
//!
//! The codec is the zero-allocation numeric-key CBOR the study manifest and
//! the UPS workitem already use (`crate::cbor`), with the same append-only
//! discipline: required keys are always written, optional keys only when they
//! are `Some`, and an unknown key is skipped on read. A record written by an
//! older build therefore still decodes after a new optional key is added, which
//! is what keeps a rolling upgrade from invalidating a live worklist.
//!
//! # MWL shape (PS3.4 Table K.6-1)
//!
//! ```cbor
//! {
//!   1: "<sps_uid>",
//!   2: "<station_aet>",            // (0040,0001)
//!   ... required and optional keys ...
//! }
//! ```
//!
//! # MPPS shape (PS3.4 Table F.7.2-1)
//!
//! ```cbor
//! {
//!   1: "<mpps_uid>",
//!   2: "<status>",                 // (0040,0252)
//!   3: "<modality>",               // (0008,0060)
//!   ... optional keys ...
//! }
//! ```

use crate::cbor::{CborError, CborReader, CborToken, CborWriter};
use std::fmt;

// -- Shared helpers ------------------------------------------------------

/// Write an optional string map entry (key only when `Some`).
fn write_opt_str(
    writer: &mut CborWriter<'_>,
    key: u64,
    value: Option<&str>,
) -> Result<(), CborError> {
    if let Some(v) = value {
        writer.write_u64(key)?;
        writer.write_text(v)?;
    }
    Ok(())
}

/// Read a map key as an unsigned integer.
///
/// A non-integer key is not part of the numeric-key map this codec writes, so
/// it is reported as an unsupported item rather than silently skipped: a
/// skipped key would shift the following value out of place.
fn read_key(reader: &mut CborReader<'_>) -> Result<u64, CborError> {
    match reader.next()? {
        CborToken::UInt(k) => Ok(k),
        _ => Err(CborError::Unsupported(0)),
    }
}

/// Read a text value, mapping a non-text token to an error.
fn read_text(reader: &mut CborReader<'_>) -> Result<String, CborError> {
    match reader.next()? {
        CborToken::Text(s) => Ok(s.to_string()),
        _ => Err(CborError::Unsupported(2)),
    }
}

/// A reference pair from a sequence of performed series.
///
/// PS3.3 C.12.1 defines the same (0008,1150)/(0008,1155) pair the UPS output
/// list uses; an item with an empty instance UID names nothing and is dropped
/// on decode, the same rule [`crate::ups`] applies to an output reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MppsReference {
    /// (0008,1150) Referenced SOP Class UID.
    pub sop_class_uid: String,
    /// (0008,1155) Referenced SOP Instance UID.
    pub sop_instance_uid: String,
}

/// Join reference pairs into one text value, `\u{1e}` between items and
/// `\u{1f}` between the two UIDs - the append-only one-key-one-value shape
/// [`crate::ups::record`] uses for its output list.
///
/// A UID is a digit/dot string (PS3.5 9), so the unit separators cannot occur
/// inside one. A pair whose instance UID is empty names nothing and is skipped
/// here as well as on read: writing it would append a dangling
/// `class<US>` item that the reader would have to reject anyway.
fn join_references(refs: &[MppsReference]) -> String {
    let mut joined = String::new();
    let mut first = true;
    for r in refs {
        if r.sop_instance_uid.is_empty() {
            continue;
        }
        if !first {
            joined.push('\u{1e}');
        }
        first = false;
        joined.push_str(&r.sop_class_uid);
        joined.push('\u{1f}');
        joined.push_str(&r.sop_instance_uid);
    }
    joined
}

/// Split a joined reference list back into pairs, dropping empty items.
///
/// An item with an empty instance UID is dropped: `1.2.3\u{1f}` decodes to a
/// pair that names no instance, and keeping it would put a phantom reference in
/// the MPPS record for every truncated or hostile input.
fn split_references(joined: &str) -> Vec<MppsReference> {
    let mut out = Vec::new();
    for item in joined.split('\u{1e}') {
        if item.is_empty() {
            continue;
        }
        let mut parts = item.split('\u{1f}');
        let sop_class_uid = parts.next().unwrap_or("").to_string();
        let sop_instance_uid = parts.next().unwrap_or("").to_string();
        if sop_instance_uid.is_empty() {
            continue;
        }
        out.push(MppsReference {
            sop_class_uid,
            sop_instance_uid,
        });
    }
    out
}

/// Ceiling on one encoded record.
///
/// The buffer starts small and doubles until the record fits. 16 MiB is far
/// above any real record (a 45-series MPPS is a few KiB) and still bounded, so
/// a record whose writer keeps reporting `BufferFull` fails instead of
/// growing until the allocator does. It mirrors the limit `ups::record` uses
/// for the same reason.
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// Encode a record into a fresh `Vec`, doubling the buffer until it fits.
///
/// A single fixed 4096-byte buffer was the ceiling on a record's size: an MPPS
/// with ~45 performed series (or a UPS workitem with ~20 output references)
/// simply could not be written, and the failure was permanent - the same
/// record would be re-sent and rejected forever. The size of a record is a
/// property of its content, so the buffer is sized from the result rather than
/// guessed.
fn encode_growing<E>(
    mut encode_to: impl FnMut(&mut CborWriter<'_>) -> Result<(), E>,
) -> Result<Vec<u8>, CborError> {
    let mut cap = 512;
    loop {
        let mut buf = vec![0u8; cap];
        let written = {
            let mut writer = CborWriter::new(&mut buf);
            match encode_to(&mut writer) {
                Ok(()) => writer.written(),
                Err(_) if cap < MAX_RECORD_BYTES => {
                    cap = cap.saturating_mul(2).min(MAX_RECORD_BYTES);
                    continue;
                }
                Err(_) => return Err(CborError::BufferFull),
            }
        };
        buf.truncate(written);
        return Ok(buf);
    }
}

// =======================================================================
// MWL: Scheduled Procedure Step record
// =======================================================================

// Map keys, ascending. Append-only: a new key takes the next free number so
// an older reader skips it as an unknown value.
const S_SPS_UID: u64 = 1;
const S_STATION_AET: u64 = 2;
const S_SPS_START_DATE: u64 = 3;
const S_SPS_START_TIME: u64 = 4;
const S_MODALITY: u64 = 5;
const S_PERFORMING_PHYSICIAN: u64 = 6;
const S_PATIENT_NAME: u64 = 7;
const S_PATIENT_ID: u64 = 8;
const S_ACCESSION_NUMBER: u64 = 9;
const S_REQUESTED_PROCEDURE_ID: u64 = 10;
const S_REQUESTED_PROCEDURE_DESCRIPTION: u64 = 11;
const S_STUDY_UID: u64 = 12;
const S_BIRTH_DATE: u64 = 13;
const S_SEX: u64 = 14;
const S_REQUESTING_PHYSICIAN: u64 = 15;
const S_REFERRING_PHYSICIAN: u64 = 16;
const S_SPS_ID: u64 = 17;
const S_CREATED_AT: u64 = 18;
const S_UPDATED_AT: u64 = 19;

/// Errors produced while encoding/decoding an MWL record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpsError {
    /// Underlying CBOR codec failure.
    Cbor(CborError),
    /// The required record key is missing or the envelope is malformed.
    Malformed(&'static str),
}

impl fmt::Display for SpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cbor(e) => write!(f, "SPS CBOR error: {e}"),
            Self::Malformed(why) => write!(f, "malformed SPS record: {why}"),
        }
    }
}

impl std::error::Error for SpsError {}

impl From<CborError> for SpsError {
    fn from(e: CborError) -> Self {
        Self::Cbor(e)
    }
}

/// One Modality Worklist row: a Scheduled Procedure Step.
///
/// The field set is PS3.4 Table K.6-1: the seven REQUIRED matching keys
/// (0040,0001 / 0040,0002 / 0040,0003 / 0008,0060 / 0040,0006 / 0010,0010 /
/// 0010,0020) plus the optional keys this server supports. All three of
/// (0040,0001), (0040,0002) and (0040,0003) are required, so - unlike a UPS
/// workitem - a record here is only complete when it carries them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpsRecord {
    /// (0040,0100) > (0040,0009) is the SPS ID; the RECORD KEY here is the
    /// Scheduled Procedure Step identity of the store, which is the SPS ID when
    /// the site has one and a minted UID otherwise.
    pub sps_uid: String,
    /// (0040,0001) Scheduled Station AE Title - REQUIRED (K.6-1, R).
    pub station_aet: String,
    /// (0040,0002) Scheduled Procedure Step Start Date - REQUIRED (K.6-1, R).
    pub sps_start_date: String,
    /// (0040,0003) Scheduled Procedure Step Start Time - REQUIRED (K.6-1, R).
    pub sps_start_time: String,
    /// (0008,0060) Modality - REQUIRED (K.6-1, R).
    pub modality: String,
    /// (0040,0006) Scheduled Performing Physician's Name - REQUIRED (K.6-1, R,
    /// VM 2, single-value or wild-card match).
    pub scheduled_performing_physician: String,
    /// (0010,0010) Patient's Name - REQUIRED (K.6-1, R).
    pub patient_name: String,
    /// (0010,0020) Patient ID - REQUIRED (K.6-1, R).
    pub patient_id: String,
    /// (0008,0050) Accession Number - optional (K.6-1, O).
    pub accession_number: Option<String>,
    /// (0040,1001) Requested Procedure ID - optional (K.6-1, O).
    pub requested_procedure_id: Option<String>,
    /// (0032,1060) Requested Procedure Description - optional (K.6-1, O, 1C).
    pub requested_procedure_description: Option<String>,
    /// (0020,000D) Study Instance UID - optional (K.6-1, O).
    pub study_uid: Option<String>,
    /// (0010,0030) Patient's Birth Date - optional (K.6-1, O).
    pub birth_date: Option<String>,
    /// (0010,0040) Patient's Sex - optional (K.6-1, O).
    pub sex: Option<String>,
    /// (0032,1032) Requesting Physician - optional (K.6-1, O).
    pub requesting_physician: Option<String>,
    /// (0008,0090) Referring Physician's Name - optional (K.6-1, O).
    pub referring_physician: Option<String>,
    /// (0040,0009) Scheduled Procedure Step ID - optional (K.6-1, O).
    pub sps_id: Option<String>,
    /// Ingestion timestamp (the store's own bookkeeping, not a DICOM element).
    pub created_at: Option<String>,
    /// Last-write timestamp (the store's own bookkeeping).
    pub updated_at: Option<String>,
}

impl SpsRecord {
    /// Encode this record, returning the number of bytes written.
    pub fn encode_to(&self, writer: &mut CborWriter<'_>) -> Result<(), SpsError> {
        let optional: &[bool] = &[
            self.accession_number.is_some(),
            self.requested_procedure_id.is_some(),
            self.requested_procedure_description.is_some(),
            self.study_uid.is_some(),
            self.birth_date.is_some(),
            self.sex.is_some(),
            self.requesting_physician.is_some(),
            self.referring_physician.is_some(),
            self.sps_id.is_some(),
            self.created_at.is_some(),
            self.updated_at.is_some(),
        ];
        let entry_count = 8 + optional.iter().filter(|p| **p).count() as u64;
        writer.write_map_header(entry_count)?;

        writer.write_u64(S_SPS_UID)?;
        writer.write_text(&self.sps_uid)?;
        writer.write_u64(S_STATION_AET)?;
        writer.write_text(&self.station_aet)?;
        writer.write_u64(S_SPS_START_DATE)?;
        writer.write_text(&self.sps_start_date)?;
        writer.write_u64(S_SPS_START_TIME)?;
        writer.write_text(&self.sps_start_time)?;
        writer.write_u64(S_MODALITY)?;
        writer.write_text(&self.modality)?;
        writer.write_u64(S_PERFORMING_PHYSICIAN)?;
        writer.write_text(&self.scheduled_performing_physician)?;
        writer.write_u64(S_PATIENT_NAME)?;
        writer.write_text(&self.patient_name)?;
        writer.write_u64(S_PATIENT_ID)?;
        writer.write_text(&self.patient_id)?;

        write_opt_str(writer, S_ACCESSION_NUMBER, self.accession_number.as_deref())?;
        write_opt_str(
            writer,
            S_REQUESTED_PROCEDURE_ID,
            self.requested_procedure_id.as_deref(),
        )?;
        write_opt_str(
            writer,
            S_REQUESTED_PROCEDURE_DESCRIPTION,
            self.requested_procedure_description.as_deref(),
        )?;
        write_opt_str(writer, S_STUDY_UID, self.study_uid.as_deref())?;
        write_opt_str(writer, S_BIRTH_DATE, self.birth_date.as_deref())?;
        write_opt_str(writer, S_SEX, self.sex.as_deref())?;
        write_opt_str(
            writer,
            S_REQUESTING_PHYSICIAN,
            self.requesting_physician.as_deref(),
        )?;
        write_opt_str(
            writer,
            S_REFERRING_PHYSICIAN,
            self.referring_physician.as_deref(),
        )?;
        write_opt_str(writer, S_SPS_ID, self.sps_id.as_deref())?;
        write_opt_str(writer, S_CREATED_AT, self.created_at.as_deref())?;
        write_opt_str(writer, S_UPDATED_AT, self.updated_at.as_deref())?;
        Ok(())
    }

    /// Encode into a freshly allocated `Vec<u8>`.
    pub fn encode(&self) -> Result<Vec<u8>, SpsError> {
        encode_growing(|w| self.encode_to(w)).map_err(SpsError::Cbor)
    }

    /// Parse an SPS record from `buf`.
    pub fn decode(buf: &[u8]) -> Result<SpsRecord, SpsError> {
        let mut reader = CborReader::new(buf);
        let CborToken::MapStart(n) = reader.next()? else {
            return Err(SpsError::Malformed("top-level record is not a map"));
        };
        let mut rec = SpsRecord::default();
        for _ in 0..n {
            let key = read_key(&mut reader)?;
            match key {
                S_SPS_UID => rec.sps_uid = read_text(&mut reader)?,
                S_STATION_AET => rec.station_aet = read_text(&mut reader)?,
                S_SPS_START_DATE => rec.sps_start_date = read_text(&mut reader)?,
                S_SPS_START_TIME => rec.sps_start_time = read_text(&mut reader)?,
                S_MODALITY => rec.modality = read_text(&mut reader)?,
                S_PERFORMING_PHYSICIAN => {
                    rec.scheduled_performing_physician = read_text(&mut reader)?
                }
                S_PATIENT_NAME => rec.patient_name = read_text(&mut reader)?,
                S_PATIENT_ID => rec.patient_id = read_text(&mut reader)?,
                S_ACCESSION_NUMBER => rec.accession_number = Some(read_text(&mut reader)?),
                S_REQUESTED_PROCEDURE_ID => {
                    rec.requested_procedure_id = Some(read_text(&mut reader)?)
                }
                S_REQUESTED_PROCEDURE_DESCRIPTION => {
                    rec.requested_procedure_description = Some(read_text(&mut reader)?)
                }
                S_STUDY_UID => rec.study_uid = Some(read_text(&mut reader)?),
                S_BIRTH_DATE => rec.birth_date = Some(read_text(&mut reader)?),
                S_SEX => rec.sex = Some(read_text(&mut reader)?),
                S_REQUESTING_PHYSICIAN => rec.requesting_physician = Some(read_text(&mut reader)?),
                S_REFERRING_PHYSICIAN => rec.referring_physician = Some(read_text(&mut reader)?),
                S_SPS_ID => rec.sps_id = Some(read_text(&mut reader)?),
                S_CREATED_AT => rec.created_at = Some(read_text(&mut reader)?),
                S_UPDATED_AT => rec.updated_at = Some(read_text(&mut reader)?),
                // Unknown key: skip the value (tolerant decode).
                _ => reader.skip_value()?,
            }
        }
        if !reader.is_empty() {
            return Err(SpsError::Cbor(CborError::TrailingData));
        }
        Ok(rec)
    }
}

// =======================================================================
// MPPS: Modality Performed Procedure Step record
// =======================================================================

const M_MPPS_UID: u64 = 1;
const M_STATUS: u64 = 2;
const M_MODALITY: u64 = 3;
const M_STATION_AE: u64 = 4;
const M_STATION_NAME: u64 = 5;
const M_START_DATE: u64 = 6;
const M_START_TIME: u64 = 7;
const M_END_DATE: u64 = 8;
const M_END_TIME: u64 = 9;
const M_STUDY_UID: u64 = 10;
const M_PERFORMED_SERIES: u64 = 11;
const M_DISCONTINUATION_REASON: u64 = 12;
const M_CREATED_AT: u64 = 13;
const M_UPDATED_AT: u64 = 14;

/// Errors produced while encoding/decoding an MPPS record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MppsError {
    /// Underlying CBOR codec failure.
    Cbor(CborError),
    /// The required record key is missing or the envelope is malformed.
    Malformed(&'static str),
}

impl fmt::Display for MppsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cbor(e) => write!(f, "MPPS CBOR error: {e}"),
            Self::Malformed(why) => write!(f, "malformed MPPS record: {why}"),
        }
    }
}

impl std::error::Error for MppsError {}

impl From<CborError> for MppsError {
    fn from(e: CborError) -> Self {
        Self::Cbor(e)
    }
}

/// One Modality Performed Procedure Step (PS3.4 Table F.7.2-1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MppsRecord {
    /// The MPPS SOP Instance UID - the record key and the `{mppsUID}` path
    /// segment (PS3.18 15.1).
    pub mpps_uid: String,
    /// (0040,0252) Performed Procedure Step Status - Type 1 at N-CREATE.
    /// `IN PROGRESS` while the step runs; `COMPLETED` or `DISCONTINUED` once
    /// it is final (PS3.4 F.7.2.1).
    pub status: String,
    /// (0008,0060) Modality.
    pub modality: Option<String>,
    /// (0040,0241) Performed Station AE Title.
    pub station_ae: Option<String>,
    /// (0040,0242) Performed Station Name.
    pub station_name: Option<String>,
    /// (0040,0244) Performed Procedure Step Start Date.
    pub start_date: Option<String>,
    /// (0040,0245) Performed Procedure Step Start Time.
    pub start_time: Option<String>,
    /// (0040,0250) Performed Procedure Step End Date.
    pub end_date: Option<String>,
    /// (0040,0251) Performed Procedure Step End Time.
    pub end_time: Option<String>,
    /// (0040,0270) > (0020,000D) Study Instance UID - the study lives INSIDE
    /// the Scheduled Step Attributes Sequence, not at the top level (PS3.4
    /// F.7 state attributes).
    pub study_uid: Option<String>,
    /// (0040,0340) Performed Series Sequence. PS3.4 F.7 requires this to be
    /// NON-EMPTY when the status becomes final; `None` means the sequence was
    /// never written, `Some(vec![])` would mean it was written empty - the
    /// handler refuses the latter at a final transition.
    pub performed_series: Option<Vec<MppsReference>>,
    /// (0040,0281) Performed Procedure Step Discontinuation Reason Code
    /// Sequence - OPTIONAL (3/3). When the performer wants to use it, it is
    /// created ZERO-LENGTH at N-CREATE, because a top-level attribute cannot be
    /// deleted once set (PS3.4 F.7.2.1.1 note 5). An empty string is therefore
    /// a legal, distinct value: "created but not filled".
    pub discontinuation_reason: Option<String>,
    /// Ingestion timestamp (store bookkeeping).
    pub created_at: Option<String>,
    /// Last-write timestamp (store bookkeeping).
    pub updated_at: Option<String>,
}

impl MppsRecord {
    /// Whether the status is a final one (COMPLETED or DISCONTINUED).
    ///
    /// The values are a DICOM CS; the comparison is exact because the standard
    /// fixes the enumeration (PS3.4 Table F.7.2-1).
    #[must_use]
    pub fn is_final_status(status: &str) -> bool {
        matches!(status, "COMPLETED" | "DISCONTINUED")
    }

    /// Encode this record, returning the number of bytes written.
    pub fn encode_to(&self, writer: &mut CborWriter<'_>) -> Result<(), MppsError> {
        let optional: &[bool] = &[
            self.modality.is_some(),
            self.station_ae.is_some(),
            self.station_name.is_some(),
            self.start_date.is_some(),
            self.start_time.is_some(),
            self.end_date.is_some(),
            self.end_time.is_some(),
            self.study_uid.is_some(),
            self.performed_series.is_some(),
            self.discontinuation_reason.is_some(),
            self.created_at.is_some(),
            self.updated_at.is_some(),
        ];
        let entry_count = 2 + optional.iter().filter(|p| **p).count() as u64;
        writer.write_map_header(entry_count)?;

        writer.write_u64(M_MPPS_UID)?;
        writer.write_text(&self.mpps_uid)?;
        writer.write_u64(M_STATUS)?;
        writer.write_text(&self.status)?;

        write_opt_str(writer, M_MODALITY, self.modality.as_deref())?;
        write_opt_str(writer, M_STATION_AE, self.station_ae.as_deref())?;
        write_opt_str(writer, M_STATION_NAME, self.station_name.as_deref())?;
        write_opt_str(writer, M_START_DATE, self.start_date.as_deref())?;
        write_opt_str(writer, M_START_TIME, self.start_time.as_deref())?;
        write_opt_str(writer, M_END_DATE, self.end_date.as_deref())?;
        write_opt_str(writer, M_END_TIME, self.end_time.as_deref())?;
        write_opt_str(writer, M_STUDY_UID, self.study_uid.as_deref())?;
        // The performed series list is written as ONE text value, the same
        // one-key-one-value shape the UPS output list uses, so an older reader
        // skips a single value rather than a nested array.
        if let Some(refs) = &self.performed_series {
            writer.write_u64(M_PERFORMED_SERIES)?;
            writer.write_text(&join_references(refs))?;
        }
        // The discontinuation reason is written even when it is an empty
        // string: an empty value is the standard's own "created zero-length at
        // N-CREATE and not yet filled", which is a different statement from
        // "the key was never written" (None).
        write_opt_str(
            writer,
            M_DISCONTINUATION_REASON,
            self.discontinuation_reason.as_deref(),
        )?;
        write_opt_str(writer, M_CREATED_AT, self.created_at.as_deref())?;
        write_opt_str(writer, M_UPDATED_AT, self.updated_at.as_deref())?;
        Ok(())
    }

    /// Encode into a freshly allocated `Vec<u8>`.
    pub fn encode(&self) -> Result<Vec<u8>, MppsError> {
        encode_growing(|w| self.encode_to(w)).map_err(MppsError::Cbor)
    }

    /// Parse an MPPS record from `buf`.
    pub fn decode(buf: &[u8]) -> Result<MppsRecord, MppsError> {
        let mut reader = CborReader::new(buf);
        let CborToken::MapStart(n) = reader.next()? else {
            return Err(MppsError::Malformed("top-level record is not a map"));
        };
        let mut rec = MppsRecord::default();
        for _ in 0..n {
            let key = read_key(&mut reader)?;
            match key {
                M_MPPS_UID => rec.mpps_uid = read_text(&mut reader)?,
                M_STATUS => rec.status = read_text(&mut reader)?,
                M_MODALITY => rec.modality = Some(read_text(&mut reader)?),
                M_STATION_AE => rec.station_ae = Some(read_text(&mut reader)?),
                M_STATION_NAME => rec.station_name = Some(read_text(&mut reader)?),
                M_START_DATE => rec.start_date = Some(read_text(&mut reader)?),
                M_START_TIME => rec.start_time = Some(read_text(&mut reader)?),
                M_END_DATE => rec.end_date = Some(read_text(&mut reader)?),
                M_END_TIME => rec.end_time = Some(read_text(&mut reader)?),
                M_STUDY_UID => rec.study_uid = Some(read_text(&mut reader)?),
                M_PERFORMED_SERIES => {
                    let joined = read_text(&mut reader)?;
                    // An empty value is an EMPTY list, not `None`: the key being
                    // present is the statement "the performer wrote a series
                    // list", which is exactly the distinction the final-state
                    // check needs.
                    rec.performed_series = Some(split_references(&joined));
                }
                M_DISCONTINUATION_REASON => {
                    rec.discontinuation_reason = Some(read_text(&mut reader)?)
                }
                M_CREATED_AT => rec.created_at = Some(read_text(&mut reader)?),
                M_UPDATED_AT => rec.updated_at = Some(read_text(&mut reader)?),
                _ => reader.skip_value()?,
            }
        }
        if !reader.is_empty() {
            return Err(MppsError::Cbor(CborError::TrailingData));
        }
        Ok(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sps_sample() -> SpsRecord {
        SpsRecord {
            sps_uid: "1.2.826.0.1.3680043.10.1234.9001".to_string(),
            station_aet: "CT01".to_string(),
            sps_start_date: "20261011".to_string(),
            sps_start_time: "090000".to_string(),
            modality: "CT".to_string(),
            scheduled_performing_physician: "SMITH^JOHN".to_string(),
            patient_name: "IVANOV^IVAN".to_string(),
            patient_id: "PAT-1".to_string(),
            accession_number: Some("ACC-1".to_string()),
            sps_id: Some("SPS-1".to_string()),
            ..Default::default()
        }
    }

    fn mpps_sample() -> MppsRecord {
        MppsRecord {
            mpps_uid: "1.2.826.0.1.3680043.10.1234.9002".to_string(),
            status: "IN PROGRESS".to_string(),
            modality: Some("CT".to_string()),
            station_ae: Some("CT01".to_string()),
            study_uid: Some("1.2.826.0.1.9".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn sps_roundtrip_preserves_fields() {
        let rec = sps_sample();
        let bytes = rec.encode().expect("encode");
        let decoded = SpsRecord::decode(&bytes).expect("decode");
        assert_eq!(decoded, rec);
    }

    #[test]
    fn sps_roundtrip_empty_optionals() {
        let rec = SpsRecord {
            sps_uid: "1.2.3".to_string(),
            station_aet: "CT01".to_string(),
            sps_start_date: "20261011".to_string(),
            sps_start_time: "090000".to_string(),
            modality: "CT".to_string(),
            scheduled_performing_physician: "SMITH^JOHN".to_string(),
            patient_name: "P^Q".to_string(),
            patient_id: "P1".to_string(),
            ..Default::default()
        };
        let bytes = rec.encode().expect("encode");
        let decoded = SpsRecord::decode(&bytes).expect("decode");
        assert_eq!(decoded, rec);
    }

    #[test]
    fn sps_unknown_key_is_skipped() {
        // A record written by a newer build carries a key this reader does not
        // know. Mirror the record's own encoding capacity, then append key 999.
        let mut buf = vec![0u8; 4096];
        let written = {
            let mut writer = CborWriter::new(&mut buf);
            // A two-entry map: the required sps_uid plus an unknown key.
            writer.write_map_header(2).unwrap();
            writer.write_u64(S_SPS_UID).unwrap();
            writer.write_text("1.2.3").unwrap();
            writer.write_u64(999).unwrap();
            writer.write_text("ignored").unwrap();
            writer.written()
        };
        buf.truncate(written);
        let decoded = SpsRecord::decode(&buf).expect("tolerant decode");
        assert_eq!(decoded.sps_uid, "1.2.3");
    }

    #[test]
    fn sps_garbage_is_malformed_not_panic() {
        assert!(SpsRecord::decode(b"\x00\x01\x02").is_err());
        // A corrupted byte in an otherwise-valid record fails cleanly.
        let mut bytes = sps_sample().encode().unwrap();
        let last = bytes.len() - 1;
        bytes[last] = 0xff;
        // Either a decode error or a preserved record is fine; a panic is not.
        let _ = SpsRecord::decode(&bytes);
    }

    #[test]
    fn mpps_roundtrip_preserves_fields() {
        let rec = mpps_sample();
        let bytes = rec.encode().expect("encode");
        let decoded = MppsRecord::decode(&bytes).expect("decode");
        assert_eq!(decoded, rec);
    }

    #[test]
    fn mpps_roundtrip_with_performed_series() {
        let rec = MppsRecord {
            performed_series: Some(vec![
                MppsReference {
                    sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
                    sop_instance_uid: "1.2.3.4".to_string(),
                },
                MppsReference {
                    sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
                    sop_instance_uid: "1.2.3.5".to_string(),
                },
            ]),
            ..mpps_sample()
        };
        let bytes = rec.encode().expect("encode");
        let decoded = MppsRecord::decode(&bytes).expect("decode");
        assert_eq!(decoded, rec);
    }

    #[test]
    fn mpps_empty_series_list_is_not_none() {
        // The distinction the final-state check depends on: a key that was
        // written (even empty) must not collapse into "never written".
        let rec = MppsRecord {
            performed_series: Some(Vec::new()),
            ..mpps_sample()
        };
        let bytes = rec.encode().expect("encode");
        let decoded = MppsRecord::decode(&bytes).expect("decode");
        assert_eq!(decoded.performed_series, Some(Vec::new()));
    }

    #[test]
    fn mpps_empty_discontinuation_reason_is_not_none() {
        // (0040,0281) is optional but may be created zero-length at N-CREATE
        // (PS3.4 F.7.2.1.1 note 5); that empty value must survive the codec.
        let rec = MppsRecord {
            discontinuation_reason: Some(String::new()),
            ..mpps_sample()
        };
        let bytes = rec.encode().expect("encode");
        let decoded = MppsRecord::decode(&bytes).expect("decode");
        assert_eq!(decoded.discontinuation_reason, Some(String::new()));
    }

    #[test]
    fn mpps_final_status_predicate() {
        assert!(MppsRecord::is_final_status("COMPLETED"));
        assert!(MppsRecord::is_final_status("DISCONTINUED"));
        assert!(!MppsRecord::is_final_status("IN PROGRESS"));
    }
}
