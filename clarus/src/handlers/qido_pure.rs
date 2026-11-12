// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Pure (non-axum) QIDO-RS + health logic.
//!
//! The axum handlers in `qido.rs` are thin adapters: they map
//! `HeaderMap -> Option<&str>`, call the pure functions here, and render the
//! returned `QidoResp` / `QidoError` into a `Response`. This module has zero
//! HTTP-framework tokens and no non-blocking functions (grep-checkable), so
//! the sync logic is unchanged by the transport swap.
//!
//! # Invariants (carried over from `qido.rs`)
//! - INV-001: Query parameters are validated BEFORE reaching any SQL.
//! - INV-002: Wildcard `*` -> SQL `%`, `?` -> `_`.
//! - INV-003: `PatientName` <= 64 chars (Unicode-aware `chars().count()`).
//! - INV-006: `unsafe` forbidden; `unwrap()` forbidden in the wrapper handlers.

#![forbid(unsafe_code)]

use super::{DICOM_JSON_CONTENT_TYPE, DICOM_XML_CONTENT_TYPE};
use crate::state::AppState;

// -- Status constants (u16 - no http::StatusCode in pure code) ------------

/// 200 OK.
pub const STATUS_OK: u16 = 200;
/// 400 Bad Request - a validation error.
pub const STATUS_BAD_REQUEST: u16 = 400;
/// 500 Internal Server Error.
pub const STATUS_INTERNAL_SERVER_ERROR: u16 = 500;
/// 503 Service Unavailable - health only.
pub const STATUS_SERVICE_UNAVAILABLE: u16 = 503;
/// `application/json` - used for non-QIDO error bodies (matches axum `Json`).
const APPLICATION_JSON: &str = "application/json";

// -- Return types ---------------------------------------------------------

/// A complete QIDO-RS response the wrapper turns into an axum `Response`.
///
/// Header order is NOT part of the contract (HTTP does not guarantee it);
/// the wrapper adds `x-pragma` / `x-clarus` / `Warning` only when `status` is
/// 200 and emits `Content-Type` from `content_type`.
pub struct QidoResp {
    /// HTTP status. 200 for success, 500 for a surfaced error body.
    pub status: u16,
    /// Response content type (`DICOM_JSON_CONTENT_TYPE` | XML | JSON error).
    pub content_type: &'static str,
    /// Result count >= limit -> the response is truncated. Retained so the
    /// existing constructors keep their meaning; `remaining` is the precise
    /// value the PS3.18 8.3.4.4.1 Warning needs.
    pub truncated: bool,
    /// PS3.18 8.3.4.4.1 truncation count: `Some(remaining)` when more matches
    /// exist beyond the returned page, `None` otherwise. The wrapper emits
    /// `Warning: 299 <host>: There are <remaining> additional results that can
    /// be requested` only when this is `Some(n)`.
    pub remaining: Option<u64>,
    /// Serialized response body (the wire bytes).
    pub body: Vec<u8>,
}

/// A validation or internal error rendered as `{"error":"<message>"}`.
#[derive(Debug)]
pub struct QidoError {
    /// HTTP status (400 | 500).
    pub status: u16,
    /// Message rendered exactly as `{"error":"<message>"}`.
    pub message: String,
}

// -- Accept negotiation --------------------------------------------------

/// Choose the response content type from the `Accept` header.
///
/// Simple substring check - no full MIME parsing (P26 deferred - quality
/// factors `q=...` per RFC 7231 Section 5.3.1 are NOT parsed yet). Returns XML when
/// the client explicitly asks for `application/dicom+xml`, else JSON.
pub fn negotiate(accept: Option<&str>) -> &'static str {
    if let Some(a) = accept
        && a.contains(DICOM_XML_CONTENT_TYPE)
    {
        return DICOM_XML_CONTENT_TYPE;
    }
    DICOM_JSON_CONTENT_TYPE
}

// -- Query parameter validation --------------------------------------------

/// Maximum length of a PatientName query parameter, in Unicode characters.
/// DICOM PN VR allows up to 64 characters per component group.
/// Using `.chars().count()` ensures Cyrillic (2 bytes/char) and ASCII (1 byte/char)
/// get the same 64-character limit.
const PATIENT_NAME_MAX_LEN: usize = 64;

/// Decode a URL-encoded string (percent-encoding).
fn url_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            if let Ok(decoded) =
                u8::from_str_radix(std::str::from_utf8(&[hi, lo]).unwrap_or("00"), 16)
            {
                result.push(decoded);
            }
        } else if b == b'+' {
            result.push(b' ');
        } else {
            result.push(b);
        }
    }
    // UTF-8 multi-byte sequences decode into BYTES, not Latin-1 scalars:
    // %D0%BC is "м", not "Ð¼". Lossy is fine - a malformed sequence can
    // never match a stored UTF-8 name anyway.
    String::from_utf8_lossy(&result).into_owned()
}

/// StudyDate in compact DICOM form: `YYYYMMDD`, `YYYYMMDD-YYYYMMDD`, or a
/// range with one end absent (`YYYYMMDD-`, `-YYYYMMDD`).
/// INV-003: ISO 8601 date-times with hyphens stay rejected.
const STUDY_DATE_LEN_EXACT: usize = 8;
const STUDY_DATE_LEN_RANGE: usize = 17; // "YYYYMMDD-YYYYMMDD" = 17 chars

/// One side of a date range: empty (absent) or exactly 8 digits (DA).
fn study_date_bound(bound: &[u8]) -> bool {
    bound.is_empty()
        || (bound.len() == STUDY_DATE_LEN_EXACT && bound.iter().all(u8::is_ascii_digit))
}

/// A parsed range end, keeping `None` for the absent side.
fn bound(side: &str) -> Option<String> {
    if side.is_empty() {
        None
    } else {
        Some(side.to_string())
    }
}

/// Manual StudyDate validation without regex dependency.
///
/// PS3.4 C.2.2.2.5 range matching is `lower-upper` with either side allowed to
/// be absent, so "everything since a date" is a legal key. Rejecting the
/// open-ended forms left that question unaskable and forced a client to
/// invent an upper bound it does not know.
///
/// Accepts: `20200101` (exact), `20200101-20201231` (closed range),
/// `20200101-` (open above), `-20201231` (open below). A bare `-` constrains
/// nothing - both ends absent - so it is rejected rather than silently
/// matching everything, which would turn a malformed key into a full scan.
fn validate_study_date(date: &str) -> bool {
    // Wildcard: match all - skip validation
    if date == "*" {
        return true;
    }
    let bytes = date.as_bytes();
    match date.len() {
        STUDY_DATE_LEN_EXACT => study_date_bound(bytes),
        STUDY_DATE_LEN_RANGE => {
            study_date_bound(&bytes[0..8]) && bytes[8] == b'-' && study_date_bound(&bytes[9..17])
        }
        // "YYYYMMDD-" and "-YYYYMMDD" are 9 characters: one 8-digit bound and
        // the separator.
        9 => match bytes[8] {
            b'-' => study_date_bound(&bytes[0..8]) && !bytes[0..8].is_empty(),
            _ if bytes[0] == b'-' => study_date_bound(&bytes[1..9]),
            _ => false,
        },
        _ => false,
    }
}

/// Validate StudyTime format: `HHMMSS` or `HHMMSS.ffffff` (DICOM TM VR).
/// Per DICOM PS3.5, TM allows optional fractional seconds after a dot.
fn validate_study_time(time: &str) -> bool {
    // Must be at least 6 chars (HHMMSS)
    if time.len() < 6 {
        return false;
    }
    let bytes = time.as_bytes();
    // First 6 chars must be ASCII digits
    if !bytes[0..6].iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    // Optional fractional part: dot followed by 1-6 digits
    if time.len() > 6 {
        if bytes[6] != b'.' {
            return false;
        }
        if time.len() == 7 {
            return false; // dot with no digits
        }
        if !bytes[7..].iter().all(|b| b.is_ascii_digit()) {
            return false;
        }
        // Max 6 fractional digits per DICOM TM
        if time.len() > 13 {
            return false;
        }
    }
    true
}

/// QIDO-RS query parameters (validated, never interpolated into SQL directly).
///
/// Corresponds to DICOMweb PS3.18 Table 8.3.3-1 search attributes.
/// Field names use PascalCase to match DICOMweb query parameter conventions.
/// Each field also accepts the hex tag DICOM keyword per PS3.18 Section 8.3.3.
#[derive(Debug)]
#[allow(non_snake_case)]
pub struct QidoQuery {
    /// PatientName (0010,0010). Wildcard `*` -> `%`, max 64 chars.
    pub PatientName: Option<String>,
    /// PatientID (0010,0020).
    pub PatientID: Option<String>,
    /// StudyDate (0008,0020) range: `YYYYMMDD[-YYYYMMDD]`.
    pub StudyDate: Option<String>,
    /// Modality (0008,0060) comma-separated: `CT,MR`.
    pub Modality: Option<String>,
    /// AccessionNumber (0008,0050). Wildcard `*` -> `%`.
    pub AccessionNumber: Option<String>,
    /// StudyDescription (0008,1030).
    pub StudyDescription: Option<String>,
    /// PatientBirthDate (0010,0030). Regex `^\d{8}$`.  - G-02
    pub PatientBirthDate: Option<String>,
    /// PatientSex (0010,0040). `M`/`F`/`O`.  - G-02
    pub PatientSex: Option<String>,
    /// ReferringPhysicianName (0008,0090). Wildcard <=64 chars.  - G-02
    pub ReferringPhysicianName: Option<String>,
    /// InstitutionName (0008,0080). Wildcard. Vendor extension 08.09.
    pub InstitutionName: Option<String>,
    /// RequestingPhysician (0032,1032). PN wildcard <=64 chars.
    /// Vendor extension 08.09; ReferringPhysicianName template.
    pub RequestingPhysician: Option<String>,
    /// RequestedProcedureDescription (0032,1060). Wildcard.
    /// Vendor extension 08.09.
    pub RequestedProcedureDescription: Option<String>,
    /// PerformingPhysicianName (0008,1050). Wildcard <=64 chars.
    pub PerformingPhysicianName: Option<String>,
    /// NameOfPhysiciansReadingStudy (0008,1060). Wildcard <=64 chars.
    pub NameOfPhysiciansReadingStudy: Option<String>,
    /// ResponsiblePerson (0010,2297). Wildcard <=64 chars. PN veterinary
    /// search key (vendor extension, FR-vet stage 1; accepted but not
    /// returned in responses).
    pub ResponsiblePerson: Option<String>,
    /// RequestAttributesSequence > ScheduledProcedureStepID (0040,0009).
    /// REQUIRED series matching key - PS3.18 Table 10.6.1-5 spells it as the
    /// path `00400275.00400009`.
    pub ScheduledProcedureStepID: Option<String>,
    /// RequestAttributesSequence > RequestedProcedureID (0040,1001).
    /// The second REQUIRED nested series key of the same table.
    pub RequestedProcedureIDInSequence: Option<String>,
    /// OtherPatientIDsSequence > PatientID (0010,1002 > 0010,0020).
    /// MULTI-VALUED study key - matches when ANY listed identifier matches.
    pub OtherPatientIDs: Option<String>,
    /// StudyInstanceUID (0020,000D). UID validation.  - G-02
    pub StudyInstanceUID: Option<String>,
    /// StudyTime (0008,0030). Regex `^\d{6}(\.\d+)?$`.  - G-02
    pub StudyTime: Option<String>,
    /// StudyID (0020,0010). Wildcard.  - G-02
    pub StudyID: Option<String>,
    /// SeriesInstanceUID (0020,000E). UID validation.  - G-02
    pub SeriesInstanceUID: Option<String>,
    /// SeriesNumber (0020,0011). Wildcard.  - Table 10.6.1-5 delta
    pub SeriesNumber: Option<String>,
    /// SOPInstanceUID (0008,0018). UID validation.  - G-02
    pub SOPInstanceUID: Option<String>,
    /// SOPClassUID (0008,0016). UID validation.  - Table 10.6.1-5
    pub SOPClassUID: Option<String>,
    /// InstanceNumber (0020,0013).  - Table 10.6.1-5
    pub InstanceNumber: Option<String>,
    /// ModalitiesInStudy (0008,0061). Comma-separated.  - R-08
    pub ModalitiesInStudy: Option<String>,
    /// SeriesDate (0008,0021). Regex `^\d{8}$`.  - R-07
    pub SeriesDate: Option<String>,
    /// SeriesTime (0008,0031). Regex `^\d{6}(\.\d+)?$`.  - R-07
    pub SeriesTime: Option<String>,
    /// SeriesDescription (0008,103E). Wildcard.  - R-07
    pub SeriesDescription: Option<String>,
    /// Manufacturer (0008,0070). Wildcard. Vendor extension 08.09.
    pub Manufacturer: Option<String>,
    /// InstitutionalDepartmentName (0008,1040). Wildcard. Vendor extension 08.09.
    pub InstitutionalDepartmentName: Option<String>,
    /// OperatorsName (0008,1070). PN wildcard <=64 chars.
    /// Vendor extension 08.09; PerformingPhysicianName template.
    pub OperatorsName: Option<String>,
    /// ProtocolName (0018,1030). Wildcard. Vendor extension 08.09.
    pub ProtocolName: Option<String>,
    /// StationName (0008,1010). Wildcard `*`/`?`. Vendor extension 08.09
    /// (dcm4chee/Orthanc parity), declared in docs/CONFORMANCE.md.
    pub StationName: Option<String>,
    /// BodyPartExamined (0018,0015). Wildcard `*`/`?`. Vendor extension 08.09.
    pub BodyPartExamined: Option<String>,
    /// PerformedProcedureStepStartDate (0040,0244). Regex `^\d{8}$`.
    pub PerformedProcedureStepStartDate: Option<String>,
    /// PerformedProcedureStepStartTime (0040,0245). Regex `^\d{6}(\.\d+)?$`.
    pub PerformedProcedureStepStartTime: Option<String>,
    /// RequestAttributesSequence (0040,0275). SQ: accepted, no scalar match.
    pub RequestAttributesSequence: Option<String>,
    /// Maximum results to return. Default: 250.
    pub limit: Option<u64>,
    /// Zero-based offset. Default: 0.
    pub offset: Option<u64>,
    /// Comma-separated tag hex strings to include in response: `00100010,00080060`.
    /// Accepts both comma-separated and repeated-key forms.
    pub includefield: Option<String>,
    /// Fuzzy matching toggle.  - G-12
    pub fuzzymatching: Option<bool>,
    /// PS3.18 8.3.9.2 `emptyvaluematching`: parsed case-insensitively and
    /// accepted. Honest no-op for the B-tree tier: the manifest stores no empty
    /// single-value attributes, so an empty stored value never exists to match.
    pub emptyvaluematching: Option<bool>,
    /// PS3.18 8.3.9.2 `multiplevaluematching`: parsed case-insensitively and
    /// accepted. Conforms to the B-tree matcher without a toggle: for the CSV
    /// attributes (Modality / ModalitiesInStudy) the matcher already tests whether
    /// ANY requested component is present among the stored components. `false`/absent
    /// is not a single-value narrowing mode (the any-component rule is unconditional).
    pub multiplevaluematching: Option<bool>,
}

fn default_offset() -> Option<u64> {
    Some(0)
}

impl Default for QidoQuery {
    fn default() -> Self {
        Self {
            PatientName: None,
            PatientID: None,
            StudyDate: None,
            Modality: None,
            AccessionNumber: None,
            StudyDescription: None,
            PatientBirthDate: None,
            PatientSex: None,
            ReferringPhysicianName: None,
            InstitutionName: None,
            RequestingPhysician: None,
            RequestedProcedureDescription: None,
            PerformingPhysicianName: None,
            NameOfPhysiciansReadingStudy: None,
            ResponsiblePerson: None,
            ScheduledProcedureStepID: None,
            RequestedProcedureIDInSequence: None,
            OtherPatientIDs: None,
            StudyInstanceUID: None,
            StudyTime: None,
            StudyID: None,
            SeriesInstanceUID: None,
            SeriesNumber: None,
            SOPInstanceUID: None,
            SOPClassUID: None,
            InstanceNumber: None,
            ModalitiesInStudy: None,
            SeriesDate: None,
            SeriesTime: None,
            SeriesDescription: None,
            Manufacturer: None,
            InstitutionalDepartmentName: None,
            OperatorsName: None,
            ProtocolName: None,
            StationName: None,
            BodyPartExamined: None,
            PerformedProcedureStepStartDate: None,
            PerformedProcedureStepStartTime: None,
            RequestAttributesSequence: None,
            // No explicit `limit`/`offset` over the wire -> `resolve_limit`
            // applies `[query] default_limit` at the handler level.
            limit: None,
            offset: default_offset(),
            includefield: None,
            fuzzymatching: None,
            emptyvaluematching: None,
            multiplevaluematching: None,
        }
    }
}

/// Resolve the effective QIDO result limit - the single source of truth.
///
/// * `Some(l)` explicit client limit is honored **without clamping**.
/// * `Some(0)` is honored as 0: PS3.18 8.3.4.4.1 computes
///   `results = min(max(0, matches - offset), maxResults, limit)`, so an
///   explicit `limit=0` forces `results = 0` -> 200 with an empty payload.
///   Treating it as "unlimited" returned the full set, which is wrong.
/// * absent + `[query] default_limit == 0` -> `None` (unlimited).
/// * absent + nonzero default -> `[query] default_limit`.
///
/// `None` = unlimited.
fn resolve_limit(query: &QidoQuery, cfg: &crate::config::QueryConfig) -> Option<u64> {
    match query.limit {
        Some(l) => Some(l),
        None => {
            if cfg.default_limit == 0 {
                None
            } else {
                Some(cfg.default_limit)
            }
        }
    }
}

/// Resolve the limit for SCOPED resource enumeration
/// (`/studies/{s}/series`, `/studies/{s}/series/{se}/instances`,
/// `/studies/{s}/instances`).
///
/// These resources are bounded by the path (one study / one series), not an
/// open-ended search - `[query] default_limit` does NOT apply here (12.09:
/// MicroDicom 250-cap root cause - it enumerates a study's series / a series'
/// instances for download and took the search cap as the full list). Only an
/// explicit client `limit` is honored; `Some(0)` and absent both mean
/// unlimited.
fn resolve_enumeration_limit(query: &QidoQuery) -> Option<u64> {
    match query.limit {
        Some(0) => None,
        Some(l) => Some(l),
        None => None,
    }
}

