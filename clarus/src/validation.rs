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

/// Build the DICOM header elements that FR-001 generated for an instance.
///
/// A coerced store fills an empty PatientID/PatientName in the MANIFEST, but
/// the stored blob kept no such element - so QIDO found the instance and
/// WADO-RS `/metadata` (which reads the blob) did not. These are the SAME
/// values written as ordinary Explicit VR LE elements, in ascending tag order
/// so the dataset stays walkable:
///
/// ```text
/// (0010,0010) PN PatientName
/// (0010,0020) LO PatientID
/// ```
///
/// Callers must place the result BEFORE (7FE0,0010) PixelData: a header
/// attribute appended to the tail of a file is never read, because the
/// metadata walk stops at the pixel data.
///
/// Empty values are skipped - a coercion that generated nothing writes nothing,
/// and an empty PN/LO element would claim a value where there is none.
#[must_use]
pub fn build_coerced_patient_elements(patient_name: &str, patient_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(patient_name.len() + patient_id.len() + 16);
    if let Some(pn) = build_coerced_patient_name_element(patient_name) {
        out.extend_from_slice(&pn);
    }
    if let Some(pid) = build_coerced_patient_id_element(patient_id) {
        out.extend_from_slice(&pid);
    }
    out
}

/// The single generated (0010,0010) PatientName element, or `None` when there
/// is no value to write.
#[must_use]
pub fn build_coerced_patient_name_element(patient_name: &str) -> Option<Vec<u8>> {
    if patient_name.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(patient_name.len() + 8);
    write_element(&mut out, 0x0010, 0x0010, "PN", patient_name.as_bytes());
    Some(out)
}

/// The single generated (0010,0020) PatientID element, or `None` when there
/// is no value to write.
#[must_use]
pub fn build_coerced_patient_id_element(patient_id: &str) -> Option<Vec<u8>> {
    if patient_id.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(patient_id.len() + 8);
    write_element(&mut out, 0x0010, 0x0020, "LO", patient_id.as_bytes());
    Some(out)
}

// -- Generated patient-element placement (PS3.5 7.1 ascending tag order) ----

/// One splice seam for a generated patient element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatientElementSplice {
    /// Byte offset of the seam (the existing element's TAG offset when the
    /// tag is present, else the offset of the first element with a larger tag
    /// or of PixelData).
    pub offset: usize,
    /// Byte length of the existing element to overwrite (0 = pure insert).
    pub replace_len: usize,
}

/// The two splice seams for the generated patient elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PatientElementSplices {
    /// (0010,0010) PatientName.
    pub patient_name: Option<PatientElementSplice>,
    /// (0010,0020) PatientID.
    pub patient_id: Option<PatientElementSplice>,
}

/// Where a `(0400,0561)` Original Attributes Sequence belongs in `src`.
///
/// PS3.5 7.1 requires the tags of a data set to be ordered by increasing
/// (group, element) and each to appear at most once. Appending the sequence at
/// end-of-file satisfies neither for an image: group 0400 would follow PixelData
/// (7FE0), and an object that already carried a 0561 would end up with two.
///
/// Returns `(offset, replace_len)`:
/// - `replace_len > 0` when the tag is present - the existing sequence is
///   REPLACED, which also merges an incoming object's own history;
/// - `replace_len == 0` when it is absent - the seam is the first element with
///   a larger tag, or the end of the (complete) dataset.
///
/// `None` when the walk could not reach a trustworthy seam: a partial read has
/// no defined end, and inserting there would land inside an element.
pub fn original_attributes_splice(src: &[u8]) -> Option<PatientElementSplice> {
    const TARGET: (u16, u16) = (0x0400, 0x0561);
    let data_start = if src.len() >= 132 && &src[128..132] == b"DICM" {
        132
    } else if src.len() >= 4 && &src[..4] == b"DICM" {
        4
    } else {
        return None;
    };

    let mut pos = data_start;
    let mut anchor: Option<usize> = None;
    let mut dataset_end: Option<usize> = None;
    let mut complete = true;

    while pos + 8 <= src.len() {
        let tag_offset = pos;
        let tag = (
            u16::from_le_bytes([src[pos], src[pos + 1]]),
            u16::from_le_bytes([src[pos + 2], src[pos + 3]]),
        );
        if tag.0 == 0xFFFE {
            dataset_end = Some(pos);
            break;
        }
        let vr = [src[pos + 4], src[pos + 5]];
        let extended = vr_is_extended(vr);
        let (value_len, value_start) = if extended {
            if pos + 12 > src.len() {
                complete = false;
                break;
            }
            (
                u32::from_le_bytes([src[pos + 8], src[pos + 9], src[pos + 10], src[pos + 11]])
                    as usize,
                pos + 12,
            )
        } else {
            (
                u16::from_le_bytes([src[pos + 6], src[pos + 7]]) as usize,
                pos + 8,
            )
        };
        if tag == (0x7FE0, 0x0010) {
            // PixelData's offset is where the sequence belongs (group 0400
            // sorts before 7FE0), so it is the default seam - but an object
            // written by an EARLIER build carries the sequence AFTER the
            // pixels. Skip the value and keep walking so such an object is
            // recognised as already holding one, rather than gaining a second
            // copy when it is corrected again.
            if anchor.is_none() {
                anchor = Some(tag_offset);
            }
            // An encapsulated PixelData is a fragment stream: its extent is
            // not `value_len`, so stop rather than mis-walk it.
            if value_len == 0xFFFF_FFFF {
                dataset_end = Some(tag_offset);
                complete = true;
                break;
            }
            let Some(value_end) = value_start.checked_add(value_len) else {
                complete = false;
                break;
            };
            if value_end > src.len() {
                complete = false;
                break;
            }
            dataset_end = Some(value_end + (value_len & 1));
            pos = value_end + (value_len & 1);
            continue;
        }
        if value_len == 0xFFFF_FFFF {
            match undefined_length_extent(src, value_start) {
                Some(end) => {
                    dataset_end = Some(end);
                    if tag == TARGET {
                        return Some(PatientElementSplice {
                            offset: tag_offset,
                            replace_len: end - tag_offset,
                        });
                    }
                    if tag > TARGET && anchor.is_none() {
                        anchor = Some(tag_offset);
                    }
                    pos = end;
                    continue;
                }
                None => {
                    complete = false;
                    break;
                }
            }
        }
        let value_end = match value_start.checked_add(value_len) {
            Some(v) => v,
            None => {
                complete = false;
                break;
            }
        };
        if value_end > src.len() {
            complete = false;
            break;
        }
        let elem_len = value_end + (value_len & 1) - tag_offset;
        if elem_len == 0 {
            complete = false;
            break;
        }
        dataset_end = Some(value_end + (value_len & 1));
        if tag == TARGET {
            return Some(PatientElementSplice {
                offset: tag_offset,
                replace_len: elem_len,
            });
        }
        if tag > TARGET && anchor.is_none() {
            anchor = Some(tag_offset);
        }
        pos = value_end + (value_len & 1);
    }

    if !complete {
        // A partial walk cannot place the sequence without landing inside the
        // element that was cut off.
        return anchor.map(|offset| PatientElementSplice {
            offset,
            replace_len: 0,
        });
    }
    // The ANCHOR wins over the dataset end: for an image the anchor is the
    // PixelData tag, and group 0400 belongs BEFORE it (PS3.5 7.1 ascending
    // order), not after the pixels.
    anchor.or(dataset_end).map(|offset| PatientElementSplice {
        offset,
        replace_len: 0,
    })
}

