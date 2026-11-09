// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Minimal DICOM utilities for cltranscode.
//! Tag parsing is handled by `dicom-object`. This module provides audit trail
//! generation, datetime formatting, and group 0000 cleanup.

#![deny(unsafe_code)]

/// Strip group 0000 elements (DIMSE command set leakage) from a DICOM dataset.
/// Must be called on raw dataset bytes (no preamble/meta header).
/// Returns cleaned data. Cheap no-op if no group 0000 present.
pub fn strip_group_0000(mut data: Vec<u8>) -> Vec<u8> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if group == 0x0000 {
            let vlen =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            if vlen == 0xFFFF_FFFF {
                pos += 8; // undefined length -- skip tag+len, can't drain
                continue;
            }
            let end = pos + 8 + vlen;
            if end <= data.len() {
                data.drain(pos..end);
                continue; // re-examine new element at same position
            }
        }
        // Skip this element (Implicit VR: tag 4 + len 4)
        let vlen = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        if vlen == 0xFFFF_FFFF {
            pos += 8;
        } else {
            pos += 8 + vlen;
        }
    }
    data
}

/// Wrap raw DICOM dataset in proper Part 10 format with meta-header.
///
/// Input: raw DICOM dataset bytes (no preamble, no meta-header).
/// Output: [128-byte preamble] "DICM" [meta-header with TransferSyntaxUID] [dataset]
///
/// The meta-header includes (0002,0010) TransferSyntaxUID, so the server parser
/// can unambiguously determine Explicit vs Implicit VR without guessing.
///
/// For Implicit VR LE (DIMSE default), TS = "1.2.840.10008.1.2".
/// Replace `ensure_dicom_preamble` - that function only added "DICM" (4 bytes)
/// with no preamble and no meta-header, violating DICOM Part 10 Section 7.1.
pub fn wrap_part10(data: Vec<u8>) -> Vec<u8> {
    // Already has proper Part 10 preamble at offset 128 - pass through.
    if data.len() >= 132 && &data[128..132] == b"DICM" {
        return data;
    }

    // Detect source VR: check if first element's "VR bytes" look like a
    // valid DICOM VR string. If yes -> Explicit VR; otherwise -> Implicit VR.
    let is_explicit = detect_vr_from_data(&data);
    let source_ts = if is_explicit {
        "1.2.840.10008.1.2.1" // Explicit VR Little Endian
    } else {
        "1.2.840.10008.1.2" // Implicit VR Little Endian (DIMSE default)
    };

    let ts_bytes = source_ts.as_bytes();
    let ts_padded_len = if ts_bytes.len() % 2 != 0 {
        ts_bytes.len() + 1
    } else {
        ts_bytes.len()
    };

    let mut out = Vec::with_capacity(data.len() + 256);

    // Preamble (128 bytes of zeros)
    out.extend_from_slice(&[0u8; 128]);

    // DICM magic at offset 128
    out.extend_from_slice(b"DICM");

    // Meta-header: (0002,0010) UI TransferSyntaxUID (always Explicit VR LE)
    out.extend_from_slice(&0x0002u16.to_le_bytes()); // group 0002
    out.extend_from_slice(&0x0010u16.to_le_bytes()); // element 0010
    out.extend_from_slice(b"UI"); // VR
    out.extend_from_slice(&(ts_padded_len as u16).to_le_bytes()); // length
    out.extend_from_slice(ts_bytes);
    if ts_bytes.len() % 2 != 0 {
        out.push(0x00); // pad to even length
    }

    // Dataset
    out.extend_from_slice(&data);

    out
}