/// Validation error returned when query parameters don't meet invariants.
/// The exact message strings are part of the wire contract (golden fixtures).
#[derive(Debug)]
enum ValidationError {
    PatientNameTooLong(usize),
    InvalidStudyDate(String),
    InvalidPatientBirthDate(String),
    InvalidPatientSex(String),
    InvalidStudyTime(String),
    InvalidPerformedStepStartDate(String),
    InvalidPerformedStepStartTime(String),
    InvalidDicomUid(String),
    ReferringPhysicianTooLong(usize),
    RequestingPhysicianTooLong(usize),
    OperatorsNameTooLong(usize),
    PerformingPhysicianTooLong(usize),
    NameOfReadingPhysiciansTooLong(usize),
}

impl ValidationError {
    /// Render this validation error as a [`QidoError`] (400, `{"error":...}`).
    fn into_qido_error(self) -> QidoError {
        let message = match self {
            ValidationError::PatientNameTooLong(len) => {
                format!("PatientName must be ≤ {PATIENT_NAME_MAX_LEN} characters (got {len})")
            }
            ValidationError::InvalidStudyDate(date) => {
                format!("StudyDate must match pattern YYYYMMDD[-YYYYMMDD] (got: {date})")
            }
            ValidationError::InvalidPatientBirthDate(date) => {
                format!("PatientBirthDate must match pattern YYYYMMDD[-YYYYMMDD] (got: {date})")
            }
            ValidationError::InvalidPatientSex(sex) => {
                format!("PatientSex must be M, F, or O (got: {sex})")
            }
            ValidationError::InvalidStudyTime(time) => {
                format!("StudyTime must match pattern HHMMSS[.ffffff] (got: {time})")
            }
            ValidationError::InvalidPerformedStepStartDate(date) => {
                format!("PerformedProcedureStepStartDate must match pattern YYYYMMDD (got: {date})")
            }
            ValidationError::InvalidPerformedStepStartTime(time) => format!(
                "PerformedProcedureStepStartTime must match pattern HHMMSS[.ffffff] (got: {time})"
            ),
            ValidationError::InvalidDicomUid(uid) => format!("Invalid DICOM UID: {uid}"),
            ValidationError::ReferringPhysicianTooLong(len) => format!(
                "ReferringPhysicianName must be ≤ {PATIENT_NAME_MAX_LEN} characters (got {len})"
            ),
            ValidationError::RequestingPhysicianTooLong(len) => format!(
                "RequestingPhysician must be ≤ {PATIENT_NAME_MAX_LEN} characters (got {len})"
            ),
            ValidationError::OperatorsNameTooLong(len) => {
                format!("OperatorsName must be ≤ {PATIENT_NAME_MAX_LEN} characters (got {len})")
            }
            ValidationError::PerformingPhysicianTooLong(len) => format!(
                "PerformingPhysicianName must be <= {PATIENT_NAME_MAX_LEN} characters (got {len})"
            ),
            ValidationError::NameOfReadingPhysiciansTooLong(len) => format!(
                "NameOfPhysiciansReadingStudy must be <= {PATIENT_NAME_MAX_LEN} characters (got {len})"
            ),
        };
        QidoError {
            status: STATUS_BAD_REQUEST,
            message,
        }
    }
}

impl QidoQuery {
    /// Validate query parameters. Returns `Err(QidoError)` (400) on violation.
    /// INV-001: validation happens BEFORE any SQL is constructed.
    fn validate(&self) -> Result<(), QidoError> {
        // INV-003: PatientName <= 64 characters (Unicode-aware)
        if let Some(ref name) = self.PatientName
            && name.chars().count() > PATIENT_NAME_MAX_LEN
        {
            return Err(ValidationError::PatientNameTooLong(name.chars().count()).into_qido_error());
        }

        // INV-003: StudyDate must match YYYYMMDD or YYYYMMDD-YYYYMMDD
        if let Some(ref date) = self.StudyDate
            && !validate_study_date(date)
        {
            return Err(ValidationError::InvalidStudyDate(date.clone()).into_qido_error());
        }

        // 08.09: PatientBirthDate now accepts the same forms as StudyDate -
        // `YYYYMMDD` (exact) or `YYYYMMDD-YYYYMMDD` (range); `*` matches all.
        // Reuses the StudyDate validator end-to-end (dcm4chee/Orthanc parity).
        if let Some(ref bdate) = self.PatientBirthDate
            && !validate_study_date(bdate)
        {
            return Err(ValidationError::InvalidPatientBirthDate(bdate.clone()).into_qido_error());
        }

        // G-02: PatientSex must be M, F, or O
        if let Some(ref sex) = self.PatientSex {
            let upper = sex.to_ascii_uppercase();
            if upper != "M" && upper != "F" && upper != "O" {
                return Err(ValidationError::InvalidPatientSex(sex.clone()).into_qido_error());
            }
        }

        // G-02: StudyTime must match HHMMSS[.ffffff]
        if let Some(ref time) = self.StudyTime
            && !validate_study_time(time)
        {
            return Err(ValidationError::InvalidStudyTime(time.clone()).into_qido_error());
        }

        // PerformedProcedureStepStartDate (0040,0244) is DA `YYYYMMDD`.
        if let Some(ref d) = self.PerformedProcedureStepStartDate
            && (d.len() != 8 || !d.chars().all(|c| c.is_ascii_digit()))
        {
            return Err(ValidationError::InvalidPerformedStepStartDate(d.clone()).into_qido_error());
        }

        // PerformedProcedureStepStartTime (0040,0245) is TM `HHMMSS[.ffffff]`.
        if let Some(ref t) = self.PerformedProcedureStepStartTime
            && !validate_study_time(t)
        {
            return Err(ValidationError::InvalidPerformedStepStartTime(t.clone()).into_qido_error());
        }
        // RequestAttributesSequence (0040,0275) is an SQ; there is no
        // scalar QIDO value to validate, so it is accepted without validation.

        // G-02: PerformingPhysicianName <= 64 characters
        if let Some(ref name) = self.PerformingPhysicianName
            && name.chars().count() > PATIENT_NAME_MAX_LEN
        {
            return Err(
                ValidationError::PerformingPhysicianTooLong(name.chars().count()).into_qido_error(),
            );
        }

        // G-02: NameOfPhysiciansReadingStudy <= 64 characters
        if let Some(ref name) = self.NameOfPhysiciansReadingStudy
            && name.chars().count() > PATIENT_NAME_MAX_LEN
        {
            return Err(
                ValidationError::NameOfReadingPhysiciansTooLong(name.chars().count())
                    .into_qido_error(),
            );
        }
        // G-02: ReferringPhysicianName <= 64 characters
        if let Some(ref name) = self.ReferringPhysicianName
            && name.chars().count() > PATIENT_NAME_MAX_LEN
        {
            return Err(
                ValidationError::ReferringPhysicianTooLong(name.chars().count()).into_qido_error(),
            );
        }

        // 08.09: RequestingPhysician follows the ReferringPhysicianName
        // template (PN, <= 64 characters).
        if let Some(ref name) = self.RequestingPhysician
            && name.chars().count() > PATIENT_NAME_MAX_LEN
        {
            return Err(
                ValidationError::RequestingPhysicianTooLong(name.chars().count()).into_qido_error(),
            );
        }

        // 08.09: OperatorsName follows the PerformingPhysicianName template
        // (PN, <= 64 characters).
        if let Some(ref name) = self.OperatorsName
            && name.chars().count() > PATIENT_NAME_MAX_LEN
        {
            return Err(
                ValidationError::OperatorsNameTooLong(name.chars().count()).into_qido_error()
            );
        }

        // G-02: UID validation for StudyInstanceUID, SeriesInstanceUID, SOPInstanceUID
        if let Some(ref uid) = self.StudyInstanceUID {
            validate_uid(uid)?;
        }
        if let Some(ref uid) = self.SeriesInstanceUID {
            validate_uid(uid)?;
        }
        if let Some(ref uid) = self.SOPInstanceUID {
            validate_uid(uid)?;
        }
        // Table 10.6.1-5: SOPClassUID is a matching UID key.
        if let Some(ref uid) = self.SOPClassUID {
            validate_uid(uid)?;
        }
        // InstanceNumber mirrors SeriesNumber: both accept any string
        // (wildcards `*`/`?` pass through to the glob matcher) - no
        // additional validation here.

        Ok(())
    }

    /// Parse StudyDate range into (from, to) tuple.
    /// `20200101-20201231` -> `("20200101", "20201231")`.
    /// `20200101` (exact match) -> `("20200101", "20200101")`.
    /// `20200101-` -> `("20200101", None)`, `-20201231` -> `(None, "20201231")`.
    fn study_date_range(&self) -> (Option<String>, Option<String>) {
        match &self.StudyDate {
            // Wildcard: no date filter
            Some(date) if date == "*" => (None, None),
            Some(date) if date.contains('-') => {
                let parts: Vec<&str> = date.splitn(2, '-').collect();
                // An absent end is an absent bound, not an empty string: the
                // filter would `parse::<u64>()` it to None anyway, but passing
                // it through as `Some("")` hides the intent at the boundary.
                (bound(parts[0]), parts.get(1).copied().and_then(bound))
            }
            Some(date) => (Some(date.clone()), Some(date.clone())),
            None => (None, None),
        }
    }

    /// Parse PatientBirthDate range into (from, to) `YYYYMMDD` strings -
    /// the exact mirror of [`Self::study_date_range`] (08.09 extension).
    fn patient_birth_date_range(&self) -> (Option<String>, Option<String>) {
        match &self.PatientBirthDate {
            // Wildcard: no date filter
            Some(date) if date == "*" => (None, None),
            Some(date) if date.contains('-') => {
                let parts: Vec<&str> = date.splitn(2, '-').collect();
                (
                    Some(parts[0].to_string()),
                    parts.get(1).copied().map(String::from),
                )
            }
            Some(date) => (Some(date.clone()), Some(date.clone())),
            None => (None, None),
        }
    }
}

/// Validate that a string looks like a DICOM UID (digits and dots only, non-empty).
/// Rejects path-traversal attempts like `../../etc/passwd`.
fn validate_uid(uid: &str) -> Result<(), QidoError> {
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(ValidationError::InvalidDicomUid(uid.to_string()).into_qido_error());
    }
    Ok(())
}

/// Parse a case-insensitive ABNF boolean query value (PS3.18 8.3.4). `true` /
/// `false` in any case -> `Some(bool)`; anything else (incl. absent) -> `None`.
/// Used by the non-search modifiers: fuzzymatching, emptyvaluematching,
/// multiplevaluematching.
fn parse_query_bool(v: &str) -> Option<bool> {
    let t = v.trim();
    if t.eq_ignore_ascii_case("true") {
        Some(true)
    } else if t.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

/// Parse a `limit` / `offset` query value (PS3.18 8.3.4 Table 8.3.4-1 declares
/// both as `uint`). An absent value is `None`; a present value that is not an
/// unsigned integer (negative, non-numeric) is an INVALID query parameter and
/// must be answered 400 (PS3.18 8.5), not silently ignored. `u64::from_str`
/// rejects a leading `-` and every non-digit, so it is the whole check.
fn parse_uint_param(name: &str, raw: Option<&str>) -> Result<Option<u64>, QidoError> {
    match raw {
        None => Ok(None),
        Some(v) => v.parse::<u64>().map(Some).map_err(|_| QidoError {
            status: 400,
            message: format!("{name} must be a non-negative integer (got \"{v}\")"),
        }),
    }
}

/// Parse a QidoQuery from the raw query string, handling duplicate keys
/// (e.g. `includefield=00080060&includefield=00100010`) by combining them
/// with commas.
///
/// * Keys are **case-insensitive** (PS3.18 Section 8.3.4.2): both the camelCase
///   keyword and the 8-hex-digit tag alias are accepted in any case.
/// * A key that is not a known search key is IGNORED and logged as one sorted
///   warning - not a 400. DICOM lets clients over-specify, and rejecting that
///   with a 400 broke conformant clients.
fn parse_qido_query(raw_str: &str) -> Result<QidoQuery, QidoError> {
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for pair in raw_str.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let key = url_decode(k).to_ascii_lowercase();
            let val = url_decode(v);
            map.entry(key)
                .and_modify(|existing| {
                    existing.push(',');
                    existing.push_str(&val);
                })
                .or_insert(val);
        }
    }

    // Consume a known key. `kw` is the lowercase keyword, `hex` the lowercase
    // 8-hex tag alias (empty = no alias). Returns the value if either was
    // present and removes it from `map` (so leftover = unknown keys).
    let take = |map: &mut std::collections::HashMap<String, String>, kw: &str, hex: &str| {
        if let Some(v) = map.remove(kw) {
            Some(v)
        } else if !hex.is_empty() {
            map.remove(hex)
        } else {
            None
        }
    };

    // limit/offset are uint (PS3.18 8.3.4 Table 8.3.4-1); a present value that
    // does not parse is a 400, so these are resolved before the literal.
    let limit = parse_uint_param("limit", take(&mut map, "limit", "").as_deref())?;
    let offset = parse_uint_param("offset", take(&mut map, "offset", "").as_deref())?;
    let query = QidoQuery {
        PatientName: take(&mut map, "patientname", "00100010"),
        PatientID: take(&mut map, "patientid", "00100020"),
        StudyDate: take(&mut map, "studydate", "00080020"),
        Modality: take(&mut map, "modality", "00080060"),
        AccessionNumber: take(&mut map, "accessionnumber", "00080050"),
        StudyDescription: take(&mut map, "studydescription", "00081030"),
        PatientBirthDate: take(&mut map, "patientbirthdate", "00100030"),
        PatientSex: take(&mut map, "patientsex", "00100040"),
        ReferringPhysicianName: take(&mut map, "referringphysicianname", "00080090"),
        InstitutionName: take(&mut map, "institutionname", "00080080"),
        RequestingPhysician: take(&mut map, "requestingphysician", "00321032"),
        RequestedProcedureDescription: take(&mut map, "requestedproceduredescription", "00321060"),
        PerformingPhysicianName: take(&mut map, "performingphysicianname", "00081050"),
        NameOfPhysiciansReadingStudy: take(&mut map, "nameofphysiciansreadingstudy", "00081060"),
        ResponsiblePerson: take(&mut map, "responsibleperson", "00102297"),
        StudyInstanceUID: take(&mut map, "studyinstanceuid", "0020000d"),
        StudyTime: take(&mut map, "studytime", "00080030"),
        StudyID: take(&mut map, "studyid", "00200010"),
        SeriesInstanceUID: take(&mut map, "seriesinstanceuid", "0020000e"),
        SeriesNumber: take(&mut map, "seriesnumber", "00200011"),
        SOPInstanceUID: take(&mut map, "sopinstanceuid", "00080018"),
        SOPClassUID: take(&mut map, "sopclassuid", "00080016"),
        InstanceNumber: take(&mut map, "instancenumber", "00200013"),
        ModalitiesInStudy: take(&mut map, "modalitiesinstudy", "00080061"),
        SeriesDate: take(&mut map, "seriesdate", "00080021"),
        SeriesTime: take(&mut map, "seriestime", "00080031"),
        SeriesDescription: take(&mut map, "seriesdescription", "0008103e"),
        Manufacturer: take(&mut map, "manufacturer", "00080070"),
        InstitutionalDepartmentName: take(&mut map, "institutionaldepartmentname", "00081040"),
        OperatorsName: take(&mut map, "operatorsname", "00081070"),
        ProtocolName: take(&mut map, "protocolname", "00181030"),
        StationName: take(&mut map, "stationname", "00081010"),
        BodyPartExamined: take(&mut map, "bodypartexamined", "00180015"),
        PerformedProcedureStepStartDate: take(
            &mut map,
            "performedprocedurestepstartdate",
            "00400244",
        ),
        PerformedProcedureStepStartTime: take(
            &mut map,
            "performedprocedurestepstarttime",
            "00400245",
        ),
        RequestAttributesSequence: take(&mut map, "requestattributessequence", "00400275"),
        // PS3.18 Table 10.6.1-5 spells these two REQUIRED keys as tag paths.
        // The keyword form is the sequence keyword plus the nested attribute's
        // keyword; the hex form is the same path written in digits. Both are
        // accepted, and the bare nested tag is NOT - a bare (0040,0009) names
        // the PerformedProcedureStep attribute, not this one.
        ScheduledProcedureStepID: take(
            &mut map,
            "requestattributessequence.scheduledprocedurestepid",
            "00400275.00400009",
        ),
        RequestedProcedureIDInSequence: take(
            &mut map,
            "requestattributessequence.requestedprocedureid",
            "00400275.00401001",
        ),
        OtherPatientIDs: take(
            &mut map,
            "otherpatientidssequence.patientid",
            "00101002.00100020",
        ),
        limit,
        offset,
        includefield: take(&mut map, "includefield", ""),
        fuzzymatching: take(&mut map, "fuzzymatching", "").and_then(|v| parse_query_bool(&v)),
        emptyvaluematching: take(&mut map, "emptyvaluematching", "")
            .and_then(|v| parse_query_bool(&v)),
        multiplevaluematching: take(&mut map, "multiplevaluematching", "")
            .and_then(|v| parse_query_bool(&v)),
    };

    // Any key left in `map` is not one of our supported search keys. Owner
    // a query may carry keys we cannot express - DICOM
    // lets clients over-specify; rejecting the whole request (old 400
    // "unsupported search keys") broke conformant clients. Instead ignore the
    // unknown keys (matched as before) and log one sorted warning so a
    // genuinely-misspelt key is still operator-visible.
    if !map.is_empty() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        let joined = keys
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        crate::clog_warn!("ignoring unsupported search keys: {joined}");
    }

    Ok(query)
}