/// Whether a VR is one of the extended-length forms (PS3.5 7.1.2) that carry
/// a 32-bit length after two reserved bytes.
fn vr_is_extended(vr: [u8; 2]) -> bool {
    matches!(
        &vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    )
}

/// Byte offset AFTER the SequenceDelimitationItem that closes the
/// undefined-length item/sequence whose content starts at `start`.
///
/// Explicit VR Little Endian only: items and delimiters carry no VR (PS3.5
/// 7.5). Nesting is counted so an inner delimiter is not mistaken for the
/// enclosing one.
fn undefined_length_extent(src: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    let mut nesting: u32 = 0;
    while pos + 8 <= src.len() {
        let tag = (
            u16::from_le_bytes([src[pos], src[pos + 1]]),
            u16::from_le_bytes([src[pos + 2], src[pos + 3]]),
        );
        if tag == (0xFFFE, 0xE000) {
            let len = u32::from_le_bytes([src[pos + 4], src[pos + 5], src[pos + 6], src[pos + 7]]);
            pos += 8;
            if len == 0xFFFF_FFFF {
                nesting += 1;
            } else {
                pos = pos.saturating_add(len as usize).min(src.len());
            }
            continue;
        }
        if tag == (0xFFFE, 0xE00D) || tag == (0xFFFE, 0xE0DD) {
            pos += 8;
            if nesting == 0 {
                return Some(pos);
            }
            nesting = nesting.saturating_sub(1);
            continue;
        }
        let vr = [src[pos + 4], src[pos + 5]];
        let extended = vr_is_extended(vr);
        let (len, value_start) = if extended {
            if pos + 12 > src.len() {
                return None;
            }
            (
                u32::from_le_bytes([src[pos + 8], src[pos + 9], src[pos + 10], src[pos + 11]])
                    as usize,
                pos + 12,
            )
        } else {
            (
                u16::from_le_bytes([src[pos + 6], src[pos + 7]]) as usize,
                pos + 8,
            )
        };
        if len == 0xFFFF_FFFF {
            nesting += 1;
            pos = value_start;
            continue;
        }
        let value_end = value_start.saturating_add(len);
        if value_end > src.len() {
            return None;
        }
        pos = value_end + (len & 1);
    }
    None
}

