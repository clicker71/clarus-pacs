// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Storage Commitment transaction record (`commitments/{TransactionUID}.cbor`).
//!
//! PS3.18 Section 13 splits the service into an asynchronous pair (Commit,
//! then Check Commit Result), which means the origin owes the user agent a
//! DURABLE answer keyed by the Transaction UID in the resource path. This
//! module is that record.
//!
//! # CBOR shape
//!
//! ```cbor
//! {
//!   1: "<transaction_uid>",
//!   2: "<requested_at>",       // ISO timestamp
//!   3: <request_form u64>,     // 1 = instance-shaped, 2 = study-shaped
//!   4: [                        // the referenced instances, request order
//!     { 1: "<sop_class_uid>", 2: "<sop_instance_uid>",
//!       3: <status u64>,        // 1 = committed, 2 = failed
//!       4: "<failure_reason>",  // present only when status = 2
//!       5: "<study_uid>" }      // present only for a study-shaped request
//!   ]
//! }
//! ```
//!
//! Numeric map keys follow the manifest/UPS idiom. Decoding is tolerant: a
//! missing optional key yields `None` and an unknown key is skipped, so a
//! record written by a later build still decodes in an earlier one.
//!
//! # Why the request SHAPE is stored
//!
//! Annex J.2 says the result mirrors the request: an instance-shaped request
//! is answered per instance, a study-shaped one per study. The Check Commit
//! Result transaction must return the SAME form as the original Commit, so
//! the shape is a property of the stored answer rather than of the read path
//! asking - without it, Check would have to guess.

use crate::cbor::{CborError, CborReader, CborToken, CborWriter};
use std::fmt;

// Record map keys.
const K_TRANSACTION_UID: u64 = 1;
const K_REQUESTED_AT: u64 = 2;
const K_REQUEST_FORM: u64 = 3;
const K_INSTANCES: u64 = 4;

// Instance item keys.
const K_SOP_CLASS_UID: u64 = 1;
const K_SOP_INSTANCE_UID: u64 = 2;
const K_STATUS: u64 = 3;
const K_FAILURE_REASON: u64 = 4;
const K_STUDY_UID: u64 = 5;

/// The request form the user agent used (Annex J.1).
///
/// Stored because the result must MIRROR it (Annex J.2): an instance-shaped
/// request is answered with Referenced/Failed SOP Sequence, a study-shaped
/// one with Referenced/Failed Study Sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestForm {
    /// Referenced SOP Sequence (0008,1199) - the per-instance form.
    Instance,
    /// Referenced Study Sequence (0008,1110) - the study form.
    Study,
}

impl RequestForm {
    /// The on-disk discriminant.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        match self {
            Self::Instance => 1,
            Self::Study => 2,
        }
    }

    /// Decode the discriminant; an unknown value is a malformed record.
    fn from_u64(value: u64) -> Result<Self, RecordError> {
        match value {
            1 => Ok(Self::Instance),
            2 => Ok(Self::Study),
            other => Err(RecordError::Malformed(match other {
                0 => "request_form 0 is not a form",
                _ => "request_form is not 1 (instance) or 2 (study)",
            })),
        }
    }
}

/// What happened to one referenced SOP Instance (Annex J.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitmentOutcome {
    /// The instance is held: it appears in Referenced SOP Sequence (0008,1199).
    Committed,
    /// The instance is not held: it appears in Failed SOP Sequence (0008,1198)
    /// with a Failure Reason (0008,1197).
    Failed,
}

impl CommitmentOutcome {
    fn as_u64(self) -> u64 {
        match self {
            Self::Committed => 1,
            Self::Failed => 2,
        }
    }

    fn from_u64(value: u64) -> Result<Self, RecordError> {
        match value {
            1 => Ok(Self::Committed),
            2 => Ok(Self::Failed),
            _ => Err(RecordError::Malformed(
                "instance status is not 1 (committed) or 2 (failed)",
            )),
        }
    }
}