/// Translate a validated `QidoQuery` into the B-tree `QueryFilter`.
///
/// `limit` is the already-`resolve_limit`ed value (tier-aligned with the
/// SQLite path); `None` = unlimited.
fn qido_to_btree_filter(
    query: &QidoQuery,
    limit: Option<u64>,
    qcfg: &crate::config::QueryConfig,
) -> clarus_core::query::QueryFilter {
    let (date_from, date_to) = query.study_date_range();
    let (birth_from, birth_to) = query.patient_birth_date_range();
    clarus_core::query::QueryFilter {
        patient_name: query.PatientName.clone(),
        patient_id: query.PatientID.clone(),
        study_date_from: date_from,
        study_date_to: date_to,
        modality: query.Modality.clone(),
        accession_number: query.AccessionNumber.clone(),
        study_description: query.StudyDescription.clone(),
        referring_physician: query.ReferringPhysicianName.clone(),
        institution_name: query.InstitutionName.clone(),
        requesting_physician: query.RequestingPhysician.clone(),
        requested_procedure_description: query.RequestedProcedureDescription.clone(),
        performing_physician: query.PerformingPhysicianName.clone(),
        name_of_reading_physicians: query.NameOfPhysiciansReadingStudy.clone(),
        responsible_person: query.ResponsiblePerson.clone(),
        // (0010,1002 > 0010,0020) is multi-valued: the filter intersects the
        // requested identifiers with the study's comma-joined aggregate.
        other_patient_ids: query.OtherPatientIDs.clone(),
        study_instance_uid: query.StudyInstanceUID.clone(),
        study_time: query.StudyTime.clone(),
        study_id: query.StudyID.clone(),
        birth_date_from: birth_from,
        birth_date_to: birth_to,
        patient_sex: query.PatientSex.clone(),
        modalities_in_study: query.ModalitiesInStudy.clone(),
        series_instance_uid: query.SeriesInstanceUID.clone(),
        series_number: query.SeriesNumber.clone(),
        limit,
        offset: query.offset,
        fuzzymatch: query.fuzzymatching.unwrap_or(false),
        fuzzymatch_override: qcfg.fuzzymatch_override,
        pn_case_insensitive: !qcfg.case_sensitive_pn,
    }
}

/// Detect and enqueue a runtime modality-idx heal for one exact-UID study.
///
/// Called by the exact study-UID QIDO path (a direct `?StudyInstanceUID=`
/// lookup) after the study's manifest has been decoded. Extraction of the
/// study's `modality` + `study date` is followed by a CONFIRMED-ABSENCE check
/// (`idx_fixer::needs_heal`) - a single small bucket read. Only when the study
/// is present in the manifest but ABSENT from its modality-idx bucket is the
/// fixer poked and the detection counted. A healthy study (row present in the
/// bucket) is never counted, so `CLARUS_IDX_RUNTIME_STALE` stays 0 on a
/// healthy archive and 0 = clean holds.
///
/// NOT called by broad queries whose results legitimately bypass the modality
/// index (non-indexed attributes, fuzzymatch), and not called by the
/// series/instances paths (no provenance there) - the heal point is the single
/// study-search location.
fn maybe_heal_study(
    state: &AppState,
    study_uid: &str,
    attrs: &[(u32, clarus_core::manifest::AttrValue)],
) {
    use clarus_core::manifest::AttrValue;
    use clarus_core::query::tag;
    let mut modality = String::new();
    let mut day = [b'0'; 8];
    for (t, v) in attrs {
        match (*t, v) {
            (tag::MODALITY, AttrValue::Text(s)) => modality = s.clone(),
            (tag::STUDY_DATE, AttrValue::UInt(d)) => {
                day = clarus_core::recovery::day_key_from_yyyymmdd(*d)
            }
            _ => {}
        }
    }
    if modality.is_empty() {
        return;
    }
    // Confirm absence BEFORE counting a detect: a healthy study (row present in
    // its modality-idx bucket) must not inflate the live staleness gauge.
    let idx_dir = &state.study_scanner.idx_dir;
    if crate::idx_fixer::needs_heal(idx_dir, &modality, day, study_uid) {
        crate::idx_fixer::request(&modality, day, study_uid);
    }
}

/// Study-level QIDO from the B-tree tier.
///
/// Reads the study manifests + `.idx` and streams each matching study's
/// attributes into a DICOMweb JSON array using the zero-`serde_json::Value`
/// writer from `clarus_core::query::json`.
///
/// the B-tree tier is now the unconditional study oracle - the
/// SQLite shadow tier is excised. A fresh store (no manifests) naturally
/// yields an empty array.
fn search_studies_btree(
    state: &AppState,
    query: &QidoQuery,
    content_type: &'static str,
    retrieve_base: Option<&str>,
) -> QidoResp {
    let limit = resolve_limit(query, &state.config.query);
    let filter = qido_to_btree_filter(query, limit, &state.config.query);
    let (results, total_matches) = match state.study_scanner.search(&filter) {
        Ok(r) => r,
        Err(e) => {
            crate::clog_error!(error = %e, "B-tree QIDO study search failed");
            return QidoResp {
                status: STATUS_INTERNAL_SERVER_ERROR,
                content_type: APPLICATION_JSON,
                truncated: false,
                remaining: None,
                body: br#"{"error":"Internal server error"}"#.to_vec(),
            };
        }
    };

    // Exact-UID query (StudyInstanceUID=...): a study found here is an exact
    // lookup, so poke the runtime fixer when its modality-idx row is missing.
    // Broad queries must NOT be treated as staleness (a legitimate manifest
    // scan for a non-indexed attribute is not a divergence), so only the
    // direct-UID filter triggers detection.
    if filter.study_instance_uid.is_some() {
        for r in &results {
            maybe_heal_study(state, &r.study_uid, &r.qido.attrs);
        }
    }

    // addendum: the B-tree tier must reply in XML itself (the SQLite
    // fallback that used to serve it dies in). 14-tag study render from
    // the typed writer, byte-exact with the frozen study.xml golden.
    let result_count = results.len() as u64;
    let truncated = match limit {
        Some(l) => result_count >= l,
        None => false,
    };
    // PS3.18 8.3.4.4.1: `remaining = matches - offset - returned`, saturating.
    let remaining = qido_remaining(total_matches, query.offset, result_count);
    if let Some(xml) = btree_xml(
        &results,
        |scratch, s| {
            clarus_core::query::xml::write_study_attrs(
                scratch,
                &s.qido,
                query.includefield.as_deref(),
                retrieve_base,
            )
        },
        content_type,
        truncated,
        remaining,
    ) {
        return xml;
    }

    // Stream the result array: `[ {...}, {...} ]`. Each study is rendered into a
    // fixed 4 KiB stack buffer (the C-009 zero-allocation writer) before being
    // appended to the response body.
    //
    // The pre-allocation is sized from the rows that ACTUALLY came back, never
    // from the caller's `limit`. `?limit=10^18` with a client-derived estimate
    // asked the allocator for ~5e20 bytes (abort), and `u64::MAX` overflowed
    // the capacity arithmetic (panic) - one query took the whole server down.
    // The rows are already materialised at this point, so their count is both
    // exact enough and bounded by what the response will really hold.
    let cap = results.len().saturating_mul(500).saturating_add(2);
    let mut body = Vec::with_capacity(cap);
    body.push(b'[');
    let mut scratch = [0u8; 4096];
    for (i, study) in results.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        // A study larger than the stack buffer indicates a pathological
        // manifest; surface it as a 500 rather than silently truncate.
        let n = match clarus_core::query::json::write_study_json(
            &mut scratch,
            &study.qido,
            query.includefield.as_deref(),
            retrieve_base,
        ) {
            Ok(n) => n,
            Err(_) => {
                crate::clog_error!(study_uid = %study.study_uid, "QIDO study exceeded stack buffer");
                return QidoResp {
                    status: STATUS_INTERNAL_SERVER_ERROR,
                    content_type: APPLICATION_JSON,
                    truncated: false,
                    remaining: None,
                    body: br#"{"error":"Study too large to serialize"}"#.to_vec(),
                };
            }
        };
        body.extend_from_slice(&scratch[..n]);
    }
    body.push(b']');

    QidoResp {
        status: STATUS_OK,
        content_type: DICOM_JSON_CONTENT_TYPE,
        truncated,
        remaining,
        body,
    }
}

/// Empty-array response (DICOMweb convention, e.g. unknown study UID).
/// Rendered via axum `Json` in the old handler -> `application/json`.
fn empty_json() -> QidoResp {
    QidoResp {
        status: STATUS_OK,
        content_type: APPLICATION_JSON,
        truncated: false,
        remaining: None,
        body: b"[]".to_vec(),
    }
}

/// Parse the query string (or default) and validate it for `level`, rejecting
/// cross-level search keys as foreign unless they are listed in
/// `allowed_extra`.
///
/// The shared parser accepts every known search key; a key that does not
/// belong to this QIDO level is normally rejected here with a 400 - a
/// silently ignored key would return spuriously wide results, and the study
/// filter's `series_instance_uid`/`series_number` mappings would silently
/// match nothing (their tags never exist in study attrs).
///
/// `allowed_extra` is the relational-scope relaxation: DICOM
/// 10.6.1.2.1 marks certain higher-level (Patient/Study/Series IE) search keys
/// LEGAL on the global (all-<T>) and scoped relational resources. A key in
/// `allowed_extra` is accepted and IGNORED (never applied as a filter); it is
/// no longer a 400 but still does not narrow the result.
fn parse_and_validate_with(
    query_str: Option<&str>,
    level: QidoLevel,
    allowed_extra: &[&str],
) -> Result<QidoQuery, QidoError> {
    let query = match query_str {
        Some(raw_str) => parse_qido_query(raw_str)?,
        None => QidoQuery::default(),
    };
    query.validate()?;
    let foreign = foreign_keys(&query, level);
    // With an empty `allowed_extra` this reduces to the strict policy: every
    // foreign key is blocked and the result is a 400.
    let blocked: Vec<&str> = foreign
        .iter()
        .copied()
        .filter(|n| !allowed_extra.contains(n))
        .collect();
    if !blocked.is_empty() {
        return Err(QidoError {
            status: STATUS_BAD_REQUEST,
            message: format!(
                "unsupported search keys for {}: {}",
                level.name(),
                blocked.join(", ")
            ),
        });
    }
    Ok(query)
}

/// Parse and validate a query for `level` with the strict foreign-key policy
/// (no cross-level keys are allowed). Most resources use this.
fn parse_and_validate(query_str: Option<&str>, level: QidoLevel) -> Result<QidoQuery, QidoError> {
    parse_and_validate_with(query_str, level, &[])
}

/// Which QIDO level a route serves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QidoLevel {
    Study,
    Series,
    Instance,
}

impl QidoLevel {
    fn name(self) -> &'static str {
        match self {
            QidoLevel::Study => "studies",
            QidoLevel::Series => "series",
            QidoLevel::Instance => "instances",
        }
    }
}

/// Legal keys of the global All-Series resource.
///
/// The four Patient IE keys are LEGAL on every global (all-Study /
/// all-Series / all-Instances) relational resource per DICOM 10.6.1.2.1 even
/// though they live at the Patient/Study level; on this route they are
/// accepted-and-ignored (the search uses only the route's own level keys).
///
/// `StudyInstanceUID` (0020,000D) is different: PS3.18 2026c Table 10.6.1-2
/// makes it a REQUIRED matching key for Series Search, so it is not foreign on
/// `/series` and not accept-and-ignore either - the route resolves it to a
/// single study manifest and applies it as a real filter (see
/// `qido_series_global`).
const PATIENT_IE_KEYS_PLUS_STUDY_UID: [&str; 5] = [
    "PatientName",
    "PatientID",
    "PatientBirthDate",
    "PatientSex",
    "StudyInstanceUID",
];

/// Series IE search keys that the instance-level routes actually APPLY.
///
/// PS3.18 10.6.1.2.1 marks the whole Series IE legal on the Study's-Instances
/// resource, but legality is not the same as being honoured: the former
/// accepted-and-ignored list answered a `Modality=CT` instance query with every
/// modality in the study, which is a wider answer than the ask.
///
/// `Modality` is a real filter in `btree_study_instances`; the rest below are
/// refused with 400 rather than silently dropped (see
/// [`SERIES_IE_KEYS_REFUSED`]).
/// Series IE search keys, applied by the instance-level routes.
///
/// PS3.18 10.6.1.2.1 marks the whole Series IE legal on the Study's-Instances
/// resource, and `btree_study_instances` APPLIES all of them through the same
/// `SeriesFilter` the series routes use. The former list was
/// accepted-and-ignored, which answered a filtered instance query with the
/// study's every instance - a wider answer than the ask.
const SERIES_IE_KEYS: [&str; 15] = [
    "SeriesInstanceUID",
    "SeriesNumber",
    "SeriesDate",
    "SeriesTime",
    "SeriesDescription",
    "Manufacturer",
    "InstitutionalDepartmentName",
    "OperatorsName",
    "ProtocolName",
    "StationName",
    "BodyPartExamined",
    "PerformedProcedureStepStartDate",
    "PerformedProcedureStepStartTime",
    "RequestAttributesSequence",
    "Modality",
];

/// Study IE + Series IE search keys. Both groups are Legal on the global
/// All-Instances resource and both are APPLIED there: the Study IE keys narrow
/// the manifest scan and the Series IE keys narrow the series the instance
/// walk descends into.
const ALL_STUDY_SERIES_KEYS: [&str; 32] = [
    "PatientName",
    "PatientID",
    "StudyDate",
    "AccessionNumber",
    "StudyDescription",
    "PatientBirthDate",
    "PatientSex",
    "ReferringPhysicianName",
    "InstitutionName",
    "RequestingPhysician",
    "RequestedProcedureDescription",
    "PerformingPhysicianName",
    "NameOfPhysiciansReadingStudy",
    "StudyInstanceUID",
    "StudyTime",
    "StudyID",
    "ModalitiesInStudy",
    "SeriesInstanceUID",
    "SeriesNumber",
    "SeriesDate",
    "SeriesTime",
    "SeriesDescription",
    "Manufacturer",
    "InstitutionalDepartmentName",
    "OperatorsName",
    "ProtocolName",
    "StationName",
    "BodyPartExamined",
    "PerformedProcedureStepStartDate",
    "PerformedProcedureStepStartTime",
    "RequestAttributesSequence",
    "Modality",
];

/// The parsed search keys that do not belong to `level`, as keyword names.
///
/// `Modality` (0008,0060) is a Series IE key. It is accepted at the study
/// level (matches the study's first-series modality - a documented deviation)
/// and at the series level, and on the RELATIONAL instance resources
/// (`/studies/{study}/instances`, `/instances`) it is accepted-and-FILTERED
/// (the series-level modality narrows the returned instances). It is rejected
/// only on the hierarchical `/studies/{study}/series/{series}/instances`,
/// where Series IE keys are not legal. The instance-level keys are
/// `SOPInstanceUID`, `SOPClassUID` and `InstanceNumber` (Table 10.6.1-5).
fn foreign_keys(q: &QidoQuery, level: QidoLevel) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::new();
    let study_fields: [(&str, bool); 17] = [
        ("PatientName", q.PatientName.is_some()),
        ("PatientID", q.PatientID.is_some()),
        ("StudyDate", q.StudyDate.is_some()),
        ("AccessionNumber", q.AccessionNumber.is_some()),
        ("StudyDescription", q.StudyDescription.is_some()),
        ("PatientBirthDate", q.PatientBirthDate.is_some()),
        ("PatientSex", q.PatientSex.is_some()),
        ("ReferringPhysicianName", q.ReferringPhysicianName.is_some()),
        ("InstitutionName", q.InstitutionName.is_some()),
        ("RequestingPhysician", q.RequestingPhysician.is_some()),
        (
            "RequestedProcedureDescription",
            q.RequestedProcedureDescription.is_some(),
        ),
        (
            "PerformingPhysicianName",
            q.PerformingPhysicianName.is_some(),
        ),
        (
            "NameOfPhysiciansReadingStudy",
            q.NameOfPhysiciansReadingStudy.is_some(),
        ),
        ("StudyInstanceUID", q.StudyInstanceUID.is_some()),
        ("StudyTime", q.StudyTime.is_some()),
        ("StudyID", q.StudyID.is_some()),
        ("ModalitiesInStudy", q.ModalitiesInStudy.is_some()),
    ];
    let series_fields: [(&str, bool); 14] = [
        ("SeriesInstanceUID", q.SeriesInstanceUID.is_some()),
        ("SeriesNumber", q.SeriesNumber.is_some()),
        ("SeriesDate", q.SeriesDate.is_some()),
        ("SeriesTime", q.SeriesTime.is_some()),
        ("SeriesDescription", q.SeriesDescription.is_some()),
        ("Manufacturer", q.Manufacturer.is_some()),
        (
            "InstitutionalDepartmentName",
            q.InstitutionalDepartmentName.is_some(),
        ),
        ("OperatorsName", q.OperatorsName.is_some()),
        ("ProtocolName", q.ProtocolName.is_some()),
        ("StationName", q.StationName.is_some()),
        ("BodyPartExamined", q.BodyPartExamined.is_some()),
        // PS3.18 Table 10.6.1-5 (2026c): PerformedProcedureStepStartDate/Time are
        // mandatory SERIES matching keys; RequestAttributesSequence (SQ) is a
        // series query key accepted without scalar matching.
        (
            "PerformedProcedureStepStartDate",
            q.PerformedProcedureStepStartDate.is_some(),
        ),
        (
            "PerformedProcedureStepStartTime",
            q.PerformedProcedureStepStartTime.is_some(),
        ),
        (
            "RequestAttributesSequence",
            q.RequestAttributesSequence.is_some(),
        ),
    ];
    match level {
        QidoLevel::Study => {
            v.extend(
                series_fields
                    .iter()
                    .filter(|(_, set)| *set)
                    .map(|(n, _)| *n),
            );
            if q.SOPInstanceUID.is_some() {
                v.push("SOPInstanceUID");
            }
            if q.SOPClassUID.is_some() {
                v.push("SOPClassUID");
            }
            if q.InstanceNumber.is_some() {
                v.push("InstanceNumber");
            }
        }
        QidoLevel::Series => {
            v.extend(study_fields.iter().filter(|(_, set)| *set).map(|(n, _)| *n));
            if q.SOPInstanceUID.is_some() {
                v.push("SOPInstanceUID");
            }
            if q.SOPClassUID.is_some() {
                v.push("SOPClassUID");
            }
            if q.InstanceNumber.is_some() {
                v.push("InstanceNumber");
            }
        }
        QidoLevel::Instance => {
            v.extend(study_fields.iter().filter(|(_, set)| *set).map(|(n, _)| *n));
            v.extend(
                series_fields
                    .iter()
                    .filter(|(_, set)| *set)
                    .map(|(n, _)| *n),
            );
            if q.Modality.is_some() {
                v.push("Modality");
            }
        }
    }
    v.sort_unstable();
    v
}

