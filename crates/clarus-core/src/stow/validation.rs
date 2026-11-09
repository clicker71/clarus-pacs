// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Deterministic generation of empty DICOM Type 2 fields (B-007).
//!
//! DICOM PS3.3: PatientName (0010,0010), PatientID (0010,0020), and
//! StudyInstanceUID (0020,000D) are Type 2 - they may be present but empty.
//! Empty values would break both SQLite's B-tree indexes and the shadow
//! tier, so the server synthesises deterministic values:
//!
//! ```text
//! StudyInstanceUID (empty) -> BLAKE3(PatientID|StudyDate|StudyTime|Accession|Modality)
//!                          -> "2.25.{decimal}"   (ISO 9834-3 OID)
//! PatientID         (empty) -> BLAKE3(StudyUID)  -> "AUTOPAT_{12hex}"
//! PatientName       (empty) -> BLAKE3(StudyUID)  -> "Anonymous^{6hex}"
//! ```
//!
//! Determinism (same input -> same output) is what lets the shadow-read test
//! compare SQLite and B-tree answers byte-for-byte: both tiers derive the
//! same synthetic values from the same inputs.

use crate::hash::{Blake3CasHasher, CasHasher};

/// Errors produced by the empty-field generators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationError {
    /// A required input was empty, so a deterministic value cannot be built.
    EmptyInput(&'static str),
}

impl std::fmt::Display for GenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerationError::EmptyInput(which) => {
                write!(f, "cannot generate: required input {which} is empty")
            }
        }
    }
}

impl std::error::Error for GenerationError {}

/// BLAKE3 digest over a joined key, returned as a 32-byte array.
fn blake3_of(parts: &[&str]) -> [u8; 32] {
    let mut h = Blake3CasHasher::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            h.update(b"|");
        }
        h.update(part.as_bytes());
    }
    h.finalize()
}

/// Lowercase hex encoding of a byte slice (32 bytes -> 64 chars).
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Generate a deterministic StudyInstanceUID (0020,000D) when empty.
///
/// The UID is `"2.25."` + a decimal integer (ISO 9834-3 OID / UUID-as-URN
/// style root, `"2.25"` is the arc for UUID-based OIDs). The decimal is the
/// first 8 bytes of the BLAKE3 of the joined search inputs, read as a u64 -
/// a large, unique, but fully deterministic number.
pub fn generate_study_instance_uid(
    patient_id: &str,
    study_date: Option<&str>,
    study_time: Option<&str>,
    accession: Option<&str>,
    modality: &str,
) -> Result<String, GenerationError> {
    if patient_id.is_empty() {
        return Err(GenerationError::EmptyInput("PatientID"));
    }
    let digest = blake3_of(&[
        patient_id,
        study_date.unwrap_or(""),
        study_time.unwrap_or(""),
        accession.unwrap_or(""),
        modality,
    ]);
    // First 8 bytes as a big-endian u64 -> decimal string. Clearing the top bit
    // keeps the value within the positive 64-bit range so the decimal never
    // exceeds the OID ceiling of 2^63-1.
    let mut b = [0u8; 8];
    b.copy_from_slice(&digest[..8]);
    let n = u64::from_be_bytes(b) & 0x7fff_ffff_ffff_ffff;
    Ok(format!("2.25.{n}"))
}

/// Generate a deterministic PatientID (0010,0020) when empty.
///
/// `"AUTOPAT_"` + the first 12 hex chars of BLAKE3(StudyInstanceUID). All
/// instances of a study share the same StudyUID, so they get one PatientID.
pub fn generate_patient_id(study_instance_uid: &str) -> Result<String, GenerationError> {
    if study_instance_uid.is_empty() {
        return Err(GenerationError::EmptyInput("StudyInstanceUID"));
    }
    let h = hex(&blake3_of(&[study_instance_uid]));
    Ok(format!("AUTOPAT_{}", &h[..12]))
}

/// Generate a deterministic PatientName (0010,0010) when empty.
///
/// `"Anonymous^"` + the first 6 hex chars of BLAKE3(StudyInstanceUID), in PN
/// format (LastName^FirstName). Study-anchored - one name per study.
pub fn generate_patient_name(study_instance_uid: &str) -> Result<String, GenerationError> {
    if study_instance_uid.is_empty() {
        return Err(GenerationError::EmptyInput("StudyInstanceUID"));
    }
    let h = hex(&blake3_of(&[study_instance_uid]));
    Ok(format!("Anonymous^{}", &h[..6]))
}

/// Fallback StudyInstanceUID key when PatientID is empty too.
///
/// Returns the `u64` decimal body (already masked positive) that the server
/// turns into `2.25.{decimal}`. BLAKE3 over StudyDate|StudyTime|Accession|
/// Modality - deterministic within the study, same outer shape as the
/// PatientID-anchored path.
pub fn fallback_study_uid_key(
    study_date: Option<&str>,
    study_time: Option<&str>,
    accession: Option<&str>,
    modality: &str,
) -> u64 {
    let digest = blake3_of(&[
        study_date.unwrap_or(""),
        study_time.unwrap_or(""),
        accession.unwrap_or(""),
        modality,
    ]);
    let mut b = [0u8; 8];
    b.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(b) & 0x7fff_ffff_ffff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn study_uid_is_stable_and_oid_shaped() {
        let a =
            generate_study_instance_uid("P1", Some("20240115"), Some("120000"), Some("A1"), "CT")
                .expect("gen");
        let b =
            generate_study_instance_uid("P1", Some("20240115"), Some("120000"), Some("A1"), "CT")
                .expect("gen");
        assert_eq!(a, b, "deterministic");
        assert!(a.starts_with("2.25."), "ISO 9834-3 OID root: {a}");
        assert!(!a.is_empty());
    }

    #[test]
    fn study_uid_changes_with_inputs() {
        let a =
            generate_study_instance_uid("P1", Some("20240115"), Some("120000"), Some("A1"), "CT")
                .expect("a");
        let b =
            generate_study_instance_uid("P2", Some("20240115"), Some("120000"), Some("A1"), "CT")
                .expect("b");
        assert_ne!(a, b);
    }

    #[test]
    fn patient_id_is_study_anchored() {
        let study = "2.25.12345";
        let a = generate_patient_id(study).expect("a");
        let b = generate_patient_id(study).expect("b");
        assert_eq!(a, b);
        assert!(a.starts_with("AUTOPAT_"), "got {a}");
        assert_eq!(a.len(), "AUTOPAT_".len() + 12);
    }

    #[test]
    fn patient_name_is_study_anchored_pn() {
        let study = "2.25.12345";
        let a = generate_patient_name(study).expect("a");
        assert!(a.starts_with("Anonymous^"), "got {a}");
        assert_eq!(a.len(), "Anonymous^".len() + 6);
    }

    #[test]
    fn empty_required_input_errors() {
        assert!(generate_study_instance_uid("", None, None, None, "CT").is_err());
        assert!(generate_patient_id("").is_err());
        assert!(generate_patient_name("").is_err());
    }
}