/// Locate the splice seams for the generated patient elements in an Explicit
/// VR Little Endian dataset.
///
/// The stream is walked once, from the Part 10 preamble/DICM (or a bare DICM)
/// through the dataset, stopping at PixelData. Meta-group (0002,xxxx) elements
/// are Explicit VR LE by construction (PS3.10 7.1); the caller only runs this
/// for a transfer syntax whose dataset non-pixel elements are Explicit VR LE.
///
/// Per tag:
/// - present: `offset` is the tag's own byte offset and `replace_len` its full
///   element extent (tag + VR + length + value + pad), so the caller REPLACES
///   the value and the result carries exactly one occurrence;
/// - absent: `offset` is the insertion seam and `replace_len` is 0. The seam is
///   the first element whose tag is not less than the missing one (PS3.5 7.1
///   ascending tag order), so a missing PatientName inserts BEFORE an existing
///   PatientID instead of after it; when no such element exists the seam is the
///   end of the dataset, and only when the walk consumed it all.
///
/// The dataset end is the end of the walk: the last element's extent, or the
/// SequenceDelimitationItem that closed an undefined-length one. PixelData is
/// NOT required - an SR, a presentation state or a structured report has none,
/// and its header attributes are just as readable, so a coercion that only the
/// manifest carried left those datasets unchanged (the ID the server reports
/// and the ID in the file disagreed).
pub fn patient_element_splices(src: &[u8]) -> PatientElementSplices {
    let data_start = if src.len() >= 132 && &src[128..132] == b"DICM" {
        132
    } else if src.len() >= 4 && &src[..4] == b"DICM" {
        4
    } else {
        return PatientElementSplices::default();
    };

    let mut out = PatientElementSplices::default();
    let mut pos = data_start;
    let mut pn_anchor: Option<usize> = None;
    let mut pid_anchor: Option<usize> = None;
    // The end of what the walk actually consumed: the seam for an insert when
    // no larger tag follows. A walk that stopped because it ran out of bytes is
    // a PARTIAL read, so appending there would land inside the dataset.
    let mut dataset_end: Option<usize> = None;
    let mut complete = true;

    while pos + 8 <= src.len() {
        let tag_offset = pos;
        let tag = (
            u16::from_le_bytes([src[pos], src[pos + 1]]),
            u16::from_le_bytes([src[pos + 2], src[pos + 3]]),
        );

        if tag.0 == 0xFFFE {
            // Item / delimiter: the header dataset ends at the delimitation
            // item itself, which a truncated read may have cut short.
            dataset_end = Some(pos);
            break;
        }

        let vr = [src[pos + 4], src[pos + 5]];
        let extended = vr_is_extended(vr);
        let (value_len, value_start) = if extended {
            if pos + 12 > src.len() {
                complete = false;
                break;
            }
            (
                u32::from_le_bytes([src[pos + 8], src[pos + 9], src[pos + 10], src[pos + 11]])
                    as usize,
                pos + 12,
            )
        } else {
            (
                u16::from_le_bytes([src[pos + 6], src[pos + 7]]) as usize,
                pos + 8,
            )
        };

        if tag == (0x7FE0, 0x0010) {
            // PixelData is the LAST element: everything after it is pixels,
            // not part of the header walk. Its offset is a valid seam too.
            dataset_end = Some(tag_offset);
            break;
        }

        if value_len == 0xFFFF_FFFF {
            // Undefined-length sequence: skip its whole extent (nested items
            // counted) so the walk lands after the SequenceDelimitationItem.
            //
            // Its TAG OFFSET is still a valid insertion anchor, and it has to
            // be recorded BEFORE the `continue` below. The anchor is "the
            // first element whose tag is not less than the missing one"; an
            // undefined-length (0020,xxxx) sequence is such an element, and
            // skipping it meant a missing PatientName was inserted after it -
            // or at the end of the dataset - which breaks the ascending order
            // the reader depends on (PS3.5 7.1).
            match undefined_length_extent(src, value_start) {
                Some(end) => {
                    if tag >= (0x0010, 0x0010) && pn_anchor.is_none() {
                        pn_anchor = Some(tag_offset);
                    }
                    if tag >= (0x0010, 0x0020) && pid_anchor.is_none() {
                        pid_anchor = Some(tag_offset);
                    }
                    dataset_end = Some(end);
                    pos = end;
                    continue;
                }
                None => {
                    // The read stopped inside the sequence: the end of what
                    // was consumed is not the end of the dataset.
                    complete = false;
                    break;
                }
            }
        }

        let value_end = match value_start.checked_add(value_len) {
            Some(v) => v,
            None => {
                complete = false;
                break;
            }
        };
        if value_end > src.len() {
            // Truncated read: the element's value is not all here.
            complete = false;
            break;
        }
        // Full element extent, including the pad byte of an odd-length value.
        let elem_len = value_end + (value_len & 1) - tag_offset;
        if elem_len == 0 {
            // PS3.5 7.1 allows a zero-length value, and then `pos` would not
            // move: the walk has to stop rather than spin.
            complete = false;
            break;
        }
        dataset_end = Some(value_end + (value_len & 1));

        // The anchor is recorded for EVERY element at or after the missing
        // tag, including the sibling patient tag: the PatientID element is
        // itself the correct seam for an absent PatientName, and its own
        // replacement seam stays in `out.patient_id`. Recording it only in the
        // `else` branch put the inserted PatientName after PatientID, i.e.
        // out of the PS3.5 7.1 ascending order the reader relies on.
        if tag >= (0x0010, 0x0010) && pn_anchor.is_none() {
            pn_anchor = Some(tag_offset);
        }
        if tag >= (0x0010, 0x0020) && pid_anchor.is_none() {
            pid_anchor = Some(tag_offset);
        }

        if tag == (0x0010, 0x0010) {
            out.patient_name = Some(PatientElementSplice {
                offset: tag_offset,
                replace_len: elem_len,
            });
        } else if tag == (0x0010, 0x0020) {
            out.patient_id = Some(PatientElementSplice {
                offset: tag_offset,
                replace_len: elem_len,
            });
        }
        pos = value_end + (value_len & 1);
    }

    // The walk ended without consuming the last element: the buffer is a
    // PARTIAL dataset (a bounded header read), so only the REPLACEMENT seams
    // found so far can be trusted. Inserting at `dataset_end` would splice
    // inside the element that was cut off.
    //
    // The loop can also fall out with `complete` still true because fewer than
    // 8 bytes remained. Those few bytes are the START of another element the
    // probe did not have room to read: a buffer that ends exactly between
    // elements and one that ends four bytes into the next tag look identical
    // from inside the loop, and only the caller knows whether the file is
    // really over. Treating that tail as the end of the dataset put the
    // inserted element in front of an element that already exists on disk.
    let trailed = pos < src.len() && complete;
    let insert_seam = if complete && !trailed {
        dataset_end
    } else {
        None
    };

    if out.patient_name.is_none() {
        out.patient_name = pn_anchor
            .or(insert_seam)
            .map(|offset| PatientElementSplice {
                offset,
                replace_len: 0,
            });
    }
    if out.patient_id.is_none() {
        out.patient_id = pid_anchor
            .or(insert_seam)
            .map(|offset| PatientElementSplice {
                offset,
                replace_len: 0,
            });
    }
    out
}

/// The (0400,0562) Attribute Modification DateTime, from ONE clock read.
///
/// Both halves used to come from separate calls: the time of day from
/// `SystemTime::now()` and the civil date from `audit_log::utc_ymd()`, which
/// reads the clock again. At UTC midnight the two reads land on different days,
/// so the audit item could claim yesterday's date with this morning's time -
/// a timestamp that never existed. One `secs` value drives both.
fn modification_datetime() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    dt_from_secs(secs)
}

/// The DT value for one UTC instant, as `YYYYMMDDHHMMSS`.
///
/// Both halves derive from the SAME `secs`: the civil date from
/// `secs / 86_400` and the time from `secs % 86_400`. Taking them from two
/// clock reads is what allowed a midnight-straddling item to name a date and a
/// time that never coexisted.
fn dt_from_secs(secs: u64) -> String {
    let time_of_day = secs % 86_400;
    let hh = time_of_day / 3600;
    let mm = (time_of_day % 3600) / 60;
    let ss = time_of_day % 60;
    let (y, mo, d) = crate::audit_log::civil_from_days((secs / 86_400) as i64);
    format!("{y:04}{mo:02}{d:02}{hh:02}{mm:02}{ss:02}")
}

/// `YYYYMMDD` for a day count since the Unix epoch (test reference).
#[cfg(test)]
fn ymd_from_days(days: i64) -> String {
    let (y, mo, d) = crate::audit_log::civil_from_days(days);
    format!("{y:04}{mo:02}{d:02}")
}