/// `GET /dicomweb/studies` - study-level QIDO search.
///
/// the B-tree tier is the unconditional study oracle; the
/// SQLite shadow tier is excised. Validation errors -> `QidoError`.
pub fn qido_studies(
    state: &AppState,
    query_str: Option<&str>,
    accept: Option<&str>,
    retrieve_base: Option<&str>,
) -> Result<QidoResp, QidoError> {
    let content_type = negotiate(accept);
    let query = parse_and_validate(query_str, QidoLevel::Study)?;

    Ok(search_studies_btree(
        state,
        &query,
        content_type,
        retrieve_base,
    ))
}

/// A 500 QIDO response for a surfaced B-tree error (mirrors the study path).
fn btree_internal_error() -> QidoResp {
    QidoResp {
        status: STATUS_INTERNAL_SERVER_ERROR,
        content_type: APPLICATION_JSON,
        truncated: false,
        remaining: None,
        body: br#"{"error":"Internal server error"}"#.to_vec(),
    }
}

/// Assemble a QIDO XML response from a per-item typed attribute writer.
///
/// addendum: the B-tree tiers MUST serve XML themselves, or the
/// always-on XML producer dies with the SQLite fallback in. When the
/// negotiated content type is XML, all items are streamed into a single
/// `<NativeDicomModel>` root via `clarus_core::query::xml`; otherwise `None`
/// is returned so the caller keeps the JSON path.
fn btree_xml<T, F>(
    items: &[T],
    mut write_attrs: F,
    content_type: &'static str,
    truncated: bool,
    remaining: Option<u64>,
) -> Option<QidoResp>
where
    F: FnMut(&mut [u8; 4096], &T) -> Result<usize, clarus_core::query::xml::StreamOverflow>,
{
    if content_type != DICOM_XML_CONTENT_TYPE {
        return None;
    }
    let mut body = Vec::with_capacity(items.len() * 128 + 64);
    clarus_core::query::xml::write_xml_doc_start(&mut body);
    let mut scratch = [0u8; 4096];
    for item in items {
        match write_attrs(&mut scratch, item) {
            Ok(n) => body.extend_from_slice(&scratch[..n]),
            Err(_) => return Some(btree_internal_error()),
        }
    }
    clarus_core::query::xml::write_xml_doc_end(&mut body);
    Some(QidoResp {
        status: STATUS_OK,
        content_type: DICOM_XML_CONTENT_TYPE,
        truncated,
        remaining,
        body,
    })
}

/// PS3.18 8.3.4.4.1: `remaining = matches - offset - returned`, saturating.
/// `None` when nothing is left, which is exactly when no Warning is emitted.
fn qido_remaining(total_matches: usize, offset: Option<u64>, returned: u64) -> Option<u64> {
    let left = (total_matches as u64)
        .saturating_sub(offset.unwrap_or(0))
        .saturating_sub(returned);
    (left > 0).then_some(left)
}

/// Build the `SeriesFilter` shared by the scoped and the global series search.
///
/// The two routes differ only in study scope and result limit; the filter
/// itself is identical, so it is built once here (borrowed from the query).
fn series_filter_from(query: &QidoQuery, state: &AppState) -> clarus_core::query::SeriesFilter {
    clarus_core::query::SeriesFilter {
        series_instance_uid: query.SeriesInstanceUID.clone(),
        modality: query.Modality.clone(),
        series_date: query.SeriesDate.clone(),
        series_time: query.SeriesTime.clone(),
        series_description: query.SeriesDescription.clone(),
        manufacturer: query.Manufacturer.clone(),
        institutional_department_name: query.InstitutionalDepartmentName.clone(),
        operators_name: query.OperatorsName.clone(),
        protocol_name: query.ProtocolName.clone(),
        station_name: query.StationName.clone(),
        body_part_examined: query.BodyPartExamined.clone(),
        pps_start_date: query.PerformedProcedureStepStartDate.clone(),
        pps_start_time: query.PerformedProcedureStepStartTime.clone(),
        scheduled_procedure_step_id: query.ScheduledProcedureStepID.clone(),
        requested_procedure_id: query.RequestedProcedureIDInSequence.clone(),
        pn_case_insensitive: !state.config.query.case_sensitive_pn,
    }
}

/// Stream a paginated series page (shared by the scoped and global series
/// routes). `study_uid` is `Some` for the scoped route (the UID is injected
/// into each row) and `None` for the global route. `remaining` is the PS3.18
/// 8.3.4.4.1 count computed by the caller from the route's own limit contract.
fn render_series_page(
    page: &[&clarus_core::manifest::SeriesRecord],
    query: &QidoQuery,
    study_uid: Option<&str>,
    content_type: &'static str,
    retrieve_base: Option<&str>,
    remaining: Option<u64>,
) -> QidoResp {
    if let Some(xml) = btree_xml(
        page,
        |scratch, s| {
            clarus_core::query::xml::write_series_attrs(
                scratch,
                s,
                query.includefield.as_deref(),
                retrieve_base,
                study_uid,
            )
        },
        content_type,
        remaining.is_some(),
        remaining,
    ) {
        return xml;
    }

    let mut body = Vec::with_capacity(page.len() * 256 + 2);
    body.push(b'[');
    let mut scratch = [0u8; 4096];
    for (i, series) in page.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        match clarus_core::query::json::write_series_json(
            &mut scratch,
            series,
            query.includefield.as_deref(),
            retrieve_base,
            study_uid,
        ) {
            Ok(n) => body.extend_from_slice(&scratch[..n]),
            Err(_) => return btree_internal_error(),
        }
    }
    body.push(b']');

    QidoResp {
        status: STATUS_OK,
        content_type: DICOM_JSON_CONTENT_TYPE,
        truncated: remaining.is_some(),
        remaining,
        body,
    }
}

/// Stream a paginated instance page (shared by the scoped and global instance
/// routes). Each entry pairs the matched instance with its owning Study and
/// Series UIDs, so the renderer injects them without cloning. `remaining` is
/// computed by the caller from the route's own limit contract.
fn render_instance_page(
    page: &[(&clarus_core::manifest::InstanceRecord, String, String)],
    query: &QidoQuery,
    content_type: &'static str,
    retrieve_base: Option<&str>,
    remaining: Option<u64>,
) -> QidoResp {
    if let Some(xml) = btree_xml(
        page,
        |scratch, (inst, study_uid, series_uid)| {
            clarus_core::query::xml::write_instance_attrs(
                scratch,
                inst,
                study_uid,
                series_uid,
                query.includefield.as_deref(),
                retrieve_base,
            )
        },
        content_type,
        remaining.is_some(),
        remaining,
    ) {
        return xml;
    }

    let mut body = Vec::with_capacity(page.len() * 256 + 2);
    body.push(b'[');
    let mut scratch = [0u8; 4096];
    for (i, (inst, study_uid, series_uid)) in page.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        match clarus_core::query::json::write_instance_json(
            &mut scratch,
            inst,
            study_uid,
            series_uid,
            query.includefield.as_deref(),
            retrieve_base,
        ) {
            Ok(n) => body.extend_from_slice(&scratch[..n]),
            Err(_) => return btree_internal_error(),
        }
    }
    body.push(b']');

    QidoResp {
        status: STATUS_OK,
        content_type: DICOM_JSON_CONTENT_TYPE,
        truncated: remaining.is_some(),
        remaining,
        body,
    }
}

/// The B-tree series search, streaming `write_series_json`.
///
/// Opens `manifests/{study_uid}.manifest`, runs [`search_series`] with the
/// query's series filters, paginates, and streams the series JSON array into a
/// caller buffer.: an absent manifest is an unknown/empty study
/// -> empty array (the SQLite shadow tier is excised).
fn btree_series_search(
    state: &AppState,
    query: &QidoQuery,
    study_uid: &str,
    content_type: &'static str,
    retrieve_base: Option<&str>,
) -> QidoResp {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let path = manifest_dir.join(format!("{study_uid}.manifest"));
    if !path.is_file() {
        return empty_json();
    }
    let manifest = match clarus_core::manifest::read_manifest(&path) {
        Ok(m) => m,
        Err(e) => {
            crate::clog_error!(error = %e, study_uid = %study_uid, "B-tree QIDO series: undecodable manifest");
            return btree_internal_error();
        }
    };
    // No heal poke from the series path: the study was already found via the
    // study-search point (which does the provenance/absent check); series-level
    // reads carry no modality-idx provenance, so poking here would double- or
    // false-trigger detection.

    let filter = series_filter_from(query, state);

    let mut out: Vec<&clarus_core::manifest::SeriesRecord> = Vec::new();
    clarus_core::query::search_series(&manifest, &filter, &mut out);

    // Pagination then streaming (deterministic sort from search_series).
    let offset = query.offset.unwrap_or(0) as usize;
    let limit = resolve_enumeration_limit(query);
    let take_n = limit.map(|l| l as usize).unwrap_or(usize::MAX);
    let total = out.len();
    let page: Vec<_> = out.into_iter().skip(offset).take(take_n).collect();
    let remaining = qido_remaining(total, query.offset, page.len() as u64);

    // addendum: series XML carries the captured series attributes plus
    // NumberOfSeriesRelatedInstances (0020,1209) from instances.len(). It
    // always-emits SpecificCharacterSet (0008,0005) (owner 07.09, reverses
    // then the series block. Byte-exact with series.xml.
    render_series_page(
        &page,
        query,
        Some(study_uid),
        content_type,
        retrieve_base,
        remaining,
    )
}

/// The B-tree instance search, streaming `write_instance_json`.
///
/// Opens `manifests/{study_uid}.manifest`, runs [`search_instances`] for the
/// given series, paginates, and streams instance JSON with the Study/Series
/// UIDs injected from the request path.: an absent manifest is
/// an unknown study -> empty array.
fn btree_instances_search(
    state: &AppState,
    query: &QidoQuery,
    study_uid: &str,
    series_uid: &str,
    content_type: &'static str,
    retrieve_base: Option<&str>,
    expose: bool,
) -> QidoResp {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let path = manifest_dir.join(format!("{study_uid}.manifest"));
    if !path.is_file() {
        return empty_json();
    }
    let manifest = match clarus_core::manifest::read_manifest(&path) {
        Ok(m) => m,
        Err(e) => {
            crate::clog_error!(error = %e, study_uid = %study_uid, "B-tree QIDO instances: undecodable manifest");
            return btree_internal_error();
        }
    };
    let filter = clarus_core::query::InstanceFilter {
        sop_instance_uid: query.SOPInstanceUID.clone(),
        sop_class_uid: query.SOPClassUID.clone(),
        instance_number: query.InstanceNumber.clone(),
    };
    let mut out: Vec<&clarus_core::manifest::InstanceRecord> = Vec::new();
    // A series not in this manifest -> no instances (empty, not an error).
    // Sequestered instances are hidden unless the caller is on the QA surface,
    // and that surface lifts only Quality rejections.
    let _present =
        clarus_core::query::search_instances(&manifest, series_uid, &filter, &mut out, expose);
    let found = out;

    let offset = query.offset.unwrap_or(0) as usize;
    let limit = resolve_enumeration_limit(query);
    let take_n = limit.map(|l| l as usize).unwrap_or(usize::MAX);
    let total = found.len();
    let page: Vec<_> = found.into_iter().skip(offset).take(take_n).collect();
    let remaining = qido_remaining(total, query.offset, page.len() as u64);

    // addendum: instances XML with the Study/Series UIDs injected from
    // context, byte-exact with instance.xml.
    if let Some(xml) = btree_xml(
        &page,
        |scratch, inst| {
            clarus_core::query::xml::write_instance_attrs(
                scratch,
                inst,
                study_uid,
                series_uid,
                query.includefield.as_deref(),
                retrieve_base,
            )
        },
        content_type,
        remaining.is_some(),
        remaining,
    ) {
        return xml;
    }

    let mut body = Vec::with_capacity(page.len() * 256 + 2);
    body.push(b'[');
    let mut scratch = [0u8; 4096];
    for (i, inst) in page.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        match clarus_core::query::json::write_instance_json(
            &mut scratch,
            inst,
            study_uid,
            series_uid,
            query.includefield.as_deref(),
            retrieve_base,
        ) {
            Ok(n) => body.extend_from_slice(&scratch[..n]),
            Err(_) => return btree_internal_error(),
        }
    }
    body.push(b']');

    QidoResp {
        status: STATUS_OK,
        content_type: DICOM_JSON_CONTENT_TYPE,
        truncated: remaining.is_some(),
        remaining,
        body,
    }
}