/// The Failure Reason (0008,1197) values this server can emit.
///
/// The vocabulary is PS3.3 C.14.1.1. Only the two this server has an honest
/// reason to use are named: an instance we cannot find, and a request that
/// named a SOP Class we do not store. The rest of the enumeration describes
/// DIMSE circumstances that a DICOMweb origin evaluating its own manifest
/// never encounters, and inventing a code to fill a field would be worse than
/// omitting the case.
///
/// 0131H (Duplicate Transaction UID) is deliberately ABSENT: in DICOMweb that
/// condition is the HTTP 409 of Table 13.4.3-1, not a per-instance result, so
/// it must not appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// 0112H No such SOP Instance - "The specified SOP Instance was not
    /// available." The verifiable fact when a referenced instance is not in
    /// the archive.
    NoSuchSopInstance,
    /// 0110H Processing Failure - the catch-all for a lookup that could not be
    /// completed (a manifest that will not decode, an I/O fault). Distinct
    /// from 0112H on purpose: "not held" and "could not be determined" are
    /// different answers and an operator acts on them differently.
    ProcessingFailure,
}

impl FailureReason {
    /// The DICOM code as the four hex digits a DICOMweb payload carries.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NoSuchSopInstance => "0112",
            Self::ProcessingFailure => "0110",
        }
    }

    fn from_code(code: &str) -> Result<Self, RecordError> {
        match code {
            "0112" => Ok(Self::NoSuchSopInstance),
            "0110" => Ok(Self::ProcessingFailure),
            _ => Err(RecordError::Malformed(
                "failure_reason is not a reason this server emits",
            )),
        }
    }
}

/// One referenced SOP Instance and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceResult {
    /// (0008,1150) Referenced SOP Class UID.
    pub sop_class_uid: String,
    /// (0008,1155) Referenced SOP Instance UID.
    pub sop_instance_uid: String,
    /// Committed or failed.
    pub outcome: CommitmentOutcome,
    /// (0008,1197) Failure Reason, present exactly when `outcome` is `Failed`.
    pub failure_reason: Option<FailureReason>,
    /// (0020,000D) of the study this instance was reached through, for a
    /// study-shaped request. `None` for an instance-shaped one.
    ///
    /// Carried so a study-shaped Check can group the stored per-instance
    /// results back into the study form Annex J.2 requires, without walking
    /// the archive again.
    pub study_uid: Option<String>,
}

/// One Storage Commitment transaction, as persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitmentRecord {
    /// The Transaction UID from the resource path (13.4.1.1).
    pub transaction_uid: String,
    /// When the Commit was answered, ISO text.
    pub requested_at: String,
    /// Which request form this answer mirrors.
    pub request_form: RequestForm,
    /// The per-instance results, in request order.
    ///
    /// Order is preserved rather than sorted: the payload is a mirror of what
    /// the user agent sent, and a stable order is what makes a byte-exact
    /// golden meaningful.
    pub instances: Vec<InstanceResult>,
}

