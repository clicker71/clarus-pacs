// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Deterministic generation of empty DICOM identity fields (B-007).
//!
//! DICOM PS3.3 Table C.7-1 gives PatientName (0010,0010) and PatientID
//! (0010,0020) Type 2 - they must be present but may be empty. StudyInstanceUID
//! (0020,000D) is Type 1 (PS3.3 Table C.7-3, "Unique identifier for the
//! Study"), so a conforming sender MUST supply a nonempty value, and an empty
//! one is a non-conformant data set.
//!
//! This module synthesises values for all three anyway. For StudyInstanceUID
//! that is a RECOVERY POLICY, not permission to omit the value: refusing every
//! data set that arrives with an empty Study UID would lose studies that the
//! archive can still serve under a derived identity, and the derived UID is
//! deterministic, so a retry derives the same one. The policy is recorded here
//! so nobody reads this support for the claim that Type 1 is optional.
//!
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

/// BLAKE3 digest over a list of components, joined unambiguously.
///
/// The components are concatenated with a `|` separator, but the separator is
/// only unambiguous if a `|` INSIDE a component cannot be mistaken for one.
/// PatientID (0010,0020) and AccessionNumber (0008,0050) are LO and may
/// contain `|`, and nothing on the read path filters them, so a plain join
/// made `("A|B", "C", ..)` and `("A", "B|C", ..)` hash to the SAME digest -
/// which becomes the StudyInstanceUID and therefore merges two unrelated
/// studies into one.
///
/// [`escape_component`] removes that: a component holding neither `|` nor `\`
/// is emitted verbatim (so every digest already written stays byte-identical),
/// and any other component is prefixed with 0x01 (SOH, not a legal value byte
/// in these VRs) and has its `|` and `\` backslash-escaped. The encoding is
/// injective: the marker can never begin a verbatim component, so a marked
/// component cannot be confused with a plain one.
fn blake3_of(parts: &[&str]) -> [u8; 32] {
    let mut h = Blake3CasHasher::new();
    let mut buf: Vec<u8> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            h.update(b"|");
        }
        buf.clear();
        escape_component(part, &mut buf);
        h.update(&buf);
    }
    h.finalize()
}