/// The B-tree path for `qido_study_instances`: all instances across a study's
/// series, streaming `write_instance_json` with the study/series UIDs injected.
///
/// `limit` is supplied by the caller because the route's limit contract
/// differs: the scoped route uses `resolve_enumeration_limit` (no cap), the
/// global StudyInstanceUID short-circuit uses the GLOBAL `resolve_limit`.
fn btree_study_instances(
    state: &AppState,
    query: &QidoQuery,
    study_uid: &str,
    limit: Option<u64>,
    content_type: &'static str,
    retrieve_base: Option<&str>,
    expose: bool,
) -> QidoResp {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let path = manifest_dir.join(format!("{study_uid}.manifest"));
    if !path.is_file() {
        return empty_json();
    }
    let manifest = match clarus_core::manifest::read_manifest(&path) {
        Ok(m) => m,
        Err(e) => {
            crate::clog_error!(error = %e, study_uid = %study_uid, "B-tree QIDO study instances: undecodable manifest");
            return btree_internal_error();
        }
    };
    let filter = clarus_core::query::InstanceFilter {
        sop_instance_uid: query.SOPInstanceUID.clone(),
        sop_class_uid: query.SOPClassUID.clone(),
        instance_number: query.InstanceNumber.clone(),
    };
    let offset = query.offset.unwrap_or(0) as usize;
    let take_n = limit.map(|l| l as usize).unwrap_or(usize::MAX);
    // Every Series IE key PS3.18 10.6.1.2.1 makes LEGAL here is APPLIED, not
    // accepted-and-ignored: a series whose attributes do not match contributes
    // no instances. Ignoring them answered a filtered instance query with the
    // study's every instance, which the client cannot distinguish from "no
    // matches". The same `SeriesFilter` the series routes use is built here, so
    // the semantics of one key cannot differ between the two surfaces.
    let series_filter = series_filter_from(query, state);
    // Collect (instance, study_uid, series_uid) triples across all series, in
    // the flattened series-then-instance order. `total` counts every match
    // before pagination, so the Warning can state what is left; `seen` counts
    // how many have been walked, which is what `offset` skips.
    let mut matched: Vec<(&clarus_core::manifest::InstanceRecord, String, String)> = Vec::new();
    let mut total = 0usize;
    let mut seen = 0usize;
    for series in &manifest.series {
        if !series_filter.matches(&series.attrs) {
            continue;
        }
        let mut out: Vec<&clarus_core::manifest::InstanceRecord> = Vec::new();
        let series_uid = series
            .attrs
            .iter()
            .find(|(t, _)| *t == clarus_core::query::tag::SERIES_INSTANCE_UID)
            .and_then(|(_, v)| match v {
                clarus_core::manifest::AttrValue::Text(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        clarus_core::query::search_instances(&manifest, &series_uid, &filter, &mut out, expose);
        total += out.len();
        for inst in out {
            let position = seen;
            seen += 1;
            // PS3.18 8.3.4.4: `offset` is the number of leading matches to
            // skip. Without it a study with more instances than `limit`
            // returned the same first page forever.
            if position < offset {
                continue;
            }
            if matched.len() >= take_n {
                continue;
            }
            matched.push((inst, study_uid.to_string(), series_uid.clone()));
        }
    }
    // The remaining count is the matches past this page, so the Warning can
    // carry it (PS3.18 8.3.4.4.1). Computed from the pre-pagination total.
    let remaining = qido_remaining(total, Some(offset as u64), matched.len() as u64);

    render_instance_page(&matched, query, content_type, retrieve_base, remaining)
}

/// `GET /dicomweb/studies/{StudyInstanceUID}/series` - series search.
///
/// The B-tree path opens the study manifest, filters + sorts the series per
/// the 12.08 contract, and streams `write_series_json` (byte-exact with the
/// frozen SQLite-era renderer).: B-tree is unconditional; an
/// unknown study yields an empty array.
pub fn qido_series(
    state: &AppState,
    study_uid: &str,
    query_str: Option<&str>,
    accept: Option<&str>,
    retrieve_base: Option<&str>,
) -> Result<QidoResp, QidoError> {
    let content_type = negotiate(accept);
    let query = parse_and_validate(query_str, QidoLevel::Series)?;

    // Validate study UID (reject path traversal).
    validate_uid(study_uid)?;

    Ok(btree_series_search(
        state,
        &query,
        study_uid,
        content_type,
        retrieve_base,
    ))
}

/// `GET /dicomweb/studies/{StudyInstanceUID}/series/{SeriesInstanceUID}/instances`.
pub fn qido_instances(
    state: &AppState,
    study_uid: &str,
    series_uid: &str,
    query_str: Option<&str>,
    accept: Option<&str>,
    retrieve_base: Option<&str>,
    expose: bool,
) -> Result<QidoResp, QidoError> {
    let content_type = negotiate(accept);
    let query = parse_and_validate(query_str, QidoLevel::Instance)?;

    // Validate UIDs (reject path traversal).
    validate_uid(study_uid)?;
    validate_uid(series_uid)?;

    Ok(btree_instances_search(
        state,
        &query,
        study_uid,
        series_uid,
        content_type,
        retrieve_base,
        expose,
    ))
}

/// `GET /dicomweb/series/{SeriesInstanceUID}/instances` - Weasis-compat
/// series-root instances QIDO (06.09 field incident).
///
/// Weasis appends "/instances" to the QIDO series RetrieveURL (0008,1190)
/// and queries the result verbatim. The global series search emits the
/// series-root form (`{base}/dicomweb/series/{ser}`), so this route must
/// resolve it: scan the manifests for the owning study, then render the
/// instances exactly like the study-scoped path.
pub fn qido_series_root_instances(
    state: &AppState,
    series_uid: &str,
    query_str: Option<&str>,
    accept: Option<&str>,
    retrieve_base: Option<&str>,
    expose: bool,
) -> Result<QidoResp, QidoError> {
    let content_type = negotiate(accept);
    let query = parse_and_validate(query_str, QidoLevel::Instance)?;
    validate_uid(series_uid)?;

    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let entries = match std::fs::read_dir(manifest_dir) {
        Ok(it) => it,
        Err(_) => return Ok(empty_json()),
    };
    let mut owner: Option<String> = None;
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = fname.strip_suffix(".manifest") else {
            continue;
        };
        let manifest = match clarus_core::manifest::read_manifest(&entry.path()) {
            Ok(m) => m,
            Err(_) => continue, // corrupt manifest: skip, keep scanning
        };
        let filter = clarus_core::query::SeriesFilter {
            series_instance_uid: Some(series_uid.to_string()),
            modality: None,
            series_date: None,
            series_time: None,
            series_description: None,
            manufacturer: None,
            institutional_department_name: None,
            operators_name: None,
            protocol_name: None,
            station_name: None,
            body_part_examined: None,
            pps_start_date: None,
            pps_start_time: None,
            scheduled_procedure_step_id: None,
            requested_procedure_id: None,
            pn_case_insensitive: false,
        };
        let mut hits: Vec<&clarus_core::manifest::SeriesRecord> = Vec::new();
        clarus_core::query::search_series(&manifest, &filter, &mut hits);
        if !hits.is_empty() {
            owner = Some(stem.to_string());
            break;
        }
    }
    match owner {
        Some(study_uid) => Ok(btree_instances_search(
            state,
            &query,
            &study_uid,
            series_uid,
            content_type,
            retrieve_base,
            expose,
        )),
        None => Ok(empty_json()),
    }
}

/// `GET /dicomweb/studies/{StudyInstanceUID}/instances` - all instances of a
/// study across all its series (OHIF loads a study in one request).
pub fn qido_study_instances(
    state: &AppState,
    study_uid: &str,
    query_str: Option<&str>,
    accept: Option<&str>,
    retrieve_base: Option<&str>,
    expose: bool,
) -> Result<QidoResp, QidoError> {
    let content_type = negotiate(accept);

    // This scoped resource is the study's instances - PS3.18 10.6.1.2.1 marks
    // the whole Series IE LEGAL here, and every key in `SERIES_IE_KEYS` is
    // APPLIED: `btree_study_instances` runs them through the same
    // `SeriesFilter` the series routes use, so a non-matching series
    // contributes no instances. A key that is legal but silently dropped would
    // answer `SeriesDescription=X` with every series - wider than the ask, and
    // invisible to the client.
    // Study/Patient IE keys stay foreign here (they would re-scope a study the
    // path already fixed).
    let query = parse_and_validate_with(query_str, QidoLevel::Instance, &SERIES_IE_KEYS)?;

    validate_uid(study_uid)?;

    Ok(btree_study_instances(
        state,
        &query,
        study_uid,
        // Scoped enumeration: no `[query] default_limit` cap (only an explicit
        // client `limit`), so the path scope, not the search dial, bounds it.
        resolve_enumeration_limit(&query),
        content_type,
        retrieve_base,
        expose,
    ))
}

/// `GET /dicomweb/patients?limit=250` - lightweight patient summaries.
///
/// E-014: the B-tree tier is the unconditional patient oracle (the SQLite
/// shadow tier is excised in). 12.09: the limit follows `[query]
/// default_limit` - the mini-viewer's patient list is the same open-ended
/// search as the rest, one knob governs all (no separate patients dial).
///
/// 08.09 extension: `PatientBirthDate` (0010,0030) is accepted as an
/// inclusive range `YYYYMMDD[-YYYYMMDD]` (or `*`), validated by the same
/// StudyDate machinery and applied before aggregation - a patient matches
/// when its birth date (first non-empty among its studies) is in range.
pub fn qido_patients(
    state: &AppState,
    query_str: Option<&str>,
    accept: Option<&str>,
) -> Result<QidoResp, QidoError> {
    let content_type = negotiate(accept);

    // Parse the WHOLE query string with the shared case-insensitive parser:
    // PatientName / PatientID (previously ignored here), fuzzymatching, limit
    // and PatientBirthDate all arrive url-decoded on the same path as the
    // study level. Unknown keys are ignored with one sorted WARN (same policy).
    let query = match query_str {
        Some(raw) => parse_qido_query(raw)?,
        None => QidoQuery::default(),
    };
    // INV-003 PatientName length + PatientBirthDate form -> 400 on violation.
    query.validate()?;

    // The mini-viewer's patient list follows the SAME limit knob as all
    // open-ended searches (12.09); explicit limit honored, 0 = unlimited.
    let limit: Option<u64> = resolve_limit(&query, &state.config.query);
    let take_n: usize = limit.map(|l| l as usize).unwrap_or(usize::MAX);

    // PatientBirthDate range (08.09): `query.validate()` already rejected a
    // malformed value; `patient_birth_date_range` splits `YYYYMMDD[-YYYYMMDD]`
    // (`*` -> no bound). Converted to the u64 form the aggregation filters on.
    let (birth_from, birth_to) = query.patient_birth_date_range();
    let birth_range: Option<(Option<u64>, Option<u64>)> = match (birth_from, birth_to) {
        (None, None) => None,
        (f, t) => Some((
            f.and_then(|s| s.parse::<u64>().ok()),
            t.and_then(|s| s.parse::<u64>().ok()),
        )),
    };

    // PatientName/PatientID matching REUSES the study-level `QueryFilter`
    // semantics verbatim: PatientName = fuzzymatch (client flag or the
    // `fuzzymatch_override` knob) / case-insensitive glob (`case_sensitive_pn`)
    // / case-sensitive glob; PatientID = case-sensitive glob. No ad-hoc
    // matching code of its own.
    let filter = clarus_core::query::QueryFilter {
        patient_name: query.PatientName.clone(),
        patient_id: query.PatientID.clone(),
        fuzzymatch: query.fuzzymatching.unwrap_or(false),
        fuzzymatch_override: state.config.query.fuzzymatch_override,
        pn_case_insensitive: !state.config.query.case_sensitive_pn,
        ..Default::default()
    };

    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    // Fetch the WHOLE aggregation (no limit) and apply the name/ID filter
    // BEFORE truncation - a limited scan would silently drop a matching
    // patient sitting outside the recent-N window (the reported bug).
    // `build_expected` is hoisted OUT of the row loop (it would otherwise
    // allocate one HashMap per patient); the per-row predicate reuse is the
    // same `matches_prebuilt` contract the study scanner uses.
    let expected = filter.build_expected();
    let mut patients: Vec<clarus_ports::models::PatientSummary> =
        match clarus_core::query::find_recent_patients(manifest_dir, usize::MAX, birth_range) {
            Ok(rows) => rows
                .into_iter()
                .filter(|p| {
                    let attrs = [
                        (
                            clarus_core::query::tag::PATIENT_NAME,
                            clarus_core::manifest::AttrValue::Text(p.patient_name.clone()),
                        ),
                        (
                            clarus_core::query::tag::PATIENT_ID,
                            clarus_core::manifest::AttrValue::Text(p.patient_uid.clone()),
                        ),
                    ];
                    filter.matches_prebuilt(&attrs, &expected)
                })
                .map(|p| clarus_ports::models::PatientSummary {
                    patient_id: p.patient_id,
                    patient_uid: p.patient_uid,
                    patient_name: p.patient_name,
                    last_arrival: Some(p.last_arrival),
                    study_count: p.study_count,
                })
                .collect(),
            Err(e) => {
                crate::clog_error!(error = %e, "B-tree patients search failed");
                return Err(QidoError {
                    status: STATUS_INTERNAL_SERVER_ERROR,
                    message: "Internal server error".to_string(),
                });
            }
        };

    // The number of matches BEFORE the limit is applied. PS3.18 8.3.4.4.1
    // defines `remaining = matches - offset - returned`, where `matches` is
    // what the query matched, not what survived truncation. Reading the count
    // after `truncate` made `matches == returned` always, so `remaining` was
    // always 0 and the Warning header that tells a client to page again was
    // never emitted - a silently short answer.
    let total_matches = patients.len();

    // Apply the resolved limit to the FILTERED set, then re-rank the
    // deterministic identity 0..N (it is a rank, not a stored id).
    // PS3.18 8.3.4.4.1: offset = number of matches to skip; applying the
    // limit without it answered every page with the first `limit` patients.
    let offset = query.offset.unwrap_or(0) as usize;
    patients.drain(..offset.min(patients.len()));
    patients.truncate(take_n);
    for (i, p) in patients.iter_mut().enumerate() {
        p.patient_id = i as i64;
    }

    let result_count = patients.len() as u64;
    // the patients path renders with the native writers - no
    // serde_json::Value here.
    let patients: Vec<clarus_core::query::json::PatientQido> = patients
        .into_iter()
        .map(|p| clarus_core::query::json::PatientQido {
            patient_id: p.patient_id,
            patient_uid: p.patient_uid,
            patient_name: p.patient_name,
            last_arrival: p.last_arrival,
            study_count: p.study_count,
        })
        .collect();

    // Truncation follows the resolved limit (12.09: the [query] knob, not a
    // hard cap). PS3.18 8.3.4.4.1: `remaining = matches - offset - returned`.
    let truncated = result_count as usize >= take_n;
    let patients_remaining = qido_remaining(total_matches, query.offset, result_count);

    // -- XML branch (PS3.19, always-on since) -------------------------
    if content_type == DICOM_XML_CONTENT_TYPE {
        let mut body = Vec::new();
        clarus_core::query::xml::write_xml_doc_start(&mut body);
        for p in &patients {
            clarus_core::query::xml::write_patient_xml_into(&mut body, p);
        }
        // The legacy dicomweb_json_to_xml closes the root with no leading
        // newline after the last attribute.
        body.extend_from_slice(b"</NativeDicomModel>\n");
        return Ok(QidoResp {
            status: STATUS_OK,
            content_type: DICOM_XML_CONTENT_TYPE,
            truncated,
            remaining: patients_remaining,
            body,
        });
    }

    // -- JSON path --------------------------------------------------------
    let mut body = Vec::new();
    body.push(b'[');
    let mut buf = [0u8; 1024];
    for (i, p) in patients.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        let n =
            clarus_core::query::json::write_patient_json(&mut buf, p).map_err(|_| QidoError {
                status: STATUS_INTERNAL_SERVER_ERROR,
                message: "Internal server error".to_string(),
            })?;
        body.extend_from_slice(&buf[..n]);
    }
    body.push(b']');
    Ok(QidoResp {
        status: STATUS_OK,
        content_type: DICOM_JSON_CONTENT_TYPE,
        truncated,
        remaining: patients_remaining,
        body,
    })
}

/// Decode every `*.manifest` in `manifest_dir` into an owned `Vec<Manifest>`.
///
/// This is the caller-owned per-request "archive view" (ap101-approved for gap
/// 2): the top-level series/instance scan must hold **all** decoded manifests
/// in memory so it can borrow `&SeriesRecord`/`&InstanceRecord` references
/// across them and sort globally by UID. This is an extension of the per-study
/// `read_manifest` pattern (the manifest decode path allocates by
/// construction); the render and the borrow-form collect/sort stay 0-alloc.
///
/// A directory that cannot be listed yields `Ok(Vec::new())` (treat as empty
/// archive; a fresh store legitimately has no manifests). A corrupt manifest
/// aborts the whole request - honest surfacing, matching the scanner's
/// lazy-recovery contract.
fn decode_all_manifests(
    manifest_dir: &std::path::Path,
) -> Result<Vec<clarus_core::manifest::Manifest>, ()> {
    let entries = match std::fs::read_dir(manifest_dir) {
        Ok(it) => it,
        Err(_) => return Ok(Vec::new()),
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        let fname = entry.file_name().to_string_lossy().into_owned();
        if let Some(stem) = fname.strip_suffix(".manifest") {
            names.push(stem.to_string());
        }
    }
    names.sort();
    let mut manifests: Vec<clarus_core::manifest::Manifest> = Vec::with_capacity(names.len());
    for stem in names {
        let path = manifest_dir.join(format!("{stem}.manifest"));
        let m = clarus_core::manifest::read_manifest(&path).map_err(|_| ())?;
        manifests.push(m);
    }
    Ok(manifests)
}

/// `GET /dicomweb/series` - top-level (global) series QIDO across the whole
/// archive (gap 2), with no study scope.
///
/// Scans every study manifest, merges all matching series, sorts globally by
/// `SeriesInstanceUID` ASC so `offset`-pagination is deterministic, applies
/// the unified `resolve_limit` Search-Transaction contract (PS3.18 Table
/// 8.3.4-1) with Warning 299, and streams `write_series_json` / as XML
/// `write_series_attrs`. Series filters come from the query;
/// there is no `ModalitiesInStudy`/study key here.
pub fn qido_series_global(
    state: &AppState,
    query_str: Option<&str>,
    accept: Option<&str>,
    retrieve_base: Option<&str>,
) -> Result<QidoResp, QidoError> {
    let content_type = negotiate(accept);

    // Global "All Series" - DICOM 10.6.1.2.1 marks Patient IE
    // search keys LEGAL here (accepted and ignored, no filter, no 400), and
    // PS3.18 Table 10.6.1-2 makes StudyInstanceUID a REQUIRED Series key.
    // StudyInstanceUID is NOT accept-and-ignore: the short-circuit below
    // resolves it to one study manifest and applies it as a real filter.
    // The remaining Study IE keys (StudyDate/StudyID/...) stay foreign -> 400.
    let query = parse_and_validate_with(
        query_str,
        QidoLevel::Series,
        &PATIENT_IE_KEYS_PLUS_STUDY_UID,
    )?;
    let limit = resolve_limit(&query, &state.config.query);
    let filter = series_filter_from(&query, state);
    let offset = query.offset.unwrap_or(0) as usize;
    let take_n = limit.map(|l| l as usize).unwrap_or(usize::MAX);

    // StudyInstanceUID present: resolve ONE manifest by path instead of
    // scanning the whole archive. The manifest FILE is named after the study
    // UID, so this is a direct path, not a `read_dir` + decode of every study
    // (ap101: O(1) file open + one mmap parse vs O(all studies)).
    if let Some(study_uid) = query.StudyInstanceUID.clone() {
        // Path-traversal guard BEFORE any `manifest_dir.join(...)`.
        validate_uid(&study_uid)?;
        let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
        let path = manifest_dir.join(format!("{study_uid}.manifest"));
        if !path.is_file() {
            // Unknown study -> empty array (200), matching the scoped route.
            return Ok(empty_json());
        }
        let manifest = match clarus_core::manifest::read_manifest(&path) {
            Ok(m) => m,
            Err(e) => {
                crate::clog_error!(error = %e, study_uid = %study_uid, "QIDO global series: undecodable manifest");
                return Err(QidoError {
                    status: STATUS_INTERNAL_SERVER_ERROR,
                    message: "Internal server error".to_string(),
                });
            }
        };
        // Same SeriesFilter the full scan uses, so Modality and every other
        // series key still narrow WITHIN the resolved study.
        let mut out: Vec<&clarus_core::manifest::SeriesRecord> = Vec::new();
        clarus_core::query::search_series(&manifest, &filter, &mut out);
        // The global route keeps the GLOBAL pagination contract (offset +
        // `resolve_limit` + Warning 299), not the scoped enumeration limit.
        let total = out.len();
        let start = offset.min(out.len());
        let page = &out[start..];
        let page_len = page.len().min(take_n);
        let page = &page[..page_len];
        let remaining = qido_remaining(total, query.offset, page_len as u64);
        return Ok(render_series_page(
            page,
            &query,
            None,
            content_type,
            retrieve_base,
            remaining,
        ));
    }

    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let manifests = match decode_all_manifests(manifest_dir) {
        Ok(m) => m,
        Err(()) => {
            crate::clog_error!("QIDO global series: undecodable manifest");
            return Err(QidoError {
                status: STATUS_INTERNAL_SERVER_ERROR,
                message: "Internal server error".to_string(),
            });
        }
    };
    if manifests.is_empty() {
        return Ok(empty_json());
    }

    let mut out: Vec<&clarus_core::manifest::SeriesRecord> = Vec::new();
    clarus_core::query::search_all_series(&manifests, &filter, &mut out);

    // Deterministic pagination over the globally-sorted slice (offset then
    // limit) - no re-ordering, no per-page drift between offset pages.
    let total = out.len();
    let start = offset.min(out.len());
    let page = &out[start..];
    let page_len = page.len().min(take_n);
    let page = &page[..page_len];
    let remaining = qido_remaining(total, query.offset, page_len as u64);

    Ok(render_series_page(
        page,
        &query,
        None,
        content_type,
        retrieve_base,
        remaining,
    ))
}

/// `GET /dicomweb/instances` - top-level (global) instance QIDO across the
/// whole archive (gap 2), with no study scope.
///
/// Scans every study manifest's every series, merges all matching instances,
/// sorts globally by `SOPInstanceUID` ASC, applies the unified `resolve_limit`
/// contract with Warning 299, and streams `write_instance_json` / as XML
/// `write_instance_attrs` with the owning Study/Series UIDs
/// borrowed from the manifest (no cloning). The instance filters are
/// `SOPInstanceUID`, `SOPClassUID` and `InstanceNumber` (Table 10.6.1-5,
///.
pub fn qido_instances_global(
    state: &AppState,
    query_str: Option<&str>,
    accept: Option<&str>,
    retrieve_base: Option<&str>,
    expose: bool,
) -> Result<QidoResp, QidoError> {
    let content_type = negotiate(accept);

    // Global "All Instances" - DICOM 10.6.1.2.1 marks Study IE
    // AND Series IE search keys LEGAL here, and BOTH are applied: the Study IE
    // keys gate the manifest scan through the study `QueryFilter` below, and
    // the Series IE keys gate the series the instance walk descends into.
    let query = parse_and_validate_with(query_str, QidoLevel::Instance, &ALL_STUDY_SERIES_KEYS)?;
    let limit = resolve_limit(&query, &state.config.query);

    // StudyInstanceUID present: resolve ONE manifest by path instead of
    // scanning the whole archive (ap101: O(1) open + one mmap parse). It is a
    // real filter here - previously it was accepted-and-ignored, so the route
    // returned every instance in the archive.
    if let Some(study_uid) = query.StudyInstanceUID.clone() {
        // Path-traversal guard BEFORE any `manifest_dir.join(...)`.
        validate_uid(&study_uid)?;
        return Ok(btree_study_instances(
            state,
            &query,
            &study_uid,
            // The GLOBAL route keeps the global limit contract (250 cap +
            // Warning 299), not the scoped enumeration limit.
            limit,
            content_type,
            retrieve_base,
            expose,
        ));
    }

    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let manifests = match decode_all_manifests(manifest_dir) {
        Ok(m) => m,
        Err(()) => {
            crate::clog_error!("QIDO global instances: undecodable manifest");
            return Err(QidoError {
                status: STATUS_INTERNAL_SERVER_ERROR,
                message: "Internal server error".to_string(),
            });
        }
    };
    if manifests.is_empty() {
        return Ok(empty_json());
    }

    let filter = clarus_core::query::InstanceFilter {
        sop_instance_uid: query.SOPInstanceUID.clone(),
        sop_class_uid: query.SOPClassUID.clone(),
        instance_number: query.InstanceNumber.clone(),
    };
    // Modality is a Series IE key made LEGAL on this relational resource by
    // PS3.18 10.6.1.2.1, and applied here as a REAL filter (a non-matching
    // series contributes no instances). Reuses the series-level matcher.
    // The full Series IE filter is applied here too (see the scoped route):
    // PS3.18 10.6.1.2.1 makes those keys legal on this relational resource and
    // a non-matching series contributes no instances.
    let series_filter = series_filter_from(&query, state);
    let mut out: Vec<clarus_core::query::GlobalInstance<'_>> = Vec::new();
    // P1: the Study IE keys this route ACCEPTS are applied here, not dropped.
    // `qido_to_btree_filter` turns them into the same study-level matcher the
    // study route uses, so `?PatientID=P1` narrows the scan instead of
    // returning every patient's instances.
    let study_filter = qido_to_btree_filter(&query, limit, &state.config.query);
    clarus_core::query::search_all_instances(
        &manifests,
        &filter,
        &series_filter,
        Some(&study_filter),
        &mut out,
        expose,
    );

    let offset = query.offset.unwrap_or(0) as usize;
    let take_n = limit.map(|l| l as usize).unwrap_or(usize::MAX);
    let total = out.len();
    let start = offset.min(out.len());
    let page = &out[start..];
    let page_len = page.len().min(take_n);
    let page = &page[..page_len];
    let remaining = qido_remaining(total, query.offset, page_len as u64);

    if let Some(xml) = btree_xml(
        page,
        |scratch, gi| {
            clarus_core::query::xml::write_instance_attrs(
                scratch,
                gi.instance,
                gi.study_uid,
                gi.series_uid,
                query.includefield.as_deref(),
                retrieve_base,
            )
        },
        content_type,
        remaining.is_some(),
        remaining,
    ) {
        return Ok(xml);
    }

    let mut body = Vec::with_capacity(page_len * 256 + 2);
    body.push(b'[');
    let mut scratch = [0u8; 4096];
    for (i, gi) in page.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        match clarus_core::query::json::write_instance_json(
            &mut scratch,
            gi.instance,
            gi.study_uid,
            gi.series_uid,
            query.includefield.as_deref(),
            retrieve_base,
        ) {
            Ok(n) => body.extend_from_slice(&scratch[..n]),
            Err(_) => return Ok(btree_internal_error()),
        }
    }
    body.push(b']');

    Ok(QidoResp {
        status: STATUS_OK,
        content_type: DICOM_JSON_CONTENT_TYPE,
        truncated: remaining.is_some(),
        remaining,
        body,
    })
}