/// The bytes to write for an old attribute value.
///
/// The stored value is a Rust `String` (decoded text). Writing its UTF-8 bytes
/// straight into the dataset is wrong whenever the destination declares a
/// non-UTF-8 Specific Character Set: `José` in a dataset declaring `ISO_IR 100`
/// (latin-1) would carry the UTF-8 two-byte form where the reader expects one
/// byte. The round trip then yields mojibake, and the audit record - whose only
/// purpose is to preserve what the value WAS - would be the thing that corrupted
/// it.
///
/// This crate has no charset encoder, and inventing one here would be a
/// half-measure that silently picks a repertoire. So the rule is the conservative
/// one PS3.5's Default Character Repertoire already implies: a value must be
/// representable as ASCII to be written verbatim. Anything else is replaced by
/// the value-length-preserving placeholder, and the event is still audited
/// through the access journal, which carries the change without re-encoding it
/// into the instance.
fn encode_old_value(old_value: &str, group: u16, element: u16) -> std::borrow::Cow<'_, str> {
    if old_value.is_ascii() {
        return std::borrow::Cow::Borrowed(old_value);
    }
    // Not ASCII: the destination repertoire is unknown to this function, so the
    // bytes cannot be guaranteed to round-trip. Recording a placeholder is the
    // only answer that cannot write a value that decodes to something else.
    crate::clog_debug!(
        "OriginalAttributesSequence: non-ASCII old value for ({group:04X},{element:04X}) \
         recorded as a placeholder (no charset encoder on this path)"
    );
    std::borrow::Cow::Borrowed("***")
}