/// Heuristic: does the raw dataset use Explicit or Implicit VR?
///
/// Looks at the first element's potential VR bytes (offset +4 from start).
/// In Explicit VR, these are printable ASCII VR codes ("CS", "UI", "PN"...).
/// In Implicit VR, these are the low bytes of the 4-byte length - usually
/// non-printable or invalid.
fn detect_vr_from_data(data: &[u8]) -> bool {
    const VALID_VRS: &[&str] = &[
        "AE", "AS", "AT", "CS", "DA", "DS", "DT", "FL", "FD", "IS", "LO", "LT", "OB", "OD", "OF",
        "OL", "OW", "PN", "SH", "SL", "SQ", "SS", "ST", "TM", "UC", "UI", "UL", "UN", "UR", "US",
        "UT",
    ];

    if data.len() >= 8 {
        let maybe_vr = &data[4..6];
        if let Ok(vr_str) = std::str::from_utf8(maybe_vr) {
            return VALID_VRS.contains(&vr_str);
        }
    }
    false
}

/// Extract a string value for a DICOM tag using dicom-object.
/// Returns trimmed string, or None if tag not found or parse fails.
pub fn extract_tag_string(data: &[u8], group: u16, element: u16) -> Option<String> {
    let obj = dicom_object::FileDicomObject::from_reader(data).ok()?;
    obj.get(dicom_object::Tag(group, element))?
        .value()
        .to_str()
        .ok()
        .map(|cow| cow.into_owned())
}

/// Current datetime in DICOM DT format: YYYYMMDDHHMMSS (UTC).
pub fn now_dicom_datetime() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let (y, m, d, hh, mm, ss) = unix_to_ymdhms(secs);
    format!("{:04}{:02}{:02}{:02}{:02}{:02}", y, m, d, hh, mm, ss)
}

// -- Private helpers for append_audit_trail -----------------------------