/// Health check - K8s liveness/readiness probe (Fix 7).
///
/// Returns `(status, message)`: 200 `"OK"`, or 503 with a subsystem tag:
/// `STORAGE_MISSING`, `DISK_DEGRADED`.
/// The checks run in this exact order (part of the contract). The SQLite
/// `DB_UNREACHABLE` check is gone (backend excised).
pub fn health_check(state: &AppState) -> (u16, &'static str) {
    // 1. CAS storage - does the directory exist?
    if !std::path::Path::new(&state.config.storage_dir).exists() {
        return (STATUS_SERVICE_UNAVAILABLE, "STORAGE_MISSING");
    }

    // 2. Check disk health - is storage responding? A probe that cannot write
    //    at all is the same degradation as a slow one: the read/write path the
    //    probe exercises is down, so the instance is not servable. Before the
    //    first pass there is no measurement, and the honest answer is the
    //    storage check above (already passed), not a suspicion.
    if crate::disk_health::probe_has_run() {
        match crate::disk_health::probe_latency_ms() {
            Some(disk_ms) if disk_ms > 30_000 => {
                return (STATUS_SERVICE_UNAVAILABLE, "DISK_DEGRADED");
            }
            Some(_) => {}
            None => return (STATUS_SERVICE_UNAVAILABLE, "DISK_PROBE_FAILED"),
        }
    }

    // 3. manifest-degraded. CAS is alive but manifests/ is missing/empty
    //    (server blind to stored studies) or corrupt manifests were seen.
    //    Defaults OFF (0) so a healthy store reports OK; only set by the
    //    lazy-recovery startup sanity check or a REBUILD report.
    use std::sync::atomic::Ordering;
    if crate::metrics::MANIFEST_DEGRADED.load(Ordering::Relaxed) != 0 {
        let tag = if crate::metrics::MANIFEST_MISSING.load(Ordering::Relaxed) != 0 {
            "MANIFEST_MISSING"
        } else {
            "MANIFEST_CORRUPT"
        };
        return (STATUS_SERVICE_UNAVAILABLE, tag);
    }

    (STATUS_OK, "OK")
}

/// Build the `/health/status` JSON body (viewer
/// indicator line). The server computes states + DISK color; the viewer only
/// renders. Indicators in spec order: MNF -> IDX -> ERASE -> DISK -> RSS ->
/// S3 -> OBJ. OBJ is LAST (the one unbounded-growth gauge, a scale metric not
/// a health signal). Prometheus text is never parsed by the JS - this endpoint
/// is the structured source of truth.
///
/// Field semantics:
/// - `mnf`: `ok` | `crpt` (some corrupt manifests seen) | `missing` (CAS alive
///   but no manifests - "server blind"); `mnf_n` = corrupt-manifest count.
/// - `idx` / `erase`: IDX stale objects, ERASE pending tombstones.
/// - `disk_pct` (0-100), `disk_color`: `green` (>= 2x emergency_free_gb),
///   `amber` (< 2x), `red` (below the threshold / emergency in progress).
/// - `rss_mb`: process RSS in whole MB.
/// - `s3` / `s3_ms`: `ok`+latency, `down`, or `na` (not configured / not built
///   with `--features s3`).
/// - `obj`: STORAGE_OBJECTS_TOTAL (the unbounded-growth physical gauge).
/// - `instances` / `unreferenced` (3008): the logical instance count across
///   live manifests and the physical copies referenced by NO live manifest
///   (replaced-version orphans + hidden-PHI alibi copies + crash-window
///   leaks). Together they explain OBJ: obj ~ instances + unreferenced
///   (+ dedup overlap).
/// - `q_limit`: the live `[query] default_limit` (0 = unlimited) - the
///   viewer's patient list follows it and the header displays it.
pub fn health_status(state: &AppState) -> String {
    use std::sync::atomic::Ordering;

    // MNF - manifest-degradation state from the statics.
    let missing = crate::metrics::MANIFEST_MISSING.load(Ordering::Relaxed) != 0;
    let corrupt = crate::metrics::MANIFEST_CORRUPT.load(Ordering::Relaxed);

    let obj = crate::metrics::STORAGE_OBJECTS_TOTAL.load(Ordering::Relaxed);
    // 3008: logical instances and unreferenced copies (the OBJ decomposition).
    let instances = crate::metrics::STORAGE_INSTANCES_TOTAL.load(Ordering::Relaxed);
    let unreferenced = crate::metrics::STORAGE_UNREFERENCED_TOTAL.load(Ordering::Relaxed);
    let orphan = crate::metrics::ORPHANS_PENDING.load(Ordering::Relaxed);
    // The viewer shows LIVE runtime staleness (detected - healed), not the
    // one-time startup-pass counter (which stays in /metrics as
    // clarus_idx_stale_objects). 0 = clean; >0 = detected and being repaired.
    let idx = crate::metrics::idx_runtime_stale();
    let erase = crate::metrics::ERASE_PENDING.load(Ordering::Relaxed);
    let disk_pct = crate::metrics::DISK_FREE_PCT.load(Ordering::Relaxed);
    let free_gb = crate::metrics::DISK_FREE_GB.load(Ordering::Relaxed);
    let rss_mb = crate::metrics::PROCESS_RSS_BYTES.load(Ordering::Relaxed) / (1024 * 1024);

    // DISK color from the SAME threshold emergency migration uses.
    let threshold = state.config.migration.emergency_free_gb as u64;

    #[cfg(feature = "s3")]
    let (s3, s3_ms) = {
        let reach = crate::metrics::S3_REACHABLE.load(Ordering::Relaxed);
        let lat = crate::metrics::S3_LATENCY_MS.load(Ordering::Relaxed);
        if reach == 1 {
            ("ok", lat)
        } else if reach == 0 {
            ("down", 0u64)
        } else {
            ("na", 0u64)
        }
    };
    #[cfg(not(feature = "s3"))]
    let (s3, s3_ms) = ("na", 0u64);

    // 2608 bounded pool counters. Counters only, per the spec: `total` counts
    // completed transcodes/renders, `denied` the saturation 503s, `peak` the
    // high-water of simultaneously-held permits. In a default (no-transcode)
    // build these are 0 and honestly report 0.
    let transcode_total = crate::metrics::TRANSCODE_TOTAL.load(Ordering::Relaxed);
    let transcode_denied = crate::metrics::TRANSCODE_DENIED.load(Ordering::Relaxed);
    let transcode_peak = crate::metrics::TRANSCODE_PEAK.load(Ordering::Relaxed);

    // Identity indicator (design 9b): mode = require_proxy_identity; the
    // anonymous window counts anonymous PROTECTED requests in the last hour
    // (cheap per-minute ring fed by the access journal hook). The viewer
    // badge reads this field; the server only publishes it.
    let identity_mode = state.config.require_proxy_identity;
    let anonymous_window = crate::access_log::anonymous_window();

    // Effective QIDO default limit ([query] default_limit): the viewer's
    // patient list sends no explicit limit and follows this knob, so the
    // header displays the LIVE value instead of a viewer-side hardcode.
    let q_limit = state.config.query.default_limit;

    // 0810: CAS audit outcome for the viewer's red badge. The badge has THREE
    // states, driven by the audit STATE (not the gauges): only a COMPLETED
    // audit (`status == "done"`) yields a verdict - `clean` (0/0) or `corrupt`
    // (>0). Anything else (idle / running / error) is UNKNOWN - never red, and
    // never a premature green while the walk is still in progress. The two
    // counts are the last completed audit's totals.
    let (audit_corrupted, audit_unreadable, audit_state) = {
        let a = crate::handlers::admin::pure::AUDIT_STATE.read();
        match a {
            Ok(a) => {
                let tri = match a.status.as_str() {
                    "done" if a.corrupted > 0 || a.unreadable > 0 => "corrupt",
                    "done" => "clean",
                    _ => "unknown",
                };
                (a.corrupted, a.unreadable, tri)
            }
            // A poisoned lock must not be reported as clean: UNKNOWN.
            Err(_) => (0, 0, "unknown"),
        }
    };

    // 0210: one-line tier/commitment summary. `cold` reports whether the cold
    // tier is CONFIGURED (state.cold_cas), `w_mb`/`r_mb` are the migration
    // write and WADO read byte counters in MB, `commit` counts commitment
    // answers. Counters, not rates: an operator reads the direction of travel
    // and Prometheus derives the rate. No PHI (INVARIANT_3).
    let cold_configured = state.cold_cas.is_some();
    let write_mb = crate::metrics::STORAGE_WRITE_BYTES.load(Ordering::Relaxed) / (1024 * 1024);
    let read_mb = crate::metrics::STORAGE_READ_BYTES.load(Ordering::Relaxed) / (1024 * 1024);
    let commit_ok = crate::metrics::COMMITMENT_POST[clarus_core::http_status::STATUS_2XX]
        .load(Ordering::Relaxed)
        + crate::metrics::COMMITMENT_GET[clarus_core::http_status::STATUS_2XX]
            .load(Ordering::Relaxed);
    let commit_bad = crate::metrics::COMMITMENT_POST[clarus_core::http_status::STATUS_4XX]
        .load(Ordering::Relaxed)
        + crate::metrics::COMMITMENT_GET[clarus_core::http_status::STATUS_4XX]
            .load(Ordering::Relaxed);

    status_json(
        missing,
        corrupt,
        idx,
        erase,
        disk_pct,
        free_gb,
        threshold,
        rss_mb,
        s3,
        s3_ms,
        obj,
        instances,
        unreferenced,
        orphan,
        transcode_total,
        transcode_denied,
        transcode_peak,
        identity_mode,
        anonymous_window,
        q_limit,
        cold_configured,
        write_mb,
        read_mb,
        commit_ok,
        commit_bad,
        audit_corrupted,
        audit_unreadable,
        audit_state,
    )
}

/// Pure JSON assembly for `/health/status` (the server-side state I/O is done
/// by [`health_status`]; this function is kept separate so unit tests drive
/// the exact same formatting + DISK color without building an [`AppState`]).
///
/// DISK color thresholds (the same `emergency_free_gb` the
/// emergency-migration loop uses):
/// - `red` - free < threshold (or emergency migration in progress);
/// - `amber` - free in [threshold, 2xthreshold);
/// - `green` - free >= 2xthreshold or no threshold configured (0).
#[allow(clippy::too_many_arguments)]
fn status_json(
    missing: bool,
    corrupt: u64,
    idx: u64,
    erase: u64,
    disk_pct: u64,
    free_gb: u64,
    threshold: u64,
    rss_mb: u64,
    s3: &str,
    s3_ms: u64,
    obj: u64,
    instances: u64,
    unreferenced: u64,
    orphan: u64,
    transcode_total: u64,
    transcode_denied: u64,
    transcode_peak: u64,
    identity_strict: bool,
    anonymous_window: u64,
    q_limit: u64,
    cold_configured: bool,
    storage_write_mb: u64,
    storage_read_mb: u64,
    commit_2xx: u64,
    commit_4xx: u64,
    audit_corrupted: u64,
    audit_unreadable: u64,
    audit_state: &str,
) -> String {
    let (mnf, mnf_n) = if missing {
        ("missing", 0u64)
    } else if corrupt > 0 {
        ("crpt", corrupt)
    } else {
        ("ok", 0)
    };

    let disk_color = if threshold == 0 || free_gb >= threshold * 2 {
        "green"
    } else if free_gb >= threshold {
        "amber"
    } else {
        "red"
    };

    let identity_mode = if identity_strict {
        "strict"
    } else {
        "permissive"
    };

    // Which binary is answering? The same pair `/version` reports, so a viewer
    // badge and an operator's curl cannot disagree.
    let (version, build) = crate::version::build_pair();

    format!(
        "{{\"version\":\"{version}\",\"build\":\"{build}\",\
         \"mnf\":\"{mnf}\",\"mnf_n\":{mnf_n},\"idx\":{idx},\"erase\":{erase},\
         \"disk_pct\":{disk_pct},\"disk_color\":\"{disk_color}\",\"rss_mb\":{rss_mb},\
         \"s3\":\"{s3}\",\"s3_ms\":{s3_ms},\"obj\":{obj},\
         \"instances\":{instances},\"unreferenced\":{unreferenced},\"orph\":{orphan},\
         \"transcode_total\":{transcode_total},\"transcode_denied\":{transcode_denied},\
         \"transcode_peak\":{transcode_peak},\
         \"q_limit\":{q_limit},\
         \"audit_state\":\"{audit_state}\",\"audit_corrupted\":{audit_corrupted},\"audit_unreadable\":{audit_unreadable},\
         \"tiers\":{{\"cold\":\"{cold_state}\",\"w_mb\":{storage_write_mb},\"r_mb\":{storage_read_mb}}},\
         \"commit\":{{\"ok\":{commit_2xx},\"bad\":{commit_4xx}}},\
         \"identity\":{{\"mode\":\"{identity_mode}\",\"anonymous_window\":{anonymous_window}}}}}",
        cold_state = if cold_configured { "on" } else { "off" }
    )
}