/// Build DICOM (0400,0561) OriginalAttributesSequence binary.
///
/// Structure (DICOM PS3.3 Table C.12.1.1.9-1; VRs per PS3.6 group 0400):///   (0400,0561) SQ OriginalAttributesSequence
///     Item:
///       (0400,0550) SQ ModifiedAttributesSequence
///         Item: (tag) VR [old_value]
///       (0400,0562) DT AttributeModificationDateTime
///       (0400,0563) LO ModifyingSystem = "CLARUS_SERVER"
///       (0400,0564) LO SourceOfPreviousValues = "" (Type 2, may be empty)
///       (0400,0565) CS ReasonForTheAttributeModification = "CORRECT" | "COERCE"
///
/// One Item per modified tag. Appended to the DICOM file before CAS write.
///
/// There is deliberately NO (0400,0566). PS3.6 defines no such attribute, and
/// PS3.3 C.12.1.1.9.1 lists only the four Who attributes above; group 0400 is
/// even, so a private element cannot live there either (PS3.5 7.8.1). The
/// operator's identity is carried by the access journal, which is where a
/// reader looks for "who requested this", not in a fabricated data element.
pub fn build_original_attributes_sequence(
    items: &[OriginalAttributeItem],
    modifier: &str,
    modifying_system: &str,
) -> Vec<u8> {
    // The operator identity has no conformant home in this sequence (see the
    // doc comment: there is no (0400,0566)). The parameter stays because every
    // call site already supplies it and the access journal is what actually
    // records it; discarding it here is deliberate, not an oversight.
    let _ = modifier;
    let dt = modification_datetime();

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
            encode_old_value(&item.old_value, item.group, item.element).as_bytes(),
        );

        // Build ModifiedAttributesSequence (0400,0550) containing the inner item
        let mut inner_seq = Vec::new();
        write_element(&mut inner_seq, 0xFFFE, 0xE000, "SQ", &inner_item); // Item tag
        let mut mod_attrs_seq = Vec::new();
        write_element(&mut mod_attrs_seq, 0x0400, 0x0550, "SQ", &inner_seq);

        // Build one audit item: who/when/why + ModifiedAttributesSequence.
        //
        // (0400,0550) comes FIRST. An Item is a dataset, so PS3.5 7.1 requires
        // its elements in ascending tag order, and 0x0550 < 0x0562. Writing it
        // last produced an Item that a strict reader either rejects or reads
        // with the sequence unaccounted for - and the sequence is the only
        // place the OLD values live.
        let mut audit_item = Vec::new();
        audit_item.extend_from_slice(&mod_attrs_seq); // ModifiedAttributesSequence
        write_element(&mut audit_item, 0x0400, 0x0562, "DT", dt.as_bytes()); // DateTime
        write_element(
            &mut audit_item,
            0x0400,
            0x0563,
            "LO",
            modifying_system.as_bytes(),
        ); // ModifyingSystem
        write_element(&mut audit_item, 0x0400, 0x0564, "LO", b""); // SourceOfPreviousValues (empty, Type 2)
        // (0400,0565) is CS in PS3.6, not LO: its enumerated values are
        // CORRECT and COERCE (PS3.3 C.12.1.1.9), which is precisely what a
        // Code String carries. The old LO encoding put the wrong VR byte pair
        // in front of every reason this server wrote.
        write_element(
            &mut audit_item,
            0x0400,
            0x0565,
            "CS",
            item.reason.as_bytes(),
        ); // Reason
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

    /// PS3.5 7.1: the Original Attributes Sequence belongs at its sorted
    /// position, and an existing one is a REPLACEMENT seam, not a second copy.
    #[test]
    fn the_original_attributes_seam_is_sorted_and_replaces_an_existing_one() {
        // A dataset with 0008,0060 (Modality), 0010,0010, then PixelData.
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // (0008,0060) CS "CT"
        buf.extend_from_slice(&0x0008u16.to_le_bytes());
        buf.extend_from_slice(&0x0060u16.to_le_bytes());
        buf.extend_from_slice(b"CS");
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(b"CT");
        // (0010,0010) PN "A" (padded to even)
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(b"PN");
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(b"A ");
        let before_pixels = buf.len();
        // (7FE0,0010) OW 4 bytes
        buf.extend_from_slice(&0x7FE0u16.to_le_bytes());
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4]);

        // Absent: the seam is the PixelData tag, NOT the end of the file -
        // group 0400 sorts before 7FE0.
        let seam = original_attributes_splice(&buf).expect("a seam");
        assert_eq!(seam.replace_len, 0, "pure insert");
        assert_eq!(
            seam.offset, before_pixels,
            "the sequence goes before PixelData, not after it"
        );

        // Present: the seam REPLACES the existing sequence, so the tag cannot
        // appear twice. Inserted at its SORTED position (before PixelData),
        // which is where the writer puts it.
        let mut with_seq = Vec::new();
        with_seq.extend_from_slice(&buf[..before_pixels]);
        let seq_start = with_seq.len();
        // (0400,0561) SQ, defined length 8 (one empty item header).
        with_seq.extend_from_slice(&0x0400u16.to_le_bytes());
        with_seq.extend_from_slice(&0x0561u16.to_le_bytes());
        with_seq.extend_from_slice(b"SQ");
        with_seq.extend_from_slice(&0u16.to_le_bytes());
        with_seq.extend_from_slice(&8u32.to_le_bytes());
        with_seq.extend_from_slice(&0xFFFEu16.to_le_bytes());
        with_seq.extend_from_slice(&0xE000u16.to_le_bytes());
        with_seq.extend_from_slice(&0u32.to_le_bytes());
        with_seq.extend_from_slice(&buf[before_pixels..]);
        let seam = original_attributes_splice(&with_seq).expect("a seam");
        assert_eq!(seam.offset, seq_start, "the existing sequence's offset");
        assert_eq!(seam.replace_len, 20, "its whole extent is replaced");

        // LEGACY shape: an object an earlier build wrote carries the sequence
        // AFTER the pixels. It must still be recognised as existing, or a
        // correction would add a second copy of the tag.
        let mut after = buf.clone();
        let legacy_start = after.len();
        after.extend_from_slice(&0x0400u16.to_le_bytes());
        after.extend_from_slice(&0x0561u16.to_le_bytes());
        after.extend_from_slice(b"SQ");
        after.extend_from_slice(&0u16.to_le_bytes());
        after.extend_from_slice(&8u32.to_le_bytes());
        after.extend_from_slice(&0xFFFEu16.to_le_bytes());
        after.extend_from_slice(&0xE000u16.to_le_bytes());
        after.extend_from_slice(&0u32.to_le_bytes());
        let seam = original_attributes_splice(&after).expect("a seam");
        assert_eq!(seam.offset, legacy_start, "the trailing sequence is found");
        assert_eq!(seam.replace_len, 20, "and replaced, not duplicated");
    }

    /// Walk an Explicit VR LE element stream and return `(tag, vr, length)`.
    ///
    /// A reader that trusts the number in the length field is exactly what
    /// the production build must satisfy, so the test parses the way a real
    /// consumer does rather than searching for byte patterns. Part 10 files
    /// start after the 128-byte preamble and "DICM".
    fn walk(buf: &[u8]) -> Vec<((u16, u16), [u8; 2], usize)> {
        let mut out = Vec::new();
        let mut pos = if buf.len() >= 132 && &buf[128..132] == b"DICM" {
            132
        } else {
            0
        };
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
            // PS3.5 7.5: an undefined length is a delimiter-terminated run,
            // not a byte count - follow its items instead of jumping.
            if len == u32::MAX as usize {
                pos = skip_undefined(buf, pos);
                continue;
            }
            // PS3.5 7.1.1: a Value Length is always EVEN, and the pad byte is
            // inside the declared length. The walker used to add the pad back
            // (`pos += len + (len & 1)`), which made an odd declared length
            // walk cleanly - so a fixture that wrote `len` without the pad
            // passed, and the encoder's real padding rule was never tested.
            // A conformant reader must reject the odd case outright.
            assert!(
                len.is_multiple_of(2),
                "PS3.5 7.1.1: value length must be even, got {len} for {group:04X},{elem:04X}"
            );
            pos += len;
        }
        assert_eq!(
            pos,
            buf.len(),
            "the element stream must end exactly at the buffer end, walked {:?}",
            out
        );
        out
    }

    /// Skip the item/delimiter run of an undefined-length element.
    fn skip_undefined(buf: &[u8], mut pos: usize) -> usize {
        loop {
            assert!(
                pos + 8 <= buf.len(),
                "an undefined-length run past the buffer end"
            );
            let group = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
            let elem = u16::from_le_bytes([buf[pos + 2], buf[pos + 3]]);
            let len = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]])
                as usize;
            pos += 8;
            if group == 0xFFFE && (elem == 0xE0DD || elem == 0xE00D) {
                return pos;
            }
            if len == 0xFFFF_FFFF {
                pos = skip_undefined(buf, pos);
                continue;
            }
            pos += len;
        }
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
        //
        // The VR must be CS. PS3.6 defines (0400,0565) as CS and PS3.3
        // C.12.1.1.9 enumerates CORRECT/COERCE, which is exactly what a Code
        // String carries; this assertion used to pin LO, i.e. the defect.
        let reason_at = buf
            .windows(15)
            .position(|w| &w[8..15] == b"CORRECT")
            .expect("ReasonForModification value present");
        assert_eq!(
            &buf[reason_at + 4..reason_at + 6],
            b"CS",
            "PS3.6: (0400,0565) ReasonForTheAttributeModification is CS, not LO"
        );
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

    /// The (0400,0561) Item's elements are in ascending tag order.
    ///
    /// An Item is a dataset, so PS3.5 7.1 requires increasing tags inside it -
    /// the same rule as the outer dataset. `ModifiedAttributesSequence`
    /// (0400,0550) was written AFTER (0400,0562)..(0400,0566), which put the
    /// only copy of the OLD values out of order. Fixing the outer sequence's
    /// position did not fix the Item.
    #[test]
    fn the_original_attributes_item_is_in_ascending_tag_order() {
        let items = [OriginalAttributeItem {
            group: 0x0010,
            element: 0x0020,
            old_value: "PAT-1".to_string(),
            reason: "CORRECT",
        }];
        let buf = build_original_attributes_sequence(&items, "OP", "CLARUS_SERVER");

        // Walk only the elements INSIDE the (0400,0561) Item. The sequence
        // element is tag(4) + VR(2) + reserved(2) + length(4) = 12 bytes, and
        // its Item follows immediately: Item tag(4) + length(4) + payload.
        let seq_at = buf
            .windows(4)
            .position(|w| w == [0x00, 0x04, 0x61, 0x05])
            .expect("the (0400,0561) OriginalAttributesSequence element");
        let item_pos = seq_at + 12;
        assert_eq!(
            &buf[item_pos..item_pos + 4],
            [0xFE, 0xFF, 0x00, 0xE0],
            "the sequence's defining Item (FFFE,E000, little-endian fields) follows its header"
        );
        let item_len = u32::from_le_bytes([
            buf[item_pos + 4],
            buf[item_pos + 5],
            buf[item_pos + 6],
            buf[item_pos + 7],
        ]) as usize;
        let item = &buf[item_pos + 8..item_pos + 8 + item_len];

        let mut tags: Vec<(u16, u16)> = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= item.len() {
            let tag = (
                u16::from_le_bytes([item[pos], item[pos + 1]]),
                u16::from_le_bytes([item[pos + 2], item[pos + 3]]),
            );
            let vr = [item[pos + 4], item[pos + 5]];
            let extended = vr_is_extended(vr);
            let (len, header) = if extended {
                (
                    u32::from_le_bytes([
                        item[pos + 8],
                        item[pos + 9],
                        item[pos + 10],
                        item[pos + 11],
                    ]) as usize,
                    12,
                )
            } else {
                (
                    u16::from_le_bytes([item[pos + 6], item[pos + 7]]) as usize,
                    8,
                )
            };
            // A nested Item (FFFE,E000) inside ModifiedAttributesSequence: skip
            // the containing SQ by its declared length instead of walking in.
            tags.push(tag);
            pos += header + len;
        }
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(
            tags, sorted,
            "the Item's elements must be in ascending tag order (PS3.5 7.1)"
        );
        assert!(
            tags.contains(&(0x0400, 0x0550)),
            "ModifiedAttributesSequence must be inside the Item"
        );
        assert!(
            !tags.contains(&(0x0400, 0x0566)),
            "PS3.6 defines no (0400,0566); it must not be written"
        );
    }

    /// The audit date and time come from ONE clock read.
    ///
    /// They used to come from two: `SystemTime::now()` for the time of day and
    /// `utc_ymd()` for the civil date, which read the clock again. At UTC
    /// midnight those two reads land on different days, so the DT could claim
    /// yesterday's date with today's time - a timestamp that never existed.
    /// The derivation is now a pure function of `secs`, so both sides of
    /// midnight are asserted exactly instead of raced.
    #[test]
    fn the_modification_datetime_derives_date_and_time_from_one_instant() {
        // 2026-11-24 is day 20_789; 23:59:59 and the next second are 86_399
        // and 86_400 seconds into their respective days.
        let day = 20_789i64;
        let last_second = day as u64 * 86_400 + 86_399;
        let next_midnight = day as u64 * 86_400 + 86_400;

        assert_eq!(
            dt_from_secs(last_second),
            format!("{}235959", ymd_from_days(day)),
            "one second before midnight keeps the SAME date"
        );
        assert_eq!(
            dt_from_secs(next_midnight),
            format!("{}000000", ymd_from_days(day + 1)),
            "the next second rolls the date AND resets the clock together"
        );

        // The live call keeps the 14-character DT shape.
        let dt = modification_datetime();
        assert_eq!(dt.len(), 14, "DT is YYYYMMDDHHMMSS: {dt}");
        assert!(dt.bytes().all(|b| b.is_ascii_digit()), "DT is digits: {dt}");
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

    // -- FR-001b: the generated patient elements -------------------------

    /// The two generated elements are written as ordinary Explicit VR LE
    /// attributes a reader can walk, in ascending tag order.
    #[test]
    fn the_coerced_patient_elements_are_walkable() {
        let buf = build_coerced_patient_elements("Anonymous^Patient", "AUTOPAT_1234");
        let walked = walk(&buf);
        assert_eq!(walked.len(), 2, "PN then LO, nothing else");
        assert_eq!(walked[0].0, (0x0010, 0x0010), "PatientName first");
        assert_eq!(&walked[0].1, b"PN");
        assert_eq!(walked[1].0, (0x0010, 0x0020), "PatientID second");
        assert_eq!(&walked[1].1, b"LO");
        // The values are what the manifest holds, and every length is even
        // (PS3.5 7.1.1: the pad byte is part of the value). "Anonymous^Patient"
        // is 17 bytes, so it is padded to 18.
        assert_eq!(walked[0].2, 18, "17-byte PN padded to an even length");
        assert_eq!(&buf[8..26], b"Anonymous^Patient ");
        assert!(walked[0].2.is_multiple_of(2), "PN padded to even");
        assert!(walked[1].2.is_multiple_of(2), "LO padded to even");
    }

    /// An odd-length value is padded with SPACE, and the length field states
    /// the PADDED size - a reader that trusts it must land on the next tag.
    #[test]
    fn an_odd_patient_name_is_space_padded() {
        // 5 bytes -> 6 with the pad.
        let buf = build_coerced_patient_elements("A^B^C", "P1");
        let walked = walk(&buf);
        assert_eq!(walked[0].2, 6, "padded length");
        assert_eq!(&buf[8..14], b"A^B^C ", "trailing SPACE, not NUL");
    }

    /// Nothing generated means nothing written: an empty value must not become
    /// an empty element, which would claim a value where there is none.
    /// Append one Explicit VR LE element (PS3.5 7.1) to a test dataset.
    ///
    /// An empty Sequence (`SQ`) is written as an undefined-length element with
    /// an immediately following SequenceDelimitationItem, the shape a real
    /// writer emits: a zero LENGTH on an SQ means "undefined", not "empty".
    fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
        assert_eq!(
            buf.len() - 132,
            (buf.len() - 132) & !1,
            "dataset stays even"
        );
        // PS3.5 7.1.1: the pad byte is INSIDE the declared Value Length, so the
        // length written is the PADDED one. Writing `value.len()` and appending
        // the pad afterwards left an odd length in the stream; the walker
        // compensated by adding the pad back, which is why the malformed
        // fixture still passed. A fixture that writes what the standard says is
        // what makes the walker's strictness meaningful.
        let pad: Option<u8> = if value.len().is_multiple_of(2) {
            None
        } else if *vr == *b"UI" {
            Some(0)
        } else {
            Some(b' ')
        };
        let padded_len = value.len() + usize::from(pad.is_some());

        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&element.to_le_bytes());
        buf.extend_from_slice(vr);
        let extended = vr_is_extended(*vr);
        if extended {
            buf.extend_from_slice(&[0u8, 0u8]);
            buf.extend_from_slice(&(padded_len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(padded_len as u16).to_le_bytes());
        }
        if value.is_empty() && *vr == *b"SQ" {
            // PS3.5 7.5: an sequence may be written with undefined length and
            // closed by an explicit SequenceDelimitationItem.
            let len_off = buf.len() - 4;
            buf[len_off..len_off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
            return;
        }
        buf.extend_from_slice(value);
        if let Some(byte) = pad {
            buf.push(byte);
        }
    }

    /// The fixture walker REJECTS an odd declared value length.
    ///
    /// The walker used to add the missing pad byte back (`len + (len & 1)`),
    /// so a stream that declared an odd length and then wrote an uncounted pad
    /// walked cleanly. That made the fixture itself malformed and every test
    /// built on it weaker than it looked: the encoder's padding rule was never
    /// actually exercised. PS3.5 7.1.1 requires an even Value Length with the
    /// pad INSIDE it, so an odd one is a broken stream and must stop the walk.
    #[test]
    fn the_walker_rejects_an_odd_declared_value_length() {
        let mut ds = vec![0u8; 128];
        ds.extend_from_slice(b"DICM");
        // Hand-build one element with an ODD length and an uncounted pad.
        ds.extend_from_slice(&0x0010u16.to_le_bytes());
        ds.extend_from_slice(&0x0020u16.to_le_bytes());
        ds.extend_from_slice(b"LO");
        ds.extend_from_slice(&5u16.to_le_bytes()); // 5 is odd
        ds.extend_from_slice(b"PAT-1");
        ds.push(b' '); // the pad, NOT included in the declared length

        let caught = std::panic::catch_unwind(|| walk(&ds));
        assert!(
            caught.is_err(),
            "a stream declaring an odd value length must not walk cleanly"
        );

        // The well-formed twin (padded length declared) walks fine, so the
        // rejection above is about the length, not about the fixture shape.
        let mut ok = vec![0u8; 128];
        ok.extend_from_slice(b"DICM");
        append_element(&mut ok, 0x0010, 0x0020, b"LO", b"PAT-1");
        assert_eq!(walk(&ok).len(), 1);
    }

    /// An undefined-length (0020,xxxx) sequence is a valid insertion anchor.
    ///
    /// The sequence branch skipped the element with `continue` before recording
    /// either anchor, so a missing PatientName was inserted AFTER the sequence
    /// (or at the end of the dataset instead of before it), breaking the
    /// ascending tag order the reader relies on. A sequence is a dataset
    /// element like any other: its tag decides the seam.
    #[test]
    fn an_undefined_length_sequence_is_an_insertion_anchor() {
        let mut ds = vec![0u8; 128];
        ds.extend_from_slice(b"DICM");
        // (0008,0060) CS SR, then (0020,000D) SQ with undefined length.
        append_element(&mut ds, 0x0008, 0x0060, b"CS", b"SR");
        let seq_at = ds.len();
        append_element(&mut ds, 0x0020, 0x000D, b"SQ", b"");

        let splices = patient_element_splices(&ds);
        let pn = splices.patient_name.expect("a seam for PatientName");
        assert_eq!(
            pn.replace_len, 0,
            "an absent tag is an insert, not a replace"
        );
        assert_eq!(
            pn.offset, seq_at,
            "the undefined-length sequence's own tag offset is the seam: it is \
             the first element at or after (0010,0010)"
        );

        // Splicing there keeps the dataset in ascending tag order.
        let mut out = ds[..pn.offset].to_vec();
        out.extend_from_slice(&build_coerced_patient_elements("AUTONAME", "AUTOPAT"));
        out.extend_from_slice(&ds[pn.offset..]);
        let tags: Vec<(u16, u16)> = walk(&out).iter().map(|(t, _, _)| *t).collect();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(tags, sorted, "the spliced dataset must stay ordered");
    }

    /// A probe that ends PART-WAY into the next element is not a complete
    /// dataset.
    ///
    /// A bounded header read can stop inside an element (the `value_end >
    /// src.len()` branch) or after only a few bytes of the following tag
    /// header. The second case used to fall out of the loop with `complete`
    /// still true, so the end of the walk was offered as an insertion seam -
    /// and the insert landed in front of an element that already exists on
    /// disk. A buffer ending exactly between elements is indistinguishable from
    /// that case, so no end-of-buffer seam is offered unless the walk consumed
    /// the buffer exactly.
    #[test]
    fn a_probe_ending_inside_the_next_tag_offers_no_insert_seam() {
        let mut ds = vec![0u8; 128];
        ds.extend_from_slice(b"DICM");
        append_element(&mut ds, 0x0008, 0x0060, b"CS", b"SR");
        // Nothing at or after (0010,0010) exists, so the only candidate seam is
        // the end of the buffer.
        let whole = patient_element_splices(&ds);
        assert_eq!(
            whole
                .patient_name
                .expect("seam for the complete buffer")
                .offset,
            ds.len(),
            "a fully consumed buffer ends at the dataset end"
        );

        // Now cut the next element's header short: 4 bytes of the tag only.
        let mut truncated = ds.clone();
        truncated.extend_from_slice(&0x0020u16.to_le_bytes());
        truncated.extend_from_slice(&0x000Du16.to_le_bytes());
        let cut = patient_element_splices(&truncated);
        assert!(
            cut.patient_name.is_none(),
            "a trailing partial tag header is not a usable seam"
        );
        assert!(cut.patient_id.is_none());
    }

    /// A non-ASCII old value is never written as raw UTF-8.
    ///
    /// The audit record exists to preserve what the value WAS. Writing Rust's
    /// UTF-8 bytes into a dataset that may declare a different Specific
    /// Character Set corrupts exactly that: `José` in an `ISO_IR 100` dataset
    /// arrives as two bytes where the reader expects one, and decodes to
    /// mojibake. With no charset encoder on this path the value is recorded as
    /// a placeholder instead - wrong-but-honest beats confidently wrong.
    #[test]
    fn a_non_ascii_old_value_is_not_written_as_utf8() {
        let items = [OriginalAttributeItem {
            group: 0x0010,
            element: 0x0010,
            old_value: "José".to_string(),
            reason: "CORRECT",
        }];
        let buf = build_original_attributes_sequence(&items, "OP", "CLARUS_SERVER");
        assert!(
            !buf.windows(2).any(|w| w == "é".as_bytes()),
            "the UTF-8 two-byte form of a non-ASCII value must not reach the dataset"
        );
        assert!(
            buf.windows(3).any(|w| w == b"***"),
            "the value is recorded as a placeholder"
        );
        // The element is still PRESENT: the audit record keeps its shape even
        // when the value cannot be preserved. `walk` does not descend into a
        // sequence Item, so the inner element is located by its tag bytes.
        assert!(
            buf.windows(4).any(|w| w == [0x10, 0x00, 0x10, 0x00]),
            "the (0010,0010) element must still be inside the sequence"
        );

        // An ASCII value is written verbatim: the guard must not degrade the
        // common case.
        let ascii = [OriginalAttributeItem {
            group: 0x0010,
            element: 0x0010,
            old_value: "SMIRNOV^IVAN".to_string(),
            reason: "CORRECT",
        }];
        let buf = build_original_attributes_sequence(&ascii, "OP", "CLARUS_SERVER");
        assert!(
            buf.windows(13).any(|w| &w[..12] == b"SMIRNOV^IVAN"),
            "an ASCII old value must round-trip byte for byte"
        );
    }

    #[test]
    fn empty_values_produce_no_elements() {
        assert!(build_coerced_patient_elements("", "").is_empty());
        // Only one of the two coerced: only that element is written.
        let only_id = build_coerced_patient_elements("", "AUTOPAT_X");
        let walked = walk(&only_id);
        assert_eq!(walked.len(), 1);
        assert_eq!(walked[0].0, (0x0010, 0x0020));
    }

    /// A dataset with no PixelData still has to receive the coerced elements.
    ///
    /// An SR, a presentation state or any other pixel-less instance has no
    /// PixelData, and the old walk only ever produced a seam from that tag - so
    /// the manifest carried the generated identity while the stored dataset did
    /// not. Every element here sorts BEFORE group 0010, so no tag anchor can
    /// stand in for the end of the dataset.
    #[test]
    fn a_pixel_less_dataset_still_gets_the_coerced_elements() {
        let mut ds = Vec::new();
        ds.extend_from_slice(&[0u8; 128]);
        ds.extend_from_slice(b"DICM");
        append_element(
            &mut ds,
            0x0008,
            0x0016,
            b"UI",
            b"1.2.840.10008.5.1.4.1.1.88.33",
        );
        append_element(&mut ds, 0x0008, 0x0060, b"CS", b"SR");

        let splices = patient_element_splices(&ds);
        let pn = splices.patient_name.expect("a seam for PatientName");
        let pid = splices.patient_id.expect("a seam for PatientID");
        assert_eq!(pn.replace_len, 0);
        assert_eq!(
            pn.offset,
            ds.len(),
            "trailing insert at the end of the dataset"
        );
        assert_eq!(pid.offset, pn.offset);
        assert_eq!(pid.replace_len, 0);

        // Splicing there must still yield a walkable Explicit VR LE dataset.
        let mut out = ds.clone();
        out.extend_from_slice(&build_coerced_patient_elements("AUTONAME", "AUTOPAT"));
        let walked = walk(&out);
        assert!(
            walked.iter().any(|(tag, _, _)| *tag == (0x0010, 0x0010)),
            "coerced PatientName present, walked {:?}",
            walked
        );
        assert!(
            walked.iter().any(|(tag, _, _)| *tag == (0x0010, 0x0020)),
            "coerced PatientID present, walked {:?}",
            walked
        );
    }

    /// A partially read dataset must not be appended to.
    ///
    /// The caller hands in a bounded header read, so the walk can stop inside
    /// an element. Appending at that offset would write into the element that
    /// was cut off, so a seam may only come from a tag that was fully read.
    #[test]
    fn a_truncated_dataset_gets_no_trailing_insert() {
        let mut ds = Vec::new();
        ds.extend_from_slice(&[0u8; 128]);
        ds.extend_from_slice(b"DICM");
        append_element(
            &mut ds,
            0x0008,
            0x0016,
            b"UI",
            b"1.2.840.10008.5.1.4.1.1.88.33",
        );
        append_element(&mut ds, 0x0008, 0x0060, b"CS", b"SR");
        // Claim a long value and cut the buffer inside it.
        ds.extend_from_slice(&[0x08, 0x00, 0x18, 0x00, b'U', b'I', 0xFF, 0x00]);
        ds.extend_from_slice(b"1.2");

        let splices = patient_element_splices(&ds);
        assert!(
            splices.patient_name.is_none(),
            "no insert seam for a partial read, got {:?}",
            splices.patient_name
        );
        assert!(splices.patient_id.is_none());
    }

    /// A coerced element is inserted in ascending tag order (PS3.5 7.1).
    ///
    /// A missing PatientName has to land BEFORE an existing PatientID. The
    /// dataset-end seam cannot do that: it follows every element, so the test
    /// only passes if the PatientID tag itself is used as the anchor.
    #[test]
    fn the_insert_seam_sits_before_the_patient_tags() {
        let mut ds = Vec::new();
        ds.extend_from_slice(&[0u8; 128]);
        ds.extend_from_slice(b"DICM");
        append_element(
            &mut ds,
            0x0008,
            0x0016,
            b"UI",
            b"1.2.840.10008.5.1.4.1.1.88.33",
        );
        append_element(&mut ds, 0x0010, 0x0020, b"LO", b"PAT-1");
        append_element(&mut ds, 0x0020, 0x000D, b"UI", b"1.2.3");

        let splices = patient_element_splices(&ds);
        let pn = splices.patient_name.expect("a seam for PatientName");
        let pid = splices.patient_id.expect("PatientID is present");
        assert_eq!(pn.replace_len, 0);
        assert!(pid.replace_len > 0, "PatientID is replaced, not inserted");
        // The seam is the PatientID tag itself. It is also the first tag at or
        // after PatientName, so the anchor and a forward scan agree here - the
        // order they must produce is what the spliced dataset is asked for
        // below, and the dataset-end seam fails that check.
        assert_eq!(
            pn.offset, pid.offset,
            "PatientName inserts at the PatientID tag, not at the dataset end"
        );

        // The spliced dataset keeps the tags in ascending order.
        let mut out = ds[..pn.offset].to_vec();
        out.extend_from_slice(&build_coerced_patient_name_element("AUTONAME").unwrap());
        out.extend_from_slice(&ds[pn.offset..]);
        let tags: Vec<(u16, u16)> = walk(&out).into_iter().map(|(tag, _, _)| tag).collect();
        assert_eq!(
            tags,
            vec![
                (0x0008, 0x0016),
                (0x0010, 0x0010),
                (0x0010, 0x0020),
                (0x0020, 0x000D)
            ]
        );
    }
}