/// Errors from encoding or decoding a commitment record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// The bytes on disk are not a record this build understands.
    Malformed(&'static str),
    /// The underlying codec rejected the bytes.
    Cbor(CborError),
    /// The record is structurally valid but not usable.
    Invalid(&'static str),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "commitment record malformed: {what}"),
            Self::Cbor(e) => write!(f, "commitment record codec error: {e}"),
            Self::Invalid(what) => write!(f, "commitment record invalid: {what}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl From<CborError> for RecordError {
    fn from(e: CborError) -> Self {
        Self::Cbor(e)
    }
}

impl CommitmentRecord {
    /// Encode this record into `writer`.
    pub fn encode_to(&self, writer: &mut CborWriter<'_>) -> Result<(), RecordError> {
        writer.write_map_header(4)?;
        writer.write_u64(K_TRANSACTION_UID)?;
        writer.write_text(&self.transaction_uid)?;
        writer.write_u64(K_REQUESTED_AT)?;
        writer.write_text(&self.requested_at)?;
        writer.write_u64(K_REQUEST_FORM)?;
        writer.write_u64(self.request_form.as_u64())?;

        writer.write_u64(K_INSTANCES)?;
        writer.write_array_header(self.instances.len() as u64)?;
        for item in &self.instances {
            // 3 required keys plus the optional failure reason and study UID.
            let entries =
                3 + u64::from(item.failure_reason.is_some()) + u64::from(item.study_uid.is_some());
            writer.write_map_header(entries)?;
            writer.write_u64(K_SOP_CLASS_UID)?;
            writer.write_text(&item.sop_class_uid)?;
            writer.write_u64(K_SOP_INSTANCE_UID)?;
            writer.write_text(&item.sop_instance_uid)?;
            writer.write_u64(K_STATUS)?;
            writer.write_u64(item.outcome.as_u64())?;
            if let Some(reason) = item.failure_reason {
                writer.write_u64(K_FAILURE_REASON)?;
                writer.write_text(reason.code())?;
            }
            if let Some(study) = &item.study_uid {
                writer.write_u64(K_STUDY_UID)?;
                writer.write_text(study)?;
            }
        }
        Ok(())
    }

    /// Encode into a freshly allocated buffer.
    pub fn encode(&self) -> Result<Vec<u8>, RecordError> {
        // Sized from the ACTUAL strings, not from a per-instance guess. A UID
        // is up to 64 characters (PS3.5) and the failure reason is free text,
        // so `160 + n*160` underestimated a large study badly enough that the
        // writer hit `BufferFull` and the record could never be stored.
        //
        // The previous estimate counted only the per-instance UIDs, which left
        // out every other string the record carries: the transaction UID, the
        // request timestamp, each instance's optional study UID and failure
        // reason. With those omitted a record whose payload was mostly reasons
        // or study UIDs still overflowed. Every length below is exact except
        // the framing constants, which are an upper bound:
        //   * the top-level map holds 4 keys;
        //   * each instance map holds 3 + its two optional keys;
        //   * a text header is at most 9 bytes (1 type byte + 8 length) and an
        //     integer header at most 9 bytes (uint64), so 24 bytes per key and
        //     value covers the map keys, the array header and the numbers.
        const FRAMING: usize = 256;
        const PER_KEY: usize = 24;
        let mut text_bytes = self.transaction_uid.len() + self.requested_at.len();
        let mut value_count = 0usize;
        for item in &self.instances {
            text_bytes += item.sop_class_uid.len() + item.sop_instance_uid.len();
            value_count += 1;
            if let Some(reason) = item.failure_reason {
                text_bytes += reason.code().len();
                value_count += 1;
            }
            if let Some(study) = &item.study_uid {
                text_bytes += study.len();
                value_count += 1;
            }
        }
        let capacity = FRAMING
            .saturating_add(text_bytes)
            .saturating_add(self.instances.len().saturating_mul(PER_KEY))
            .saturating_add(value_count.saturating_mul(PER_KEY));
        let mut buf = vec![0u8; capacity];
        let written = {
            let mut writer = CborWriter::new(&mut buf);
            self.encode_to(&mut writer)?;
            writer.written()
        };
        buf.truncate(written);
        Ok(buf)
    }

    /// Decode a record from `buf`.
    pub fn decode(buf: &[u8]) -> Result<Self, RecordError> {
        let mut reader = CborReader::new(buf);
        let CborToken::MapStart(entries) = reader.next()? else {
            return Err(RecordError::Malformed("top-level record is not a map"));
        };

        let mut transaction_uid: Option<String> = None;
        let mut requested_at: Option<String> = None;
        let mut request_form: Option<RequestForm> = None;
        let mut instances: Option<Vec<InstanceResult>> = None;

        for _ in 0..entries {
            let key = read_key(&mut reader)?;
            match key {
                K_TRANSACTION_UID => transaction_uid = Some(read_text(&mut reader)?),
                K_REQUESTED_AT => requested_at = Some(read_text(&mut reader)?),
                K_REQUEST_FORM => {
                    let CborToken::UInt(v) = reader.next()? else {
                        return Err(RecordError::Malformed("request_form is not an integer"));
                    };
                    request_form = Some(RequestForm::from_u64(v)?);
                }
                K_INSTANCES => instances = Some(read_instances(&mut reader)?),
                _ => reader.skip_value()?,
            }
        }

        if !reader.is_empty() {
            return Err(RecordError::Cbor(CborError::TrailingData));
        }

        Ok(CommitmentRecord {
            // A missing key is a malformed record rather than an empty string:
            // the Transaction UID is the file's own identity, so a record
            // without one would be a file that cannot be addressed.
            transaction_uid: transaction_uid.ok_or(RecordError::Malformed("no transaction_uid"))?,
            requested_at: requested_at.ok_or(RecordError::Malformed("no requested_at"))?,
            request_form: request_form.ok_or(RecordError::Malformed("no request_form"))?,
            instances: instances.ok_or(RecordError::Malformed("no instances array"))?,
        })
    }

    /// Whether every referenced instance was committed.
    ///
    /// Not used to pick a status code - a 200 carries all-failed just as
    /// legitimately as all-committed (13.4.2 Note) - but it is what the
    /// retention sweep and the audit line report.
    #[must_use]
    pub fn all_committed(&self) -> bool {
        !self.instances.is_empty()
            && self
                .instances
                .iter()
                .all(|i| i.outcome == CommitmentOutcome::Committed)
    }
}

/// Ceiling on the up-front allocation when decoding an instance array.
///
/// The count comes from the file; a forged one must not become a huge
/// `Vec::with_capacity`. The Vec still grows past this for a genuinely large
/// record - the cap bounds the SPECULATIVE allocation only.
const MAX_PREALLOC: usize = 1024;

/// Read the per-instance array.
fn read_instances(reader: &mut CborReader<'_>) -> Result<Vec<InstanceResult>, RecordError> {
    let CborToken::ArrayStart(count) = reader.next()? else {
        return Err(RecordError::Malformed("instances is not an array"));
    };
    // `with_capacity` takes a `usize`; the array length is a CBOR `u64`. A
    // hostile length (~2^40) must not become an allocation: the hint is capped
    // at [`MAX_PREALLOC`] and the Vec grows on demand from there, so a forged
    // count costs one small allocation per REAL item and nothing else.
    let capacity = usize::try_from(count)
        .map_err(|_| RecordError::Malformed("instances count does not fit this platform"))?
        .min(MAX_PREALLOC);
    let mut out = Vec::with_capacity(capacity);
    for _ in 0..count {
        out.push(read_instance(reader)?);
    }
    Ok(out)
}

/// Read one per-instance item.
fn read_instance(reader: &mut CborReader<'_>) -> Result<InstanceResult, RecordError> {
    let CborToken::MapStart(entries) = reader.next()? else {
        return Err(RecordError::Malformed("instance item is not a map"));
    };
    let mut sop_class_uid: Option<String> = None;
    let mut sop_instance_uid: Option<String> = None;
    let mut outcome: Option<CommitmentOutcome> = None;
    let mut failure_reason: Option<FailureReason> = None;
    let mut study_uid: Option<String> = None;

    for _ in 0..entries {
        let key = read_key(reader)?;
        match key {
            K_SOP_CLASS_UID => sop_class_uid = Some(read_text(reader)?),
            K_SOP_INSTANCE_UID => sop_instance_uid = Some(read_text(reader)?),
            K_STATUS => {
                let CborToken::UInt(v) = reader.next()? else {
                    return Err(RecordError::Malformed("instance status is not an integer"));
                };
                outcome = Some(CommitmentOutcome::from_u64(v)?);
            }
            K_FAILURE_REASON => {
                let code = read_text(reader)?;
                failure_reason = Some(FailureReason::from_code(&code)?);
            }
            K_STUDY_UID => study_uid = Some(read_text(reader)?),
            _ => reader.skip_value()?,
        }
    }

    let outcome = outcome.ok_or(RecordError::Malformed("instance has no status"))?;
    // The two are coupled by Annex J.2: a committed instance has no Failure
    // Reason (it is in the success sequence, which carries none), and a failed
    // one must have one (the failure sequence's whole point). A record that
    // disagreed would produce a payload no reader could classify, so it is
    // refused at the boundary rather than rendered.
    match (outcome, failure_reason) {
        (CommitmentOutcome::Committed, Some(_)) => {
            return Err(RecordError::Invalid(
                "a committed instance must not carry a failure reason",
            ));
        }
        (CommitmentOutcome::Failed, None) => {
            return Err(RecordError::Invalid(
                "a failed instance must carry a failure reason",
            ));
        }
        _ => {}
    }

    Ok(InstanceResult {
        sop_class_uid: sop_class_uid.ok_or(RecordError::Malformed("instance has no sop class"))?,
        sop_instance_uid: sop_instance_uid
            .ok_or(RecordError::Malformed("instance has no sop instance uid"))?,
        outcome,
        failure_reason,
        study_uid,
    })
}

/// Read a map key as an unsigned integer.
fn read_key(reader: &mut CborReader<'_>) -> Result<u64, RecordError> {
    match reader.next()? {
        CborToken::UInt(k) => Ok(k),
        _ => Err(RecordError::Malformed("map key is not an integer")),
    }
}

/// Read a text value.
fn read_text(reader: &mut CborReader<'_>) -> Result<String, RecordError> {
    match reader.next()? {
        CborToken::Text(s) => Ok(s.to_string()),
        _ => Err(RecordError::Malformed("expected a text value")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed(class: &str, instance: &str) -> InstanceResult {
        InstanceResult {
            sop_class_uid: class.to_string(),
            sop_instance_uid: instance.to_string(),
            outcome: CommitmentOutcome::Committed,
            failure_reason: None,
            study_uid: None,
        }
    }

    fn failed(class: &str, instance: &str) -> InstanceResult {
        InstanceResult {
            sop_class_uid: class.to_string(),
            sop_instance_uid: instance.to_string(),
            outcome: CommitmentOutcome::Failed,
            failure_reason: Some(FailureReason::NoSuchSopInstance),
            study_uid: None,
        }
    }

    fn record() -> CommitmentRecord {
        CommitmentRecord {
            transaction_uid: "1.2.3.4.5".to_string(),
            requested_at: "2026-09-23T10:00:00Z".to_string(),
            request_form: RequestForm::Instance,
            instances: vec![committed("1.2.840.10008.5.1.4.1.1.2", "2.25.1")],
        }
    }

    #[test]
    fn a_record_round_trips_through_the_cbor_codec() {
        let rec = record();
        let bytes = rec.encode().expect("encode");
        let back = CommitmentRecord::decode(&bytes).expect("decode");
        assert_eq!(back, rec);
    }

    #[test]
    fn a_mixed_result_round_trips_with_its_failure_reason() {
        let mut rec = record();
        rec.instances = vec![
            committed("1.2.840.10008.5.1.4.1.1.2", "2.25.1"),
            failed("1.2.840.10008.5.1.4.1.1.2", "2.25.2"),
        ];
        let bytes = rec.encode().expect("encode");
        let back = CommitmentRecord::decode(&bytes).expect("decode");
        assert_eq!(back.instances.len(), 2);
        assert_eq!(
            back.instances[1].failure_reason,
            Some(FailureReason::NoSuchSopInstance)
        );
        assert_eq!(
            back.instances[1].failure_reason.expect("reason").code(),
            "0112"
        );
        assert!(!back.all_committed());
    }

    #[test]
    fn a_study_shaped_record_keeps_the_study_uid_and_the_form() {
        // The form is what makes Check answer in the same shape as Commit
        // (Annex J.2), and the study UID is what lets the study form be
        // rebuilt without walking the archive again.
        let mut rec = record();
        rec.request_form = RequestForm::Study;
        let mut item = committed("1.2.840.10008.5.1.4.1.1.2", "2.25.1");
        item.study_uid = Some("1.2.3.4.5.6.7".to_string());
        rec.instances = vec![item];
        let bytes = rec.encode().expect("encode");
        let back = CommitmentRecord::decode(&bytes).expect("decode");
        assert_eq!(back.request_form, RequestForm::Study);
        assert_eq!(
            back.instances[0].study_uid.as_deref(),
            Some("1.2.3.4.5.6.7")
        );
    }

    #[test]
    fn an_optional_failure_reason_is_absent_for_a_committed_instance() {
        // Writing the key unconditionally would put a Failure Reason on a
        // committed instance, which Annex J.2's split between the two
        // sequences forbids; the decoder refuses it, so the writer must not
        // emit it. This pins the writing side.
        let bytes = record().encode().expect("encode");
        let text = String::from_utf8_lossy(&bytes);
        // The reason codes are the only text in the record beginning "011";
        // a committed-only record must contain none of them.
        assert!(
            !text.contains("0112") && !text.contains("0110"),
            "a committed record must carry no failure reason"
        );
    }

    #[test]
    fn a_committed_instance_carrying_a_reason_is_refused() {
        let mut rec = record();
        rec.instances[0].failure_reason = Some(FailureReason::NoSuchSopInstance);
        let bytes = rec.encode().expect("encode");
        assert!(matches!(
            CommitmentRecord::decode(&bytes),
            Err(RecordError::Invalid(_))
        ));
    }

    #[test]
    fn a_failed_instance_without_a_reason_is_refused() {
        let mut rec = record();
        rec.instances[0].outcome = CommitmentOutcome::Failed;
        let bytes = rec.encode().expect("encode");
        assert!(matches!(
            CommitmentRecord::decode(&bytes),
            Err(RecordError::Invalid(_))
        ));
    }

    #[test]
    fn a_record_missing_its_identity_is_refused() {
        // The Transaction UID is the file's own name; a record that does not
        // carry it cannot be addressed, so it is malformed rather than empty.
        let rec = CommitmentRecord {
            transaction_uid: String::new(),
            requested_at: "2026-09-23T10:00:00Z".to_string(),
            request_form: RequestForm::Instance,
            instances: vec![],
        };
        let bytes = rec.encode().expect("encode");
        // An empty UID still round-trips (it is a value, not an absence) -
        // what must NOT decode is a record with the key missing entirely.
        let back = CommitmentRecord::decode(&bytes).expect("decode");
        assert!(back.transaction_uid.is_empty());

        let key = K_TRANSACTION_UID;
        let mut buf = vec![0u8; 256];
        let written = {
            let mut w = CborWriter::new(&mut buf);
            w.write_map_header(2).expect("header");
            w.write_u64(K_REQUESTED_AT).expect("key");
            w.write_text("2026-09-23T10:00:00Z").expect("val");
            w.write_u64(K_REQUEST_FORM).expect("key");
            w.write_u64(1).expect("val");
            w.written()
        };
        let _ = key;
        assert!(matches!(
            CommitmentRecord::decode(&buf[..written]),
            Err(RecordError::Malformed(_))
        ));
    }

    #[test]
    fn an_unknown_top_level_key_is_skipped() {
        // Forward tolerance: a later build may append a key, and an earlier
        // one reading the file must not reject it.
        let mut buf = vec![0u8; 256];
        let written = {
            let mut w = CborWriter::new(&mut buf);
            w.write_map_header(5).expect("header");
            w.write_u64(K_TRANSACTION_UID).expect("k");
            w.write_text("1.2.3").expect("v");
            w.write_u64(K_REQUESTED_AT).expect("k");
            w.write_text("2026-09-23T10:00:00Z").expect("v");
            w.write_u64(K_REQUEST_FORM).expect("k");
            w.write_u64(1).expect("v");
            w.write_u64(K_INSTANCES).expect("k");
            w.write_array_header(0).expect("v");
            // The unknown key, carrying a nested value the decoder must skip
            // whole.
            w.write_u64(99).expect("k");
            w.write_map_header(1).expect("v");
            w.write_u64(1).expect("k");
            w.write_text("ignored").expect("v");
            w.written()
        };
        let rec = CommitmentRecord::decode(&buf[..written]).expect("decode");
        assert_eq!(rec.transaction_uid, "1.2.3");
    }

    #[test]
    fn an_empty_instance_list_round_trips() {
        // Not a valid DICOMweb request (Annex J.1 requires one or more items),
        // but the RECORD must be able to express it: the store is also where a
        // rejected request would be logged, and a decoder that refused empty
        // would make that impossible.
        let rec = CommitmentRecord {
            transaction_uid: "1.2.3".to_string(),
            requested_at: "2026-09-23T10:00:00Z".to_string(),
            request_form: RequestForm::Instance,
            instances: vec![],
        };
        let bytes = rec.encode().expect("encode");
        let back = CommitmentRecord::decode(&bytes).expect("decode");
        assert!(back.instances.is_empty());
        // And "all committed" is FALSE for an empty set: an empty success
        // sequence is not a success, it is a request that referenced nothing.
        assert!(!back.all_committed());
    }

    #[test]
    fn a_zero_request_form_is_refused() {
        let mut buf = vec![0u8; 128];
        let written = {
            let mut w = CborWriter::new(&mut buf);
            w.write_map_header(4).expect("header");
            w.write_u64(K_TRANSACTION_UID).expect("k");
            w.write_text("1.2.3").expect("v");
            w.write_u64(K_REQUESTED_AT).expect("k");
            w.write_text("now").expect("v");
            w.write_u64(K_REQUEST_FORM).expect("k");
            w.write_u64(0).expect("v");
            w.write_u64(K_INSTANCES).expect("k");
            w.write_array_header(0).expect("v");
            w.written()
        };
        assert!(matches!(
            CommitmentRecord::decode(&buf[..written]),
            Err(RecordError::Malformed(_))
        ));
    }

    #[test]
    fn processing_failure_and_no_such_instance_are_distinct_codes() {
        // The two reasons answer different questions - "not held" versus
        // "could not be determined" - so they must not share a code.
        assert_eq!(FailureReason::NoSuchSopInstance.code(), "0112");
        assert_eq!(FailureReason::ProcessingFailure.code(), "0110");
        assert_ne!(
            FailureReason::NoSuchSopInstance.code(),
            FailureReason::ProcessingFailure.code()
        );
    }

    #[test]
    fn a_large_instance_list_is_not_truncated_by_a_fixed_buffer() {
        // The encode buffer is sized from the instance count, so a study-sized
        // commitment must not hit BufferFull. 2000 instances is well past any
        // real study and inside the 64 MiB result budget.
        let mut rec = record();
        rec.instances = (0..2000)
            .map(|i| committed("1.2.840.10008.5.1.4.1.1.2", &format!("2.25.{i}")))
            .collect();
        let bytes = rec.encode().expect("a big record must encode");
        let back = CommitmentRecord::decode(&bytes).expect("decode");
        assert_eq!(back.instances.len(), 2000);
        assert!(back.all_committed());
    }

    /// The size estimate must count every string the record writes.
    ///
    /// It used to size only the two UID fields, leaving out the transaction
    /// UID, the request timestamp, and each instance's optional study UID and
    /// failure reason. A record whose payload was mostly those fields still
    /// hit `BufferFull`, and a record that cannot be encoded can never be
    /// stored - the commitment silently failed. The worst case here is a
    /// failure list where every entry carries both optional fields.
    #[test]
    fn a_record_of_failures_with_study_uids_encodes() {
        // Long UIDs stress the text accounting; PS3.5 caps a UID at 64 chars.
        let long_uid = |prefix: &str| format!("{prefix}{}", "9".repeat(48));
        let mut rec = record();
        rec.transaction_uid = long_uid("1.2.3.4.5.");
        rec.instances = (0..500)
            .map(|i| InstanceResult {
                sop_class_uid: long_uid("1.2.840.10008.5.1.4.1.1."),
                sop_instance_uid: long_uid(&format!("2.25.{i}.")),
                outcome: CommitmentOutcome::Failed,
                failure_reason: Some(FailureReason::ProcessingFailure),
                study_uid: Some(long_uid("1.3.6.1.4.1.")),
            })
            .collect();

        let bytes = rec.encode().expect("every string must be accounted for");
        let back = CommitmentRecord::decode(&bytes).expect("decode");
        assert_eq!(back.instances.len(), 500);
        assert!(
            back.instances
                .iter()
                .all(|i| i.failure_reason == Some(FailureReason::ProcessingFailure)),
            "the optional failure reasons must survive the round trip"
        );
        assert!(
            back.instances.iter().all(|i| i.study_uid.is_some()),
            "the optional study UIDs must survive the round trip"
        );
        assert_eq!(back.transaction_uid, rec.transaction_uid);
    }
}