/// Find the byte offset of a DICOM tag in raw bytes (private - only for audit trail).
fn find_tag_offset(data: &[u8], group: u16, element: u16) -> Option<usize> {
    let tag_bytes = [group.to_le_bytes(), element.to_le_bytes()].concat();
    let start = if data.len() > 132 && &data[128..132] == b"DICM" {
        132
    } else {
        0
    };
    let mut pos = start;
    while pos + 4 <= data.len() {
        if &data[pos..pos + 4] == tag_bytes.as_slice() {
            return Some(pos);
        }
        if pos + 6 > data.len() {
            break;
        }
        let vr = &data[pos + 4..pos + 6];
        let is_long_vr = matches!(
            std::str::from_utf8(vr).unwrap_or(""),
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        let (header_size, val_len) = if is_long_vr {
            if pos + 12 > data.len() {
                break;
            }
            let len =
                u32::from_le_bytes([data[pos + 8], data[pos + 9], data[pos + 10], data[pos + 11]])
                    as usize;
            (12, len)
        } else {
            if pos + 8 > data.len() {
                break;
            }
            let len = u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as usize;
            (8, len)
        };
        if val_len == 0xFFFF_FFFF || val_len == 0xFFFF {
            // Undefined length - walk Items until Sequence Delimiter (FFFE,E0DD)
            let val_start = pos + header_size;
            if let Some(end) = walk_undefined_items(data, val_start) {
                pos = end;
                continue;
            } else {
                break;
            }
        }
        pos += header_size + val_len;
    }
    None
}

/// Walk past undefined-length content (SQ or encapsulated PixelData).
/// Starts at `cursor` (first byte after element header), returns the offset
/// AFTER the Sequence Delimiter Item (FFFE,E0DD), or None if truncated.
fn walk_undefined_items(data: &[u8], mut cursor: usize) -> Option<usize> {
    let delim = [0xFE, 0xFF, 0xDD, 0xE0];
    loop {
        if cursor + 8 > data.len() {
            return None;
        }
        let tag = &data[cursor..cursor + 4];
        if tag == delim {
            // Sequence Delimiter: tag (4) + zero length (4) = 8 bytes
            return Some(cursor + 8);
        }
        // Item tag (FFFE,E000): tag (4) + length (4)
        let item_len = u32::from_le_bytes([
            data[cursor + 4],
            data[cursor + 5],
            data[cursor + 6],
            data[cursor + 7],
        ]) as usize;
        cursor += 8; // skip Item tag + length
        if item_len == 0xFFFF_FFFF {
            // Nested undefined length - recurse
            cursor = walk_undefined_items(data, cursor)?;
            continue;
        }
        cursor += item_len;
        // No padding for Items within encapsulated data
    }
}

fn find_seq_delimiter(data: &[u8], seq_start: usize) -> Option<usize> {
    let search_from = seq_start + 12; // skip SQ tag + VR + reserved + undefined length
    if search_from >= data.len() {
        return None;
    }
    // Walk Items to find the SeqDelim - safer than raw byte search
    walk_undefined_items(data, search_from).map(|end| end - 8)
}

fn unix_to_ymdhms(ts: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (ts / 86400) as i64;
    let rem = ts % 86400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    let (y, m, d) = days_to_ymd(days);
    (y, m, d, hh as u32, mm as u32, ss as u32)
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    days += 719468; // shift to days since 0000-03-01
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

/// Build (0400,0561) Original Attributes Sequence audit trail as raw DICOM bytes.
///
/// Appends one Item: ModifyingSystem="CLTRANSCODE", Reason, DateTime.
/// If the sequence already exists, the item is appended (never overwritten).
/// Otherwise, a new SQ is created and inserted before PixelData (or at end).
pub fn append_audit_trail(data: &[u8], reason: &str, system: &str) -> Vec<u8> {
    let now = now_dicom_datetime();

    log::debug!(
        "audit: appending to (0400,0561), system=CLTRANSCODE, reason={}",
        reason,
    );

    let item = build_audit_item(reason, &now, system);

    // If (0400,0561) already exists, append new item.
    if let Some(seq_start) = find_tag_offset(data, 0x0400, 0x0561) {
        // Case 1: undefined-length SQ (legacy) - insert before SeqDelim
        if let Some(delim) = find_seq_delimiter(data, seq_start) {
            log::debug!(
                "audit: appending to undefined-length SQ at {}, delim at {}",
                seq_start,
                delim
            );
            let mut out = Vec::with_capacity(data.len() + item.len());
            out.extend_from_slice(&data[..delim]);
            out.extend_from_slice(&item);
            out.extend_from_slice(&data[delim..]);
            return out;
        }
        // Case 2: defined-length SQ - insert at end, update length
        if seq_start + 12 <= data.len() {
            let old_len = u32::from_le_bytes([
                data[seq_start + 8],
                data[seq_start + 9],
                data[seq_start + 10],
                data[seq_start + 11],
            ]) as usize;
            // old_len should be < 0xFFFFFFFF (otherwise it's undefined)
            if old_len < 0xFFFF_FFFF && seq_start + 12 + old_len <= data.len() {
                let sq_content_end = seq_start + 12 + old_len;
                let new_len = (old_len + item.len()) as u32;
                log::debug!(
                    "audit: appending to defined-length SQ at {}, old_len={}, new_len={}",
                    seq_start,
                    old_len,
                    new_len
                );
                let mut out = Vec::with_capacity(data.len() + item.len());
                out.extend_from_slice(&data[..sq_content_end]);
                out.extend_from_slice(&item);
                out.extend_from_slice(&data[sq_content_end..]);
                // Patch SQ length in output (offset = seq_start + 8)
                out[seq_start + 8..seq_start + 12].copy_from_slice(&new_len.to_le_bytes());
                return out;
            }
        }
    }

    // No existing sequence - create a new SQ (defined length)
    log::debug!("audit: creating new (0400,0561) sequence");
    let seq = build_audit_sequence(&item);
    let mut out = Vec::with_capacity(data.len() + seq.len());
    if let Some(px) = find_tag_offset(data, 0x7FE0, 0x0010) {
        out.extend_from_slice(&data[..px]);
        out.extend_from_slice(&seq);
        out.extend_from_slice(&data[px..]);
    } else {
        out.extend_from_slice(data);
        out.extend_from_slice(&seq);
    }
    out
}

fn build_audit_item(reason: &str, datetime: &str, system: &str) -> Vec<u8> {
    let mut item = Vec::new();
    // Item tag (FFFE,E000)
    item.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    let len_pos = item.len();
    item.extend_from_slice(&0u32.to_le_bytes()); // placeholder length

    // -- DICOM PS3.3 Table C.12.1-1: Original Attributes Macro --

    // (0400,0562) AttributeModificationDateTime - DT (Type 1)
    let dt = datetime.as_bytes();
    write_dicom_element(&mut item, 0x0400, 0x0562, b"DT", dt);

    // (0400,0563) ModifyingSystem - LO (Type 1)
    let sys = system.as_bytes();
    write_dicom_element(&mut item, 0x0400, 0x0563, b"LO", sys);

    // (0400,0564) SourceOfPreviousValues - LO (Type 2, may be empty)
    write_dicom_element(&mut item, 0x0400, 0x0564, b"LO", b"");

    // (0400,0565) ReasonForTheAttributeModification - CS (Type 1)
    let reason_bytes = reason.as_bytes();
    write_dicom_element(&mut item, 0x0400, 0x0565, b"CS", reason_bytes);

    // Patch item length
    let item_len = (item.len() - len_pos - 4) as u32;
    item[len_pos..len_pos + 4].copy_from_slice(&item_len.to_le_bytes());
    item
}

fn build_audit_sequence(item: &[u8]) -> Vec<u8> {
    let mut seq = Vec::new();
    // (0400,0561) SQ, defined length - avoids undefined-length parser bugs
    let item_len = item.len() as u32;
    seq.extend_from_slice(&0x0400u16.to_le_bytes());
    seq.extend_from_slice(&0x0561u16.to_le_bytes());
    seq.extend_from_slice(b"SQ");
    seq.extend_from_slice(&[0x00, 0x00]);
    seq.extend_from_slice(&item_len.to_le_bytes()); // defined length = item size
    seq.extend_from_slice(item);
    // No Sequence Delimiter for defined-length SQ
    seq
}

fn write_dicom_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);

    // DICOM PS3.5 Section 6.2: all values must be even-length.
    // Pad with SPACE (0x20) for character VRs, NUL (0x00) otherwise.
    let pad = if !value.len().is_multiple_of(2) { 1 } else { 0 };
    let padded_len = value.len() + pad;

    let is_long = matches!(
        std::str::from_utf8(vr).unwrap_or(""),
        "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
    );
    if is_long {
        buf.extend_from_slice(&[0x00, 0x00]); // reserved
        buf.extend_from_slice(&(padded_len as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(padded_len as u16).to_le_bytes());
    }
    buf.extend_from_slice(value);
    if pad > 0 {
        buf.push(if vr == b"CS" { b' ' } else { 0x00 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_audit_trail_adds_cltranscode() {
        let dcm = build_test_dicom(true);
        let result = append_audit_trail(&dcm, "CORRECT", "CLTRANSCODE");
        assert!(result.len() > dcm.len());
        let s = String::from_utf8_lossy(&result);
        assert!(s.contains("CLTRANSCODE"));
    }

    #[test]
    fn now_dicom_datetime_is_14_chars() {
        let dt = now_dicom_datetime();
        assert_eq!(dt.len(), 14, "expected YYYYMMDDHHMMSS, got '{}'", dt);
    }

    fn build_test_dicom(with_pixeldata: bool) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 128]);
        data.extend_from_slice(b"DICM");

        // (0002,0001) OB - long VR
        write_dicom_element(&mut data, 0x0002, 0x0001, b"OB", &[0x00, 0x01]);
        write_dicom_element(&mut data, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1\0");
        write_dicom_element(
            &mut data,
            0x0008,
            0x0016,
            b"UI",
            b"1.2.840.10008.5.1.4.1.1.2\0",
        );

        if with_pixeldata {
            write_dicom_element(&mut data, 0x7FE0, 0x0010, b"OW", &[0xAA; 4]);
        }
        data
    }
}