// -- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The PN length errors must report CHARACTERS. The limit is counted in
    /// characters (PATIENT_NAME_MAX_LEN), so a message that reported bytes
    /// told a Cyrillic name of 65 characters it was 130 - the operator reads
    /// a number that does not match the rule they are being held to.
    #[test]
    fn a_cyrillic_name_over_the_limit_reports_its_character_count() {
        let long = "Ж".repeat(PATIENT_NAME_MAX_LEN + 1);
        assert_eq!(long.chars().count(), 65);
        assert_eq!(long.len(), 130, "precondition: 2 bytes per Cyrillic char");

        let query = QidoQuery {
            PatientName: Some(long),
            ..Default::default()
        };
        let message = match query.validate() {
            Err(e) => format!("{e:?}"),
            Ok(()) => panic!("a 65-character name must be refused"),
        };
        assert!(
            message.contains("65"),
            "the count is in characters, not bytes: {message}"
        );
        assert!(
            !message.contains("130"),
            "the byte length must not leak into the message: {message}"
        );
    }

    /// Field bug 22.08: percent-encoded UTF-8 decoded byte-as-scalar (Latin-1
    /// mojibake) - Cyrillic PatientName search found nothing, Latin worked.
    #[test]
    fn url_decode_cyrillic_utf8() {
        assert_eq!(
            url_decode("%D1%81%D0%BE%D0%BB%D0%B3%D0%B0%D0%BB%D0%BE%D0%B2"),
            "солгалов"
        );
        assert_eq!(
            url_decode("%D0%9C%D0%B0%D1%81%D1%82%D1%8F%D0%B5%D0%B2%D0%B0"),
            "Мастяева"
        );
        assert_eq!(url_decode("solga*"), "solga*");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("%D1%81%D0%BE%D0%BB%D0%B3%D0%B0*"), "солга*");
    }

    /// Same decoder must round-trip CJK (UTF-8, no case) and mixed scripts.
    #[test]
    fn url_decode_cjk_utf8() {
        assert_eq!(url_decode("%E5%BC%A0%E4%BC%9F"), "张伟");
        assert_eq!(
            url_decode("%E5%B1%B1%E7%94%B0%E5%A4%AA%E9%83%8E"),
            "山田太郎"
        );
        assert_eq!(url_decode("%E9%87%91%E6%99%BA%E7%A7%80"), "金智秀");
        // Mixed script: Cyrillic + Latin in one value.
        assert_eq!(url_decode("%D0%98%D0%B2%D0%B0%D0%BDov"), "Иванov");
        // Raw (unencoded) UTF-8 passes through byte-true.
        assert_eq!(url_decode("张"), "张");
    }

    /// `resolve_limit` matrix. Explicit client limit is
    /// honored without clamping; absent -> `[query] default_limit`; a default
    /// of 0 -> None (unlimited). `None` = unlimited.
    #[test]
    fn test_resolve_limit_matrix() {
        fn cfg(default_limit: u64) -> crate::config::QueryConfig {
            crate::config::QueryConfig {
                default_limit,
                ..crate::config::QueryConfig::default()
            }
        }
        let q = |limit| QidoQuery {
            limit,
            ..Default::default()
        };
        // Explicit 500 -> 500.
        assert_eq!(resolve_limit(&q(Some(500)), &cfg(250)), Some(500));
        // Explicit 1 -> 1.
        assert_eq!(resolve_limit(&q(Some(1)), &cfg(250)), Some(1));
        // Absent + default 250 -> 250.
        assert_eq!(resolve_limit(&q(None), &cfg(250)), Some(250));
        // Absent + default 0 -> None (unlimited).
        assert_eq!(resolve_limit(&q(None), &cfg(0)), None);
        // Explicit 0 -> 0: PS3.18 8.3.4.4.1 min-max computing returns no
        // results, so it must not be read as "unlimited".
        assert_eq!(resolve_limit(&q(Some(0)), &cfg(250)), Some(0));
        // Absent + explicit non-250 default -> that default.
        assert_eq!(resolve_limit(&q(None), &cfg(1000)), Some(1000));
    }

    /// `resolve_enumeration_limit` matrix (12.09): scoped enumeration
    /// (study's series, series' instances, study's instances) honors only the
    /// explicit client limit; the search `[query] default_limit` never applies.
    #[test]
    fn test_resolve_enumeration_limit_matrix() {
        let q = |limit| QidoQuery {
            limit,
            ..Default::default()
        };
        // Absent -> unlimited (the search default must NOT leak here).
        assert_eq!(resolve_enumeration_limit(&q(None)), None);
        // Explicit 0 -> unlimited.
        assert_eq!(resolve_enumeration_limit(&q(Some(0))), None);
        // Explicit client limit -> honored without clamping.
        assert_eq!(resolve_enumeration_limit(&q(Some(500))), Some(500));
        assert_eq!(resolve_enumeration_limit(&q(Some(1))), Some(1));
    }

    /// Unknown query keys are IGNORED (and logged),
    /// no longer a 400 - DICOM lets clients over-specify, and rejecting the
    /// whole request broke conformant clients. Matched known keys still take
    /// effect.
    #[test]
    fn test_unknown_query_keys_ignored() {
        // Known-only -> Ok, value preserved.
        let q = parse_qido_query("Modality=CT").expect("known-only ok");
        assert_eq!(q.Modality.as_deref(), Some("CT"));
        // Known + unknown -> Ok; the known value still parses (bogus ignored).
        let q = parse_qido_query("Modality=CT&bogus=1").expect("known+unknown ok");
        assert_eq!(q.Modality.as_deref(), Some("CT"));
        // Unknown-only -> Ok (all-None query, not an error).
        assert!(parse_qido_query("Foo=bar&baz=2").is_ok());
        // Matched hex alias with an unknown sibling -> Ok.
        let q = parse_qido_query("00200011=5&zzz=1").expect("hex-alias+unknown ok");
        assert_eq!(q.SeriesNumber.as_deref(), Some("5"));
    }

    /// P2-B Table 10.6.1-5 delta: `?SeriesNumber=` is a known search key, not
    /// a 400. Both the keyword and the 0020,0011 hex alias parse.
    #[test]
    fn test_seriesnumber_accepted() {
        let q = parse_qido_query("SeriesNumber=5").expect("SeriesNumber keyword ok");
        assert_eq!(q.SeriesNumber.as_deref(), Some("5"));
        let q = parse_qido_query("seriesnumber=5").expect("SeriesNumber lowercase ok");
        assert_eq!(q.SeriesNumber.as_deref(), Some("5"));
        let q = parse_qido_query("00200011=5").expect("SeriesNumber hex alias ok");
        assert_eq!(q.SeriesNumber.as_deref(), Some("5"));
        // No leftover -> not a 400.
        assert!(parse_qido_query("SeriesNumber=5&Modality=CT").is_ok());
    }

    /// `fuzzymatching` is an ABNF literal - `true`/`false` are
    /// case-insensitive (PS3.18 8.3.4). `TRUE`/`False` parse exactly like
    /// their lowercase forms; an invalid value stays absent (no 400).
    #[test]
    fn test_fuzzymatching_case_insensitive() {
        let q = parse_qido_query("fuzzymatching=TRUE").expect("TRUE parses");
        assert_eq!(q.fuzzymatching, Some(true));
        let q = parse_qido_query("fuzzymatching=False").expect("False parses");
        assert_eq!(q.fuzzymatching, Some(false));
        let q = parse_qido_query("fuzzymatching=true").expect("true parses");
        assert_eq!(q.fuzzymatching, Some(true));
        let q = parse_qido_query("fuzzymatching=fAlSe").expect("mixed-case parses");
        assert_eq!(q.fuzzymatching, Some(false));
        // Invalid value -> absent (the pre-existing absent semantics).
        let q = parse_qido_query("fuzzymatching=banana").expect("invalid is absent");
        assert_eq!(q.fuzzymatching, None);
    }

    /// `emptyvaluematching` and `multiplevaluematching` (PS3.18 8.3.9.2)
    /// parse as case-insensitive ABNF booleans, like `fuzzymatching`. Absent
    /// defaults to None. Semantics (held in-field doc on QidoQuery):
    /// `multiplevaluematching=true` conforms to the B-tree tier's CSV any-element
    /// matching on Modality/ModalitiesInStudy (already the matcher rule);
    /// `emptyvaluematching` is accepted-and-a-no-op because the manifest stores no
    /// empty single-value attributes.
    #[test]
    fn test_empty_and_multiple_value_matching_parse_case_insensitive() {
        for flag in ["emptyvaluematching", "multiplevaluematching"] {
            let q = parse_qido_query(&format!("{flag}=TRUE")).expect("TRUE parses");
            let got = if flag == "emptyvaluematching" {
                q.emptyvaluematching
            } else {
                q.multiplevaluematching
            };
            assert_eq!(got, Some(true), "{flag}=TRUE");

            let q = parse_qido_query(&format!("{flag}=false")).expect("false parses");
            let got = if flag == "emptyvaluematching" {
                q.emptyvaluematching
            } else {
                q.multiplevaluematching
            };
            assert_eq!(got, Some(false), "{flag}=false");

            let q = parse_qido_query(&format!("{flag}=TrUe")).expect("mixed parses");
            let got = if flag == "emptyvaluematching" {
                q.emptyvaluematching
            } else {
                q.multiplevaluematching
            };
            assert_eq!(got, Some(true), "{flag}=TrUe");

            // Invalid value -> absent.
            let q = parse_qido_query(&format!("{flag}=banana")).expect("invalid absent");
            let got = if flag == "emptyvaluematching" {
                q.emptyvaluematching
            } else {
                q.multiplevaluematching
            };
            assert_eq!(got, None, "{flag}=banana");

            // Consumed as a named modifier at every QIDO level (never a 400).
            let qs = format!("{flag}=true");
            for lvl in [QidoLevel::Study, QidoLevel::Series, QidoLevel::Instance] {
                assert!(
                    parse_and_validate(Some(qs.as_str()), lvl).is_ok(),
                    "{flag}=true must parse at each QIDO level"
                );
            }
        }
    }

    /// Table 10.6.1-5: SOPClassUID and InstanceNumber are instance-level
    /// matching keys - keyword and hex alias both parse, no longer a 400.
    #[test]
    fn test_instance_keys_accepted() {
        let q = parse_qido_query("SOPClassUID=1.2.840.10008.5.1.4.1.1.2").expect("keyword ok");
        assert_eq!(q.SOPClassUID.as_deref(), Some("1.2.840.10008.5.1.4.1.1.2"));
        let q = parse_qido_query("00080016=1.2.840.10008.5.1.4.1.1.4").expect("hex ok");
        assert_eq!(q.SOPClassUID.as_deref(), Some("1.2.840.10008.5.1.4.1.1.4"));
        let q = parse_qido_query("InstanceNumber=7").expect("keyword ok");
        assert_eq!(q.InstanceNumber.as_deref(), Some("7"));
        let q = parse_qido_query("00200013=7").expect("hex ok");
        assert_eq!(q.InstanceNumber.as_deref(), Some("7"));
        // Both are native at the instance level (not foreign).
        assert!(parse_and_validate(Some("00080016=1.2.3"), QidoLevel::Instance).is_ok());
        assert!(parse_and_validate(Some("00200013=7"), QidoLevel::Instance).is_ok());
        // SOPClassUID keeps the same UID validation as the other UID keys.
        let err = parse_and_validate(Some("00080016=abc!"), QidoLevel::Instance)
            .expect_err("bad SOPClassUID must 400");
        assert_eq!(err.status, STATUS_BAD_REQUEST);
    }

    /// The instance keys stay FOREIGN on the study/series routes
    /// (10.6.1.2.1) - `/dicomweb/studies?00080016=` remains a 400.
    #[test]
    fn test_instance_keys_foreign_on_study_and_series() {
        let err = parse_and_validate(Some("00080016=1.2.3"), QidoLevel::Study)
            .expect_err("SOPClassUID at /studies must 400");
        assert!(err.message.contains("SOPClassUID"), "{}", err.message);
        let err = parse_and_validate(Some("00200013=7"), QidoLevel::Study)
            .expect_err("InstanceNumber at /studies must 400");
        assert!(err.message.contains("InstanceNumber"), "{}", err.message);
        let err = parse_and_validate(Some("00080016=1.2.3"), QidoLevel::Series)
            .expect_err("SOPClassUID at /series must 400");
        assert!(err.message.contains("SOPClassUID"), "{}", err.message);
        let err = parse_and_validate(Some("00200013=7"), QidoLevel::Series)
            .expect_err("InstanceNumber at /series must 400");
        assert!(err.message.contains("InstanceNumber"), "{}", err.message);
    }

    /// Modality is a Series IE key: native at the Study (documented
    /// first-series-modality deviation) and Series levels, legal on the
    /// RELATIONAL instance resources (10.6.1.2.1), and rejected ONLY on the
    /// strict (hierarchical) instance path.
    #[test]
    fn test_modality_key_scope() {
        // Legal at the study and series levels.
        assert!(
            parse_and_validate(Some("Modality=CT"), QidoLevel::Study).is_ok(),
            "Modality is accepted at the study level (documented deviation)"
        );
        assert!(
            parse_and_validate(Some("Modality=CT"), QidoLevel::Series).is_ok(),
            "Modality is native at the series level"
        );

        // Legal on the relational instance resources via the allowlists.
        assert!(
            parse_and_validate_with(Some("Modality=CT"), QidoLevel::Instance, &SERIES_IE_KEYS)
                .is_ok(),
            "scoped study-instances must accept Modality"
        );
        assert!(
            parse_and_validate_with(
                Some("Modality=CT"),
                QidoLevel::Instance,
                &ALL_STUDY_SERIES_KEYS
            )
            .is_ok(),
            "global instances must accept Modality"
        );

        // Legal on the relational instance resources via the allowlists, and
        // APPLIED there (see `btree_study_instances` / `qido_instances_global`):
        // accepting a filter and ignoring it answered a wider set than asked.
        assert!(
            parse_and_validate_with(
                Some("Modality=CT&SeriesNumber=7"),
                QidoLevel::Instance,
                &SERIES_IE_KEYS
            )
            .is_ok(),
            "scoped study-instances must accept the Series IE keys it applies"
        );

        // Strict (no allowlist) - the hierarchical route's policy: 400.
        let err = parse_and_validate(Some("Modality=CT&SeriesNumber=7"), QidoLevel::Instance)
            .expect_err("strict instance validation must reject Modality");
        assert!(err.message.contains("Modality"), "{}", err.message);
    }

    /// PS3.18 Table 10.6.1-5: PPS series keys parse via keyword, lowercase and
    /// 8-hex-tag alias. RequestAttributesSequence (SQ) is accepted as a key.
    #[test]
    fn test_pps_series_keys_parse_kw_and_hex() {
        let q = parse_qido_query("PerformedProcedureStepStartDate=20260802")
            .expect("PPS date keyword ok");
        assert_eq!(
            q.PerformedProcedureStepStartDate.as_deref(),
            Some("20260802")
        );
        let q = parse_qido_query("performedprocedurestepstartdate=20260802")
            .expect("PPS date lowercase ok");
        assert_eq!(
            q.PerformedProcedureStepStartDate.as_deref(),
            Some("20260802")
        );
        let q = parse_qido_query("00400244=20260802").expect("PPS date hex ok");
        assert_eq!(
            q.PerformedProcedureStepStartDate.as_deref(),
            Some("20260802")
        );

        let q = parse_qido_query("PerformedProcedureStepStartTime=104500.123")
            .expect("PPS time keyword ok");
        assert_eq!(
            q.PerformedProcedureStepStartTime.as_deref(),
            Some("104500.123")
        );
        let q = parse_qido_query("00400245=104500").expect("PPS time hex ok");
        assert_eq!(q.PerformedProcedureStepStartTime.as_deref(), Some("104500"));

        let q = parse_qido_query("RequestAttributesSequence=1.2.840.10008").expect("RAS kw ok");
        assert_eq!(
            q.RequestAttributesSequence.as_deref(),
            Some("1.2.840.10008")
        );
        let q = parse_qido_query("00400275=1.2.840.10008").expect("RAS hex ok");
        assert_eq!(
            q.RequestAttributesSequence.as_deref(),
            Some("1.2.840.10008")
        );
    }

    /// 0040,0244 is DA `YYYYMMDD`; 0040,0245 is TM `HHMMSS[.ffffff]`.
    /// 0040,0275 is an SQ - accepted without scalar validation.
    #[test]
    fn test_pps_series_keys_validation() {
        let err = parse_and_validate(
            Some("PerformedProcedureStepStartDate=2026"),
            QidoLevel::Series,
        )
        .expect_err("short PPS date must 400");
        assert!(
            err.message
                .contains("PerformedProcedureStepStartDate must match pattern YYYYMMDD"),
            "{}",
            err.message
        );
        let err = parse_and_validate(
            Some("PerformedProcedureStepStartTime=10"),
            QidoLevel::Series,
        )
        .expect_err("short PPS time must 400");
        assert!(
            err.message
                .contains("PerformedProcedureStepStartTime must match pattern HHMMSS[.ffffff]"),
            "{}",
            err.message
        );
        // Valid date/time pass; SQ passes with any scalar.
        assert!(
            parse_and_validate(
                Some("PerformedProcedureStepStartDate=20260802"),
                QidoLevel::Series
            )
            .is_ok()
        );
        assert!(
            parse_and_validate(
                Some("PerformedProcedureStepStartTime=104500.123"),
                QidoLevel::Series
            )
            .is_ok()
        );
        assert!(
            parse_and_validate(Some("RequestAttributesSequence=xyz"), QidoLevel::Series).is_ok()
        );
    }

    /// PPS series keys are FOREIGN at Study and Instance levels, native
    /// at the Series level (mirrors SeriesNumber/SeriesDate).
    #[test]
    fn test_pps_series_keys_foreign_level_matrix() {
        let err = parse_and_validate(
            Some("PerformedProcedureStepStartDate=20260802"),
            QidoLevel::Study,
        )
        .expect_err("PPS date at /studies must 400");
        assert!(
            err.message
                .contains("unsupported search keys for studies: PerformedProcedureStepStartDate"),
            "{}",
            err.message
        );
        let err = parse_and_validate(Some("00400245=104500"), QidoLevel::Instance)
            .expect_err("PPS time at /instances must 400");
        assert!(
            err.message
                .contains("unsupported search keys for instances: PerformedProcedureStepStartTime"),
            "{}",
            err.message
        );
        let err = parse_and_validate(Some("RequestAttributesSequence=x"), QidoLevel::Study)
            .expect_err("RAS at /studies must 400");
        assert!(
            err.message.contains("RequestAttributesSequence"),
            "{}",
            err.message
        );
        // All three are native at the Series level (no 400).
        assert!(
            parse_and_validate(
                Some("PerformedProcedureStepStartDate=20260802"),
                QidoLevel::Series
            )
            .is_ok()
        );
        assert!(
            parse_and_validate(
                Some("PerformedProcedureStepStartTime=104500"),
                QidoLevel::Series
            )
            .is_ok()
        );
        assert!(parse_and_validate(Some("RequestAttributesSequence=x"), QidoLevel::Series).is_ok());
    }

    /// 23.08 audit: search keys of another QIDO level must 400, not silently
    /// no-op (a silent no-op returns spuriously wide results, and the study
    /// filter's SeriesInstanceUID/SeriesNumber mapping would silently match
    /// nothing - their tags never exist in study attrs).
    #[test]
    fn test_foreign_level_keys_rejected() {
        // Study route: series/instance-level keys -> 400.
        let err = parse_and_validate(Some("SeriesNumber=5"), QidoLevel::Study)
            .expect_err("SeriesNumber at /studies must 400");
        assert_eq!(err.status, STATUS_BAD_REQUEST);
        assert!(
            err.message
                .contains("unsupported search keys for studies: SeriesNumber"),
            "{}",
            err.message
        );
        let err = parse_and_validate(Some("00080018=1.2.3.4.5"), QidoLevel::Study)
            .expect_err("SOPInstanceUID at /studies must 400");
        assert!(err.message.contains("SOPInstanceUID"), "{}", err.message);

        // Series route: study-level keys -> 400.
        let err = parse_and_validate(Some("PatientName=IVANOV"), QidoLevel::Series)
            .expect_err("PatientName at /series must 400");
        assert!(
            err.message
                .contains("unsupported search keys for series: PatientName"),
            "{}",
            err.message
        );

        // Instance route: study + series keys -> 400 (sorted names, joined).
        let err = parse_and_validate(Some("Modality=CT&PatientName=IVANOV"), QidoLevel::Instance)
            .expect_err("Modality/PatientName at /instances must 400");
        assert_eq!(
            err.message,
            "unsupported search keys for instances: Modality, PatientName"
        );

        // Same-level keys still pass (ModalitiesInStudy on /studies is the
        // Weasis fix; SeriesNumber on /series is the documented 12.08 key).
        assert!(parse_and_validate(Some("00080061=CT"), QidoLevel::Study).is_ok());
        assert!(parse_and_validate(Some("Modality=CT&SeriesNumber=5"), QidoLevel::Series).is_ok());
        assert!(parse_and_validate(Some("00080018=1.2.3"), QidoLevel::Instance).is_ok());
        assert!(parse_and_validate(None, QidoLevel::Study).is_ok());
    }

    /// Keys are case-insensitive; `?modality=CT` == `?Modality=CT`.
    #[test]
    fn test_query_keys_case_insensitive() {
        let lower = parse_qido_query("modality=CT").expect("lowercase ok");
        let camel = parse_qido_query("Modality=CT").expect("camel ok");
        assert_eq!(lower.Modality.as_deref(), Some("CT"));
        assert_eq!(lower.Modality, camel.Modality);
        // Mixed-case keyword is accepted too.
        let mixed = parse_qido_query("MOdAlItY=CT").expect("mixed ok");
        assert_eq!(mixed.Modality.as_deref(), Some("CT"));
    }

    /// The 8-hex-digit tag alias of every search key works.
    #[test]
    fn test_query_hex_aliases_work() {
        // (keyword, hex): the 14 documented search keys.
        for (kw, hex, want) in [
            ("PatientName", "00100010", "P^A"),
            ("PatientID", "00100020", "PT-77"),
            ("StudyDate", "00080020", "20230515"),
            ("Modality", "00080060", "CT"),
            ("AccessionNumber", "00080050", "ACC-1"),
            ("StudyDescription", "00081030", "desc"),
            ("PatientBirthDate", "00100030", "19900101"),
            ("PatientSex", "00100040", "M"),
            ("ReferringPhysicianName", "00080090", "DOC^J"),
            ("StudyInstanceUID", "0020000D", "1.2.3"),
            ("StudyTime", "00080030", "103000"),
            ("StudyID", "00200010", "S1"),
            ("SeriesInstanceUID", "0020000E", "1.2.3.4"),
            ("SeriesNumber", "00200011", "5"),
            ("SOPInstanceUID", "00080018", "1.2.3.4.5"),
            ("SOPClassUID", "00080016", "1.2.3.4"),
            ("InstanceNumber", "00200013", "2"),
        ] {
            let q = parse_qido_query(&format!("{hex}={want}"))
                .unwrap_or_else(|_| panic!("{kw} hex {hex} should parse"));
            let got = match kw {
                "PatientName" => q.PatientName,
                "PatientID" => q.PatientID,
                "StudyDate" => q.StudyDate,
                "Modality" => q.Modality,
                "AccessionNumber" => q.AccessionNumber,
                "StudyDescription" => q.StudyDescription,
                "PatientBirthDate" => q.PatientBirthDate,
                "PatientSex" => q.PatientSex,
                "ReferringPhysicianName" => q.ReferringPhysicianName,
                "StudyInstanceUID" => q.StudyInstanceUID,
                "StudyTime" => q.StudyTime,
                "StudyID" => q.StudyID,
                "SeriesInstanceUID" => q.SeriesInstanceUID,
                "SeriesNumber" => q.SeriesNumber,
                "SOPInstanceUID" => q.SOPInstanceUID,
                "SOPClassUID" => q.SOPClassUID,
                "InstanceNumber" => q.InstanceNumber,
                _ => unreachable!(),
            }
            .unwrap_or_else(|| panic!("{kw} should be set from {hex}"));
            assert_eq!(got, want, "{kw} from {hex}");
        }
    }

    #[test]
    fn test_validate_patient_name_ascii_64_chars() {
        let q = QidoQuery {
            PatientName: Some("A".repeat(64)),
            ..Default::default()
        };
        assert!(q.validate().is_ok());
    }

    #[test]
    fn test_validate_patient_name_ascii_65_chars_rejected() {
        let q = QidoQuery {
            PatientName: Some("B".repeat(65)),
            ..Default::default()
        };
        assert!(q.validate().is_err());
    }

    #[test]
    fn test_validate_patient_name_cyrillic_32_chars_accepted() {
        // 32 pure Cyrillic (2 bytes/char) = 64 bytes. Must pass chars().count() check.
        // 0x430..=0x44F is the lowercase Cyrillic block; expect is safe by range.
        let cyrillic: String = (0x430u32..=0x44F)
            .take(32)
            .map(|c| char::from_u32(c).expect("0x430..=0x44F are valid scalar values"))
            .collect();
        assert_eq!(cyrillic.chars().count(), 32);
        assert_eq!(cyrillic.len(), 64);
        let q = QidoQuery {
            PatientName: Some(cyrillic),
            ..Default::default()
        };
        assert!(q.validate().is_ok(), "Cyrillic 32 chars must pass");
    }

    #[test]
    fn test_validate_patient_name_dicom_format_cyrillic() {
        // Realistic DICOM PN: Cyrillic capitals "SMIRNOV^IVAN" (12 chars) - well within limit.
        let name = "\u{0421}\u{041C}\u{0418}\u{0420}\u{041D}\u{041E}\u{0412}^\u{0418}\u{0412}\u{0410}\u{041D}"
            .to_string();
        assert_eq!(name.chars().count(), 12);
        let q = QidoQuery {
            PatientName: Some(name),
            ..Default::default()
        };
        assert!(q.validate().is_ok(), "DICOM Cyrillic name must pass");
    }

    #[test]
    fn test_validate_patient_name_cyrillic_33_chars_passes_chars_count() {
        // 33 Cyrillic chars = 66 bytes. Would FAIL with .len() but MUST PASS with .chars().count()
        // because 33 characters <= 64 character limit.
        let cyrillic: String = (0x430u32..=0x44F)
            .cycle()
            .take(33)
            .map(|c| char::from_u32(c).expect("0x430..=0x44F are valid scalar values"))
            .collect();
        assert_eq!(cyrillic.chars().count(), 33);
        assert!(cyrillic.len() > 64, "old .len() check would reject this"); // 66 > 64
        let q = QidoQuery {
            PatientName: Some(cyrillic),
            ..Default::default()
        };
        assert!(
            q.validate().is_ok(),
            "Cyrillic 33 chars (66 bytes) must pass chars().count() check"
        );
    }

    /// the two Weasis physician search keys parse by keyword and by
    /// 8-hex alias, in any case, and no longer trip the unknown-key 400 gate.
    #[test]
    fn test_performing_and_reading_physician_keys_parse() {
        // Keyword form (mixed case) accepted.
        let q = parse_qido_query("PerformingPhysicianName=Perf^Doc").expect("keyword ok");
        assert_eq!(q.PerformingPhysicianName.as_deref(), Some("Perf^Doc"));
        let q = parse_qido_query("NameOfPhysiciansReadingStudy=Read^Doc").expect("keyword ok");
        assert_eq!(q.NameOfPhysiciansReadingStudy.as_deref(), Some("Read^Doc"));

        // Hex alias accepted (lowercase hex).
        let q = parse_qido_query("00081050=Perf^Doc").expect("hex performing ok");
        assert_eq!(q.PerformingPhysicianName.as_deref(), Some("Perf^Doc"));
        let q = parse_qido_query("00081060=Read^Doc").expect("hex reading ok");
        assert_eq!(q.NameOfPhysiciansReadingStudy.as_deref(), Some("Read^Doc"));

        // Mixed-case keyword + unknown-key regression: a filled field is no
        // longer a 400 (it is now a recognized key).
        let q =
            parse_qido_query("PeRfOrMiNgPhYsIcIaNnAmE=Doc").expect("case-insensitive keyword ok");
        assert_eq!(q.PerformingPhysicianName.as_deref(), Some("Doc"));
    }

    #[test]
    fn test_responsible_person_key_parses_kw_and_hex() {
        // FR-vet stage 1: ResponsiblePerson (0010,2297) is a recognized vendor
        // search key - both keyword and hex forms parse and carry the value.
        let q = parse_qido_query("ResponsiblePerson=Иванов^Иван").expect("keyword ok");
        assert_eq!(q.ResponsiblePerson.as_deref(), Some("Иванов^Иван"));

        let q = parse_qido_query("00102297=Петров").expect("hex ok");
        assert_eq!(q.ResponsiblePerson.as_deref(), Some("Петров"));

        // Mixed-case keyword accepted (QIDO keys are case-insensitive).
        let q = parse_qido_query("rEsPoNsIbLePeRsOn=Doc").expect("case-insensitive ok");
        assert_eq!(q.ResponsiblePerson.as_deref(), Some("Doc"));

        // It must NOT be reported as an unsupported search key.
        let q = parse_qido_query("ResponsiblePerson=Иванов&Species=Кот").expect("parse ok");
        assert_eq!(q.ResponsiblePerson.as_deref(), Some("Иванов"));
        // The genuinely-unknown species key is still logged as unsupported.
    }

    #[test]
    fn test_performing_and_reading_physician_too_long_400() {
        // 65 chars > 64 limit -> 400 naming the key.
        let q = QidoQuery {
            PerformingPhysicianName: Some("P".repeat(65)),
            ..Default::default()
        };
        let err = q.validate().expect_err("performing >64 must 400");
        assert!(
            err.message.contains("PerformingPhysicianName"),
            "{}",
            err.message
        );

        let q = QidoQuery {
            NameOfPhysiciansReadingStudy: Some("R".repeat(65)),
            ..Default::default()
        };
        let err = q.validate().expect_err("reading >64 must 400");
        assert!(
            err.message.contains("NameOfPhysiciansReadingStudy"),
            "{}",
            err.message
        );

        // Exactly 64 chars passes.
        let q = QidoQuery {
            PerformingPhysicianName: Some("P".repeat(64)),
            ..Default::default()
        };
        assert!(q.validate().is_ok());
    }

    #[test]
    fn test_validate_study_date_exact() {
        let q = QidoQuery {
            StudyDate: Some("20200101".to_string()),
            ..Default::default()
        };
        assert!(q.validate().is_ok());
    }

    #[test]
    fn test_validate_study_date_rejects_iso8601() {
        let q = QidoQuery {
            StudyDate: Some("2020-01-01".to_string()),
            ..Default::default()
        };
        assert!(q.validate().is_err());
    }

    /// PS3.4 C.2.2.2.5: either end of a range may be absent, so "everything
    /// since a date" and "everything up to a date" are legal keys. Each end
    /// must still be a DA; the absence of both ends is not a constraint and is
    /// refused rather than read as "match everything".
    #[test]
    fn test_validate_study_date_open_ended_ranges() {
        for accepted in ["20200101-", "-20201231"] {
            let q = QidoQuery {
                StudyDate: Some(accepted.to_string()),
                ..Default::default()
            };
            assert!(q.validate().is_ok(), "{accepted} must be accepted");
        }
        for rejected in ["-", "2020-", "-2020", "2020010-", "-202012311"] {
            let q = QidoQuery {
                StudyDate: Some(rejected.to_string()),
                ..Default::default()
            };
            assert!(q.validate().is_err(), "{rejected} must be rejected");
        }
        // The parsed bounds reach the filter as one-sided ranges, which is
        // what `Predicate::DateRange` already answers (date >= lo only).
        let open = QidoQuery {
            StudyDate: Some("20200101-".to_string()),
            ..Default::default()
        };
        let (from, to) = open.study_date_range();
        assert_eq!(from.as_deref(), Some("20200101"));
        assert_eq!(to.as_deref(), None);
        let closed = QidoQuery {
            StudyDate: Some("-20201231".to_string()),
            ..Default::default()
        };
        let (from, to) = closed.study_date_range();
        assert_eq!(from.as_deref(), None);
        assert_eq!(to.as_deref(), Some("20201231"));
    }

    // -- /health/status ------------------------------------------------

    #[test]
    fn health_status_ok_reports_all_indicators() {
        // New spec order: VERSION/BUILD, MNF, IDX, ERASE, DISK, RSS, S3, OBJ,
        // INST, UNREF, ORPH, then the 2608 transcode counters
        // (total/denied/peak).
        let body = status_json(
            false, 0, 0, 0, 87, 250, 50, 7, "ok", 38, 12345, 1058, 432, 5, 7, 3, 2, false, 0, 250,
            true, 0, 0, 0, 0, 0, 0, "unknown",
        );
        let (version, build) = crate::version::build_pair();
        assert_eq!(
            body,
            format!(
                "{{\"version\":\"{version}\",\"build\":\"{build}\",\
                 \"mnf\":\"ok\",\"mnf_n\":0,\"idx\":0,\"erase\":0,\
                 \"disk_pct\":87,\"disk_color\":\"green\",\"rss_mb\":7,\"s3\":\"ok\",\"s3_ms\":38,\"obj\":12345,\
                 \"instances\":1058,\"unreferenced\":432,\"orph\":5,\
                 \"transcode_total\":7,\"transcode_denied\":3,\"transcode_peak\":2,\
                 \"q_limit\":250,\
                 \"audit_state\":\"unknown\",\"audit_corrupted\":0,\"audit_unreadable\":0,\
                 \"tiers\":{{\"cold\":\"on\",\"w_mb\":0,\"r_mb\":0}},\
                 \"commit\":{{\"ok\":0,\"bad\":0}},\
                 \"identity\":{{\"mode\":\"permissive\",\"anonymous_window\":0}}}}"
            )
        );
        assert!(!body.contains("act"), "ACT must be removed: {body}");
        // The build identifies the binary, and is never the placeholder.
        assert!(!build.is_empty(), "build stamp must be present: {body}");
    }

    #[test]
    fn health_status_mnf_states_ok_crpt_missing() {
        // Corrupt manifests observed -> crpt (amber), carries the count.
        let crpt = status_json(
            false, 3, 0, 0, 90, 200, 50, 7, "na", 0, 1, 0, 0, 0, 0, 0, 0, false, 0, 250, true, 0,
            0, 0, 0, 0, 0, "unknown",
        );
        assert!(crpt.contains("\"mnf\":\"crpt\",\"mnf_n\":3,"), "{crpt}");
        // Missing manifests (CAS alive, server blind) -> missing (red).
        let miss = status_json(
            true, 0, 0, 0, 90, 200, 50, 7, "na", 0, 1, 0, 0, 0, 0, 0, 0, false, 0, 250, true, 0, 0,
            0, 0, 0, 0, "unknown",
        );
        assert!(miss.contains("\"mnf\":\"missing\",\"mnf_n\":0,"), "{miss}");
        // Healthy store -> ok.
        let ok = status_json(
            false, 0, 0, 0, 90, 200, 50, 7, "na", 0, 1, 0, 0, 0, 0, 0, 0, false, 0, 250, true, 0,
            0, 0, 0, 0, 0, "unknown",
        );
        assert!(ok.contains("\"mnf\":\"ok\""), "{ok}");
    }

    /// 0810: the tri-state CAS badge mapping. BEFORE the first audit the state
    /// is `unknown` (grey, never red); a completed clean audit is `clean`; a
    /// completed audit that found corrupted OR unreadable blobs is `corrupt`.
    #[test]
    fn health_status_audit_tri_state() {
        let mk = |st: &str, corr: u64, unread: u64| {
            status_json(
                false, 0, 0, 0, 90, 200, 50, 7, "na", 0, 1, 0, 0, 0, 0, 0, 0, false, 0, 250, true,
                0, 0, 0, 0, corr, unread, st,
            )
        };
        // No audit yet -> unknown, counts 0 (NEVER red).
        let unknown = mk("unknown", 0, 0);
        assert!(
            unknown.contains(
                "\"audit_state\":\"unknown\",\"audit_corrupted\":0,\"audit_unreadable\":0"
            ),
            "{unknown}"
        );
        // Clean audit -> clean.
        let clean = mk("clean", 0, 0);
        assert!(clean.contains("\"audit_state\":\"clean\""), "{clean}");
        // Corruption found -> corrupt, carries the counts.
        let corrupt = mk("corrupt", 2, 1);
        assert!(
            corrupt.contains(
                "\"audit_state\":\"corrupt\",\"audit_corrupted\":2,\"audit_unreadable\":1"
            ),
            "{corrupt}"
        );
        // Unreadable alone (no corruption) is ALSO corrupt.
        let unreadable_only = mk("corrupt", 0, 3);
        assert!(
            unreadable_only.contains(
                "\"audit_state\":\"corrupt\",\"audit_corrupted\":0,\"audit_unreadable\":3"
            ),
            "{unreadable_only}"
        );
    }

    #[test]
    fn health_status_disk_color_thresholds() {
        let mk = |free_gb: u64| {
            status_json(
                false, 0, 0, 0, 50, free_gb, 50, 7, "na", 0, 1, 0, 0, 0, 0, 0, 0, false, 0, 250,
                true, 0, 0, 0, 0, 0, 0, "unknown",
            )
        };
        // Red: free below the 50 GB threshold.
        assert!(mk(40).contains("\"disk_color\":\"red\""), "{}", mk(40));
        // Amber: free in [50, 100).
        assert!(mk(75).contains("\"disk_color\":\"amber\""), "{}", mk(75));
        // Green: free >= 2x threshold (100).
        assert!(mk(100).contains("\"disk_color\":\"green\""), "{}", mk(100));
        assert!(mk(200).contains("\"disk_color\":\"green\""), "{}", mk(200));
        // Threshold disabled (0): always green.
        let no_threshold = status_json(
            false, 0, 0, 0, 50, 5, 0, 7, "na", 0, 1, 0, 0, 0, 0, 0, 0, false, 0, 250, true, 0, 0,
            0, 0, 0, 0, "unknown",
        );
        assert!(
            no_threshold.contains("\"disk_color\":\"green\""),
            "{}",
            no_threshold
        );
    }

    #[test]
    fn health_status_s3_states_roundtrip() {
        let up = status_json(
            false, 0, 0, 0, 50, 200, 50, 7, "ok", 38, 1, 0, 0, 0, 0, 0, 0, false, 0, 250, true, 0,
            0, 0, 0, 0, 0, "unknown",
        );
        assert!(up.contains("\"s3\":\"ok\",\"s3_ms\":38,\"obj\":1"), "{up}");
        let down = status_json(
            false, 0, 0, 0, 50, 200, 50, 7, "down", 0, 1, 0, 0, 0, 0, 0, 0, false, 0, 250, true, 0,
            0, 0, 0, 0, 0, "unknown",
        );
        assert!(
            down.contains("\"s3\":\"down\",\"s3_ms\":0,\"obj\":1"),
            "{down}"
        );
        let na = status_json(
            false, 0, 0, 0, 50, 200, 50, 7, "na", 0, 1, 0, 0, 0, 0, 0, 0, false, 0, 250, true, 0,
            0, 0, 0, 0, 0, "unknown",
        );
        assert!(na.contains("\"s3\":\"na\",\"s3_ms\":0,\"obj\":1"), "{na}");
    }
}
