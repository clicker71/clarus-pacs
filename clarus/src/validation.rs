// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM audit helpers (FR-001b): binary builders for the
//! (0400,0561) OriginalAttributesSequence written before the CAS store.
//!
//! PatientID / PatientName coercion (FR-001) lives in clarus-core:
//! `clarus_core::stow::generate_patient_id` / `generate_patient_name`
//! (deterministic BLAKE3 over StudyInstanceUID, call sites in
//! `stow_pure.rs` validate_and_coerce). No time-based generators here -
//! the legacy pair was dead code, excised 24.08 (flaky test
//! `test_generate_patient_id_unique` died with it).
//!
//! No external crates. Standard library only. KISS.

#![forbid(unsafe_code)]

use std::time::{SystemTime, UNIX_EPOCH};

// -- FR-001b: Binary DICOM (0400,0561) OriginalAttributesSequence builder --

/// Map the 8 auditable DICOM tags to their VR (Value Representation).
/// DICOM PS3.6 Section 6.2: VR determines the encoding of the value.
fn vr_for_tag(group: u16, elem: u16) -> &'static str {
    match (group, elem) {
        (0x0010, 0x0010) => "PN", // PatientName
        (0x0010, 0x0020) => "LO", // PatientID
        (0x0010, 0x0030) => "DA", // PatientBirthDate
        (0x0010, 0x0040) => "CS", // PatientSex
        (0x0008, 0x0020) => "DA", // StudyDate
        (0x0008, 0x0050) => "SH", // AccessionNumber
        (0x0008, 0x0090) => "PN", // ReferringPhysician
        (0x0008, 0x0060) => "CS", // Modality
        _ => "LO",                // fallback: Long String
    }
}

/// Write a raw DICOM tag (group, element) as 4 LE bytes - no VR, no length.
/// Used as a building block for lower-level encoding.
fn write_tag(buf: &mut Vec<u8>, group: u16, elem: u16) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&elem.to_le_bytes());
}

/// Write a DICOM data element (tag + VR + length + value) into `buf`.
/// Uses Explicit VR Little Endian encoding (DICOM PS3.5 Section 7.1).
///
/// Value lengths are even: PS3.5 7.1.1 requires every value field to have an
/// even length, and the padding byte is part of the value, not slack. The
/// length field written is therefore the PADDED length - writing the raw
/// length and appending bytes afterwards would shift every following element
/// and produce a blob no reader can walk. CAS is content-addressed, so such a
/// blob cannot be repaired in place either.
///
/// Text VRs pad with SPACE (0x20) and binary VRs with NUL (0x00), per the
/// padding rules of PS3.5 7.1.1 for those value types. UI pads with NUL.
///
/// Special cases:
///   - Item (FFFE,E000), ItemDelimitationItem (FFFE,E00D),
///     SequenceDelimitationItem (FFFE,E0DD): NO VR, 4-byte length. These
///     already carry a byte count the caller computed, so they are excluded
///     from padding - their payload is a sequence of elements, not a value.
///   - SQ, UN, OB, OD, OF, OL, OW, UC, UR, UT: 2 reserved + 4-byte length.
///   - All others: 2-byte length.
fn write_element(buf: &mut Vec<u8>, group: u16, elem: u16, vr: &str, value: &[u8]) {
    write_tag(buf, group, elem);

    // DICOM Item tags have NO VR - just 4-byte length (PS3.5 Section 7.5)
    let is_item = group == 0xFFFE && (elem == 0xE000 || elem == 0xE00D || elem == 0xE0DD);

    if is_item {
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        buf.extend_from_slice(value);
        return;
    }

    // PS3.5 7.1.1: pad the value to even length before the length field is
    // written, so tag/VR/length/value stay a stride a reader can walk.
    let pad = if value.len().is_multiple_of(2) {
        None
    } else if matches!(vr, "UI" | "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "UN") {
        Some(0x00u8)
    } else {
        Some(0x20u8)
    };
    let padded_len = value.len() + usize::from(pad.is_some());

    buf.extend_from_slice(vr.as_bytes());
    // OB/OD/OF/OL/OW/SQ/UC/UR/UT/UN: 2 reserved + 4-byte length
    if matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UR" | "UT" | "UN"
    ) {
        buf.extend_from_slice(&[0u8; 2]); // reserved
        buf.extend_from_slice(&(padded_len as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(padded_len as u16).to_le_bytes());
    }
    buf.extend_from_slice(value);
    if let Some(byte) = pad {
        buf.push(byte);
    }
}

/// One (0400,0561) OriginalAttributesSequence item (2808 update-in-place).
#[derive(Debug, Clone)]
pub struct OriginalAttributeItem {
    /// DICOM group of the modified tag, e.g. 0x0010.
    pub group: u16,
    /// DICOM element of the modified tag, e.g. 0x0010.
    pub element: u16,
    /// The OLD value (the archived value before this modification). Empty
    /// string for a COERCE item (the field was empty and the server
    /// generated a placeholder).
    pub old_value: String,
    /// PS3.3 C.12.1.1 enumerated value: "CORRECT" (operator re-upload) or
    /// "COERCE" (server-generated placeholder, e.g. AUTOPAT_/Anonymous^).
    pub reason: &'static str,
}