/// Append `part` to `out` in the unambiguous form [`blake3_of`] hashes.
///
/// Verbatim when the component holds neither separator, which is the case for
/// every UID this function has ever produced; marked and escaped otherwise.
fn escape_component(part: &str, out: &mut Vec<u8>) {
    let needs_escape = part.as_bytes().iter().any(|&b| b == b'|' || b == b'\\');
    if !needs_escape {
        out.extend_from_slice(part.as_bytes());
        return;
    }
    // SOH marks an encoded component. It cannot begin a verbatim one: a
    // component is escaped precisely when it contains a separator, and the
    // marker is written only in that case - so the two forms are disjoint even
    // if the value itself starts with SOH.
    out.push(0x01);
    for &b in part.as_bytes() {
        if b == b'|' || b == b'\\' {
            out.push(b'\\');
        }
        out.push(b);
    }
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
/// The UID is `"2.25."` + the decimal representation of a full 128-bit UUID
/// (ISO 9834-3 OID root `2.25`; PS3.5 B.2: the decimal of a 128-bit UUID, up
/// to 39 digits with leading zeros suppressed). The UUID is derived from the
/// BLAKE3 of the joined search inputs - a large, unique, fully deterministic
/// number - with the RFC 4122 version and variant bits set so it is a valid
/// UUID, not merely a 128-bit integer.
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
    Ok(format!("2.25.{}", uuid_u128_from_digest(&digest)))
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
/// Returns the 128-bit UUID integer the server turns into `2.25.{decimal}`.
/// BLAKE3 over StudyDate|StudyTime|Accession|Modality - deterministic within
/// the study, same outer shape as the PatientID-anchored path. The version and
/// variant bits are set, so the integer is a valid RFC 4122 UUID.
pub fn fallback_study_uid_key(
    study_date: Option<&str>,
    study_time: Option<&str>,
    accession: Option<&str>,
    modality: &str,
) -> u128 {
    let digest = blake3_of(&[
        study_date.unwrap_or(""),
        study_time.unwrap_or(""),
        accession.unwrap_or(""),
        modality,
    ]);
    uuid_u128_from_digest(&digest)
}

/// Derive a 128-bit UUID integer from the first 16 BLAKE3 bytes.
///
/// PS3.5 B.2: a `2.25.` UID is the decimal of a 128-bit UUID. A bare hash
/// prefix is neither 128 bits nor a UUID, so the version nibble (byte 6) and
/// the variant bits (byte 8) are set.
///
/// The version nibble stays at 4 even though RFC 9562 defines v4 as a RANDOM
/// or pseudorandom UUID and v8 as the custom/derived form this actually is.
/// The value is PERSISTED: it is the StudyInstanceUID written into every
/// manifest and every 2.25 derived UID, so changing the nibble renames every
/// study that ever entered with an empty Study UID - the same inputs would
/// derive a different UID and the old rows would never be found again. The
/// nibble is therefore a compatibility constant, not a claim about how the
/// bits were produced, and the comment says so rather than pretending v4 fits.
fn uuid_u128_from_digest(digest: &[u8; 32]) -> u128 {
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    // RFC 9562 5.4 would put 0x80 here for a derived (v8) UUID. Kept at 0x40
    // for the persisted-value reason above.
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80; // variant 10 (RFC 4122 4.1.1 / RFC 9562 4.1)
    u128::from_be_bytes(b)
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
        // PS3.5 B.2: the decimal of a 128-bit UUID has up to 39 digits. A
        // 63-bit hash derived at most 19 digits; >19 pins the 128-bit width.
        let decimal = a.strip_prefix("2.25.").expect("root present");
        assert!(
            decimal.len() > 19,
            "the 2.25 body must be a 128-bit decimal (>19 digits): {a}"
        );
        assert!(decimal.len() <= 39, "128-bit decimal is at most 39 digits");
    }

    #[test]
    fn fallback_study_uid_key_is_128_bit_and_deterministic() {
        let a = fallback_study_uid_key(Some("20240115"), Some("120000"), Some("A1"), "CT");
        let b = fallback_study_uid_key(Some("20240115"), Some("120000"), Some("A1"), "CT");
        assert_eq!(a, b, "deterministic");
        // The decimal of the 128-bit value must exceed 19 digits.
        assert!(a.to_string().len() > 19, "128-bit decimal: {a}");
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

    #[test]
    fn a_separator_inside_a_component_cannot_imitate_a_boundary() {
        // The two tuples are chosen so that their PLAIN `|`-joins are the same
        // byte string, which is what makes the assertion discriminating:
        // escaping cannot be dropped without both sides colliding.
        //
        //   A|20240115|120000|B + 20240116 + 130000 + C
        //   A + 20240115 + 120000 + B|20240116|130000|C
        //
        // both spell `A|20240115|120000|B|20240116|130000|C`. With escaping the
        // separator and the escape byte inside a component are marked, so the
        // two encodings differ.
        let left = generate_study_instance_uid(
            "A|20240115|120000|B",
            Some("20240116"),
            Some("130000"),
            Some("C"),
            "CT",
        )
        .expect("left");
        let right = generate_study_instance_uid(
            "A",
            Some("20240115"),
            Some("120000"),
            Some("B|20240116|130000|C"),
            "CT",
        )
        .expect("right");
        assert_ne!(left, right, "delimiter collision merged two studies");

        // Pin the premise: the plain `|`-joins really are the same byte string,
        // so the test cannot pass by accident on two unrelated inputs. Built
        // with `join` rather than `blake3_of`, because `blake3_of` already
        // escapes - the point is what the UNESCAPED form would have produced.
        let plain_left = ["A|20240115|120000|B", "20240116", "130000", "C", "CT"].join("|");
        let plain_right = ["A", "20240115", "120000", "B|20240116|130000|C", "CT"].join("|");
        assert_eq!(
            plain_left, plain_right,
            "the fixture tuples must collide under a plain join"
        );
    }

    #[test]
    fn a_backslash_component_cannot_imitate_the_escape() {
        // The escape byte is only trustworthy if the escape character itself is
        // escaped: "A\\|B" must not decode to the same bytes as "A|B".
        let with_slash = blake3_of(&["A\\|B"]);
        let plain = blake3_of(&["A|B"]);
        assert_ne!(with_slash, plain);
    }

    #[test]
    fn components_without_separators_hash_exactly_as_before() {
        // Compatibility pin: every UID already written was derived from a plain
        // join, so a component pair with no separator must land on the same
        // digest the old code produced. The literal below was read out of the
        // pre-change implementation; if escaping ever starts marking plain
        // components this fails and existing studies would be re-keyed.
        let digest = blake3_of(&["P1", "20240115", "120000", "A1", "CT"]);
        let mut b = [0u8; 8];
        b.copy_from_slice(&digest[..8]);
        let n = u64::from_be_bytes(b) & 0x7fff_ffff_ffff_ffff;
        assert_eq!(n, 4_097_947_227_879_186_042);
    }

    #[test]
    fn empty_components_stay_distinct_from_separators() {
        // Nothing else pins that an empty slot is not silently dropped: with an
        // unescaped join, ("", "A") and ("|A", "") both spell "|A".
        assert_ne!(blake3_of(&["", "A"]), blake3_of(&["|A", ""]));
        assert_ne!(blake3_of(&["A", ""]), blake3_of(&["A|"]));
    }
}