/// Build DICOM (0400,0561) OriginalAttributesSequence binary.
///
/// Structure (DICOM PS3.3 Table C.12.1.1.9-1):
///   (0400,0561) SQ OriginalAttributesSequence
///     Item:
///       (0400,0562) DT AttributeModificationDateTime
///       (0400,0563) LO ModifyingSystem = "CLARUS_SERVER"
///       (0400,0564) LO SourceOfPreviousValues = "" (Type 2, may be empty)
///       (0400,0565) LO ReasonForModification = "CORRECT" | "COERCE"
///       (0400,0566) LO ModifyingOperator = modifier (effective identity)
///       (0400,0550) SQ ModifiedAttributesSequence
///         Item: (tag) VR [old_value]
///
/// One Item per modified tag. Appended to the DICOM file before CAS write.
///
/// The (0400,0562) DT uses the REAL civil UTC date (audit_log's
/// `civil_from_days`, the same helper the audit journal uses) plus real
/// HHMMSS - the old pseudo-date ("1970 + days/365", month from days%365/30)
/// produced wrong month/day values.
pub fn build_original_attributes_sequence(
    items: &[OriginalAttributeItem],
    modifier: &str,
    modifying_system: &str,
) -> Vec<u8> {
    let dt = {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        // DICOM DT format: YYYYMMDDHHMMSS (UTC).
        let secs = now.as_secs();
        let time_of_day = secs % 86_400;
        let hh = time_of_day / 3600;
        let mm = (time_of_day % 3600) / 60;
        let ss = time_of_day % 60;
        // The civil date comes from audit_log's civil_from_days (via
        // utc_ymd) - the correct month/day, not the old pseudo-date.
        let ymd = crate::audit_log::utc_ymd();
        format!("{ymd}{hh:02}{mm:02}{ss:02}")
    };

    let mut items_out = Vec::new();
    for item in items {
        let vr = vr_for_tag(item.group, item.element);

        // Inner item: the modified tag with its old value
        let mut inner_item = Vec::new();
        write_element(
            &mut inner_item,
            item.group,
            item.element,
            vr,
            item.old_value.as_bytes(),
        );

        // Build ModifiedAttributesSequence (0400,0550) containing the inner item
        let mut inner_seq = Vec::new();
        write_element(&mut inner_seq, 0xFFFE, 0xE000, "SQ", &inner_item); // Item tag
        let mut mod_attrs_seq = Vec::new();
        write_element(&mut mod_attrs_seq, 0x0400, 0x0550, "SQ", &inner_seq);

        // Build one audit item: who/when/why + ModifiedAttributesSequence
        let mut audit_item = Vec::new();
        write_element(&mut audit_item, 0x0400, 0x0562, "DT", dt.as_bytes()); // DateTime
        write_element(
            &mut audit_item,
            0x0400,
            0x0563,
            "LO",
            modifying_system.as_bytes(),
        ); // ModifyingSystem
        write_element(&mut audit_item, 0x0400, 0x0564, "LO", b""); // SourceOfPreviousValues (empty, Type 2)
        write_element(
            &mut audit_item,
            0x0400,
            0x0565,
            "LO",
            item.reason.as_bytes(),
        ); // Reason
        write_element(&mut audit_item, 0x0400, 0x0566, "LO", modifier.as_bytes()); // ModifyingOperator
        audit_item.extend_from_slice(&mod_attrs_seq); // ModifiedAttributesSequence

        // Wrap in Item tag (FFFE,E000)
        let mut item_wrapped = Vec::new();
        write_element(&mut item_wrapped, 0xFFFE, 0xE000, "SQ", &audit_item);
        items_out.push(item_wrapped);
    }

    // Wrap all items in (0400,0561) SQ OriginalAttributesSequence
    let mut seq = Vec::new();
    for item in &items_out {
        seq.extend_from_slice(item);
    }
    let mut result = Vec::new();
    write_element(&mut result, 0x0400, 0x0561, "SQ", &seq);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk an Explicit VR LE element stream and return `(tag, vr, length)`.
    ///
    /// A reader that trusts the number in the length field is exactly what
    /// the production build must satisfy, so the test parses the way a real
    /// consumer does rather than searching for byte patterns.
    fn walk(buf: &[u8]) -> Vec<((u16, u16), [u8; 2], usize)> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= buf.len() {
            let group = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
            let elem = u16::from_le_bytes([buf[pos + 2], buf[pos + 3]]);
            pos += 4;
            if group == 0xFFFE {
                let len = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
                    as usize;
                pos += 4;
                out.push(((group, elem), *b"--", len));
                pos += len;
                continue;
            }
            let vr = [buf[pos], buf[pos + 1]];
            pos += 2;
            let extended = matches!(
                &vr,
                b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UR" | b"UT" | b"UN"
            );
            let len = if extended {
                pos += 2; // reserved
                let l = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
                    as usize;
                pos += 4;
                l
            } else {
                let l = u16::from_le_bytes([buf[pos], buf[pos + 1]]) as usize;
                pos += 2;
                l
            };
            out.push(((group, elem), vr, len));
            pos += len;
        }
        assert_eq!(
            pos,
            buf.len(),
            "the element stream must end exactly at the buffer end"
        );
        out
    }

    /// PS3.5 7.1.1: an odd-length value must be padded to even, and the length
    /// field must be the padded length. The real callers pass values that are
    /// odd by construction - "CLARUS_SERVER" is 13 bytes, "CORRECT" is 7 -
    /// so a missing pad is not a corner case but the normal path.
    #[test]
    fn an_odd_length_value_is_padded_and_the_length_is_the_padded_one() {
        let items = [OriginalAttributeItem {
            group: 0x0010,
            element: 0x0010,
            old_value: "ODD^NAME".to_string(), // 8: even
            reason: "CORRECT",                 // 7: odd
        }];
        let buf = build_original_attributes_sequence(&items, "OP", "CLARUS_SERVER");
        // The sequence value is the item stream; walk into it, because the
        // outer element's length only tells us the item block's size.
        let outer = walk(&buf);
        assert_eq!(outer.len(), 1, "one OriginalAttributesSequence element");
        let body = &buf[buf.len() - outer[0].2..];
        for (tag, vr, len) in walk(body) {
            if vr == *b"--" {
                continue;
            }
            assert!(
                len.is_multiple_of(2),
                "value length must be even: tag={tag:?} vr={} len={len}",
                String::from_utf8_lossy(&vr)
            );
        }

        // Every value field in the stream must be even-length.
        for (tag, vr, len) in walk(&buf) {
            if vr == *b"--" {
                continue;
            }
            assert!(
                len.is_multiple_of(2),
                "value length must be even: tag={tag:?} vr={} len={len}",
                String::from_utf8_lossy(&vr)
            );
        }

        // The padded bytes are part of the value: 7 bytes of "CORRECT" plus
        // one SPACE, and the length says 8.
        let reason_at = buf
            .windows(15)
            .position(|w| &w[8..15] == b"CORRECT")
            .expect("ReasonForModification value present");
        assert_eq!(&buf[reason_at + 4..reason_at + 6], b"LO");
        assert_eq!(
            u16::from_le_bytes([buf[reason_at + 6], buf[reason_at + 7]]),
            8,
            "the length field must be the padded length"
        );
        assert_eq!(
            buf[reason_at + 15],
            0x20,
            "text VRs pad the value with SPACE"
        );

        // "CLARUS_SERVER" is 13 -> padded to 14.
        let system_at = buf
            .windows(21)
            .position(|w| &w[8..21] == b"CLARUS_SERVER")
            .expect("ModifyingSystem value present");
        assert_eq!(&buf[system_at + 4..system_at + 6], b"LO");
        assert_eq!(
            u16::from_le_bytes([buf[system_at + 6], buf[system_at + 7]]),
            14,
            "the length field must be the padded length"
        );
        // Value starts at +8 and is 13 bytes, so the pad is the byte at +21.
        assert_eq!(buf[system_at + 21], 0x20, "text VRs pad with SPACE");
    }

    /// An even-length value gains no padding: the rule must not grow values
    /// that already conform (a stray pad byte would be part of the value, and
    /// a PN with a trailing space is still a different PN).
    #[test]
    fn an_even_length_value_is_left_alone() {
        let mut buf = Vec::new();
        write_element(&mut buf, 0x0010, 0x0020, "LO", b"PAT-01");
        assert_eq!(&buf[4..6], b"LO");
        assert_eq!(u16::from_le_bytes([buf[6], buf[7]]), 6);
        assert_eq!(&buf[8..], b"PAT-01");
    }

    /// A binary VR pads with NUL, not SPACE (PS3.5 7.1.1): a space in an OB
    /// value is data, not padding.
    #[test]
    fn a_binary_value_pads_with_nul() {
        let mut buf = Vec::new();
        write_element(&mut buf, 0x7FE0, 0x0010, "OB", &[1, 2, 3]);
        let len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        assert_eq!(len, 4);
        assert_eq!(&buf[12..], &[1, 2, 3, 0]);
    }

    /// An Item tag carries a payload length the caller computed, and its body
    /// is a sequence of elements rather than one value - padding it would
    /// corrupt the item.
    #[test]
    fn an_item_is_never_padded() {
        let mut buf = Vec::new();
        write_element(&mut buf, 0xFFFE, 0xE000, "SQ", &[1, 2, 3]);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 3);
        assert_eq!(&buf[8..], &[1, 2, 3]);
    }
}
