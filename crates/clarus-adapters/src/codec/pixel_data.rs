// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Pixel Data (7FE0,0010) extraction from DICOM byte streams.
//!
//! Scans DICOM binary for the Pixel Data tag and extracts raw pixel bytes
//! without a full parser. Handles Explicit VR Little Endian (the most common
//! transfer syntax for CT/MR/US).

/// Extract the raw bytes of the Pixel Data tag (7FE0,0010) from a DICOM buffer.
///
/// Returns `None` if:
/// - The buffer is not a valid DICOM (no "DICM" magic at offset 128)
/// - No Pixel Data tag is found
/// - The tag uses an unsupported VR (not OB or OW)
///
/// # DICOM binary layout (Explicit VR Little Endian)
/// ```text
/// [0-127]   : Preamble (128 bytes, typically zeros)
/// [128-131] : Magic "DICM"
/// [132+]    : Data Elements:
///   [offset+0:2]   : Group number (u16 LE)
///   [offset+2:4]   : Element number (u16 LE)
///   [offset+4:6]   : VR (2 ASCII bytes)
///   [offset+6:8]   : Reserved (0x0000) for OB/OW/OD/OF/OL/SQ/UT/UN
///   [offset+8:12]  : Length (u32 LE) for long-VR types
///   [offset+12:]   : Value bytes
/// ```
pub fn extract_pixel_data(dicom_bytes: &[u8]) -> Option<&[u8]> {
    let (pixel_start, pixel_length) = find_pixel_data_location(dicom_bytes)?;
    if pixel_length == UNDEFINED_LENGTH {
        // PS3.5 A.4: an encapsulated value is undefined length and is followed
        // by its Sequence Delimitation Item. Its item stream is resolved by the
        // decoder; here the value is bounded by the delimiter when one is
        // present, never by end-of-file (which would swallow any trailing
        // element, e.g. Data Set Trailing Padding).
        return Some(bounded_encapsulated_value(dicom_bytes, pixel_start));
    }
    // PS3.5 7.1.2: a defined-length value is bounded by its declared length.
    // When the buffer ends before that, the callers split into two kinds and
    // the difference matters:
    //
    // * a HEADER-ONLY prefix (the value bytes were never supplied) still
    //   locates the region, and the slice is what the buffer holds - the
    //   rendered-frame path reads metadata from such a prefix;
    // * a value that STARTED and then stopped short is truncated, and a short
    //   tail there is not a pixel payload at all.
    //
    // The two are told apart by whether the buffer contains any of the value:
    // a prefix cut at the element header has none, a truncated payload has
    // some. Declaring four bytes and supplying two is the truncated case and
    // is refused.
    let available = dicom_bytes.len().saturating_sub(pixel_start);
    if pixel_length > available && available != 0 {
        return None;
    }
    let end = pixel_start.checked_add(pixel_length.min(available))?;
    Some(&dicom_bytes[pixel_start..end])
}

/// Slice from `value_start` to the Sequence Delimitation Item that closes it.
///
/// The walk is by Item LENGTH, so a delimiter byte pattern inside a fragment
/// value cannot end the value early; only a real (FFFE,E0DD) at item
/// boundaries does. Without a delimiter in the buffer the whole remainder is
/// returned, which is the bzip2-truncated-input case every decoder handles.
fn bounded_encapsulated_value(data: &[u8], value_start: usize) -> &[u8] {
    let mut pos = value_start;
    while pos + 8 <= data.len() {
        let tag_group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let tag_element = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        if tag_group == 0xFFFE && tag_element == 0xE0DD {
            return &data[value_start..pos];
        }
        let len = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        if len == UNDEFINED_LENGTH || pos + 8 + len > data.len() {
            break;
        }
        pos += 8 + len;
    }
    &data[value_start..]
}

/// Locate the Pixel Data tag (7FE0,0010) in a DICOM header buffer.
///
/// Returns `(pixel_start_offset, pixel_length)` - the absolute byte offset
/// in the file where pixel data begins, and its length in bytes.
///
/// Two-phase lookup:
/// 1. Explicit VR LE sequential parse (fast path, standard DICOM)
/// 2. Raw byte-scan fallback for non-standard encodings (pydicom-generated files)
pub(crate) fn find_pixel_data_location(header: &[u8]) -> Option<(usize, usize)> {
    // 1. Fast path: Explicit VR LE sequential parse (standard DICOM)
    match find_pixel_data_explicit_vr(header) {
        WalkOutcome::Found(location) => return Some(location),
        // The walk completed and there is no top-level Pixel Data. Falling
        // back to the raw scan here returns a NESTED one: the scan excludes
        // only Icon Image Sequences, so Pixel Data inside a defined-length
        // private sequence would be served as if it were the image. A clean
        // miss is an answer, not a failure to answer.
        WalkOutcome::CleanMiss => return None,
        // Only a walk that lost sync is worth a second, blinder opinion.
        WalkOutcome::Desynced => {}
    }
    // 2. Fallback: raw byte-scan for (7FE0,0010) in LE
    //    Catches pydicom-generated Explicit VR LE files with non-standard
    //    byte representations (mixed Explicit/Implicit VR, padding, VR="UN"
    //    with special length encoding).
    find_pixel_data_byte_scan(header)
}

/// How the sequential Explicit VR walk ended.
///
/// "Not found" and "could not walk" are different statements, and only the
/// second one licenses the raw byte scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkOutcome {
    Found((usize, usize)),
    /// The dataset walked to its end with no top-level Pixel Data.
    CleanMiss,
    /// The walk lost sync: a header or a value did not fit the buffer, or the
    /// buffer is not a Part 10 file at all.
    Desynced,
}

/// Phase 1: Explicit VR Little Endian sequential parse.
///
/// Standard DICOM binary layout - walks data elements from offset 132,
/// skipping by VR-determined element sizes. Returns None if the parse
/// loses sync (non-standard encoding, pydicom files, etc.).
fn find_pixel_data_explicit_vr(header: &[u8]) -> WalkOutcome {
    if header.len() < 132 {
        return WalkOutcome::Desynced;
    }
    if &header[128..132] != b"DICM" {
        return WalkOutcome::Desynced;
    }

    let mut offset: usize = 132;
    let data_len = header.len();

    // PS3.5 7.1: Pixel Data is a top-level attribute of the Data Set. An
    // instance can also carry a (7FE0,0010) INSIDE a sequence - the Icon Image
    // Sequence (0088,0200) of PS3.3 C.7.6.1.1.5 is the standard one, a private
    // sequence is another - and those are not the image the caller asked for.
    //
    // `depth` counts only the levels that a matching delimiter will close:
    // an undefined-length Item opens one and its (FFFE,E00D) closes it, and an
    // undefined-length sequence opens one and its (FFFE,E0DD) closes it. A
    // DEFINED-length Item or sequence has no delimiter at all, so it is not
    // counted - it is skipped by its length. Counting a defined-length Item
    // left `depth` permanently above zero, and counting the sequence's E0DD
    // against a depth the sequence never raised dropped it to zero inside an
    // outer item, where a nested Pixel Data then looked top level.
    let mut depth: usize = 0;

    while offset + 8 <= data_len {
        let group = u16::from_le_bytes([header[offset], header[offset + 1]]);
        let element = u16::from_le_bytes([header[offset + 2], header[offset + 3]]);

        // PS3.5 7.5: the item and delimitation tags of group FFFE carry NO
        // VR, so the two bytes a VR would occupy are part of their 32-bit
        // length. Reading a VR here would put the walk four bytes out.
        if group == 0xFFFE {
            let len_at = offset + 4;
            let Some(raw_len) = header.get(len_at..len_at + 4) else {
                return WalkOutcome::Desynced;
            };
            let item_len = u32::from_le_bytes(raw_len.try_into().expect("4 bytes")) as usize;
            match element {
                0xE000 => {
                    // PS3.5 7.5.1: a defined-length Item is closed by its own
                    // length, not by an Item Delimitation Item, so it must not
                    // change the depth - it is stepped over whole. Only the
                    // undefined-length form opens a level that an E00D closes.
                    if item_len == UNDEFINED_LENGTH {
                        depth += 1;
                        offset += 8;
                    } else {
                        offset = offset.saturating_add(8).saturating_add(item_len);
                    }
                }
                0xE00D => {
                    depth = depth.saturating_sub(1);
                    offset += 8;
                }
                0xE0DD => {
                    depth = depth.saturating_sub(1);
                    offset += 8;
                }
                _ => offset += 8,
            }
            continue;
        }

        if group == 0x7FE0 && element == 0x0010 && depth == 0 {
            let vr = &header[offset + 4..offset + 6];
            if vr == b"OB" || vr == b"OW" {
                if offset + 12 > data_len {
                    return WalkOutcome::Desynced;
                }
                let length = u32::from_le_bytes([
                    header[offset + 8],
                    header[offset + 9],
                    header[offset + 10],
                    header[offset + 11],
                ]) as usize;
                // An encapsulated payload is undefined length
                // (0xFFFFFFFF): the frames follow as items. The length is
                // returned as it stands, because `extract_pixel_data`
                // already takes the rest of the buffer when the declared
                // length runs past it.
                return WalkOutcome::Found((offset + 12, length));
            }
            return WalkOutcome::Desynced;
        }

        // Skip to the next element. PS3.5 7.1.2 with 6.2 Table 6.2-1: the
        // width class decides between an 8-byte and a 12-byte header, and
        // `vr_widths` is the one place that table is stated.
        let Some(value) = super::vr_widths::explicit_value(header, offset + 4, false) else {
            return WalkOutcome::Desynced;
        };
        let header_len = 4 + value.header_len;
        if value.len == UNDEFINED_LENGTH {
            // PS3.5 7.5.1: the sentinel is the ABSENCE of a length, and only
            // the 32-bit form can express it. An undefined-length SEQUENCE
            // opens a level here so its own E0DD can close exactly that level
            // rather than one belonging to an enclosing item.
            depth += 1;
            offset = offset.saturating_add(header_len);
        } else {
            offset = offset.saturating_add(header_len).saturating_add(value.len);
        }
    }

    // Reaching the end means the dataset was walked; if nothing opened a
    // level that is left unclosed, this is a clean miss rather than a
    // truncation. A nonzero depth means a delimiter never arrived.
    if depth == 0 {
        WalkOutcome::CleanMiss
    } else {
        WalkOutcome::Desynced
    }
}

/// The undefined-length sentinel (PS3.5 7.5.1), meaningful only in the
/// 32-bit length form of the long VRs.
const UNDEFINED_LENGTH: usize = 0xFFFF_FFFF;

/// Offset just past the (FFFE,E0DD) that closes the sequence whose first item
/// starts at `start`.
///
/// PS3.5 7.5.1: an undefined-length sequence is a run of items, each opened by
/// (FFFE,E000) and closed by (FFFE,E00D), and terminated by (FFFE,E0DD) with
/// a zero length. Items nest, so the walk keeps a depth counter rather than
/// stopping at the first item delimiter - a nested sequence's terminator would
/// otherwise end the outer one early.
fn skip_undefined_length_sequence(header: &[u8], start: usize) -> Option<usize> {
    let mut offset = start;
    let mut depth = 1usize;
    while offset + 8 <= header.len() {
        let group = u16::from_le_bytes([header[offset], header[offset + 1]]);
        let element = u16::from_le_bytes([header[offset + 2], header[offset + 3]]);
        if group != 0xFFFE {
            // An element inside an item: skip it by its VR-determined size so
            // a (FFFE,x) pattern inside a value cannot be mistaken for a
            // delimiter. An undefined-length child is entered, staying in
            // step with the outer depth.
            let vr = header.get(offset + 4..offset + 6)?;
            let long_vr = super::vr_widths::is_long_form_vr(vr);
            let (hdr_len, len) = if long_vr {
                let l = u32::from_le_bytes(header.get(offset + 8..offset + 12)?.try_into().ok()?);
                (12usize, l as usize)
            } else {
                let l = u16::from_le_bytes(header.get(offset + 6..offset + 8)?.try_into().ok()?);
                (8usize, l as usize)
            };
            offset = offset.checked_add(hdr_len)?;
            if len == UNDEFINED_LENGTH {
                depth += 1;
            } else {
                offset = offset.checked_add(len)?;
            }
            continue;
        }
        match element {
            0xE000 => depth += 1,
            0xE00D => depth = depth.saturating_sub(1),
            0xE0DD => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(offset + 8);
                }
            }
            _ => {}
        }
        offset += 8;
    }
    None
}

/// Maximal byte ranges covered by an Icon Image Sequence (0088,0200).
///
/// The byte scan has no element stack, so it asks this helper where the
/// thumbnail lives before it starts. Both encodings are handled: a defined
/// length gives the extent directly, an undefined one runs to its (FFFE,E0DD)
/// as found by `skip_undefined_length_sequence`.
fn locate_icon_image_sequences(header: &[u8]) -> Vec<(usize, usize)> {
    const TAG: [u8; 4] = [0x88, 0x00, 0x00, 0x02]; // (0088,0200) LE
    let mut out = Vec::new();
    let mut offset = 132usize;
    while offset + 12 <= header.len() {
        if header[offset..offset + 4] == TAG && &header[offset + 4..offset + 6] == b"SQ" {
            let len = u32::from_le_bytes([
                header[offset + 8],
                header[offset + 9],
                header[offset + 10],
                header[offset + 11],
            ]) as usize;
            let end = if len == UNDEFINED_LENGTH {
                match skip_undefined_length_sequence(header, offset + 12) {
                    Some(e) => e,
                    None => header.len(),
                }
            } else {
                (offset + 12 + len).min(header.len())
            };
            out.push((offset, end));
            offset = end;
            continue;
        }
        // Not the icon: step a byte. The scan only needs the ranges, and a
        // full desynchronising walk here would duplicate the explicit parser.
        offset += 1;
    }
    out
}

/// Whether `data` is a walkable element stream from `start`.
///
/// Deliberately the same arithmetic the sequential walker uses: a buffer whose
/// bytes do not form complete Explicit VR elements up to the first Pixel Data
/// candidate is not a dataset, whatever tag pattern it happens to contain.
fn walks_as_a_dataset(data: &[u8], start: usize) -> bool {
    let mut offset = start;
    let mut walked = 0usize;
    while offset + 8 <= data.len() {
        let group = u16::from_le_bytes([data[offset], data[offset + 1]]);
        let element = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
        if group == 0xFFFE {
            offset += 8;
            continue;
        }
        if group == 0x7FE0 && element == 0x0010 {
            // Reaching the candidate through complete elements is the proof.
            return walked > 0 || offset == start;
        }
        let Some(value) = super::vr_widths::explicit_value(data, offset + 4, false) else {
            return false;
        };
        if value.len == UNDEFINED_LENGTH {
            return false;
        }
        offset = offset
            .saturating_add(4 + value.header_len)
            .saturating_add(value.len);
        walked += 1;
    }
    false
}

/// Phase 2: Raw byte-scan for (7FE0,0010) tag in Little Endian.
///
/// Brute-force search for the 4-byte tag pattern `E0 7F 10 00` starting
/// after the DICM magic + meta-header (offset 132). Validates:
/// - VR must be OB or OW (pixel data value representations)
/// - Length must be > 0 and must fit in the buffer
/// - Does NOT check reserved bytes (pydicom may write non-standard values)
///
/// Skips false positives by advancing 1 byte past each match.
fn find_pixel_data_byte_scan(header: &[u8]) -> Option<(usize, usize)> {
    const PATTERN: [u8; 4] = [0xE0, 0x7F, 0x10, 0x00]; // (7FE0,0010) LE

    // The 140-byte floor belongs to the Part 10 SHAPE (128-byte preamble plus
    // "DICM" plus a 12-byte long-form Pixel Data header), and it was applied to
    // every buffer before the start was chosen. A bare dataset has no preamble,
    // so the smallest legal one - a 12-byte Pixel Data element header plus two
    // sample bytes - is 14 bytes, and the documented bare mode refused it.
    // Decide the dataset start first and let the bounds check below judge the
    // candidate; only the Part 10 shape needs the preamble length.
    if header.len() < 12 {
        return None;
    }

    // The Icon Image Sequence (0088,0200) holds a thumbnail Pixel Data of its
    // own (PS3.3 C.7.6.1.1.5). Ranges that fall inside it are skipped: a scan
    // has no stack, so the sequence's extent is located once, up front.
    let icon_ranges = locate_icon_image_sequences(header);

    // The byte pattern alone is not evidence: any four bytes of a JPEG or a
    // text file followed by a plausible length would read as a pixel payload.
    // Two shapes are accepted, and both are validated structurally rather than
    // by inspection of the pattern:
    //
    // * a Part 10 file (PS3.10 7.1) - the 128-byte preamble and "DICM", then
    //   the meta group and the dataset;
    // * a BARE dataset, which is a header slice whose caller stripped the
    //   preamble (the `bounded` metadata readers hand one over). It is only
    //   accepted when the walk from offset 0 reaches the tag through complete
    //   elements, which no arbitrary buffer does by accident.
    let part10 = header.len() >= 132 && &header[128..132] == b"DICM";
    // A 132-byte preamble with the magic stripped is the same Part 10 file
    // minus its "DICM": the dataset still starts at 132, and a zero-filled
    // preamble is the only thing that can occupy those bytes in a file whose
    // elements have not been shifted.
    let bare_with_preamble = header.len() >= 132 && header[..128].iter().all(|b| *b == 0);
    let mut search_from: usize = if part10 || bare_with_preamble {
        132 // after the preamble (and the magic, when it is there)
    } else {
        // A dataset with neither is verified by the same header rules the
        // walker uses; if it cannot be walked from zero, the buffer is not one.
        if !walks_as_a_dataset(header, 0) {
            return None;
        }
        0
    };
    while let Some(pos) = header[search_from..].windows(4).position(|w| w == PATTERN) {
        let abs = search_from + pos;
        if icon_ranges.iter().any(|&(s, e)| abs >= s && abs < e) {
            search_from = abs + 1;
            continue;
        }
        if abs + 12 > header.len() {
            return None;
        }
        let vr = &header[abs + 4..abs + 6];
        if vr == b"OB" || vr == b"OW" {
            let length = u32::from_le_bytes([
                header[abs + 8],
                header[abs + 9],
                header[abs + 10],
                header[abs + 11],
            ]) as usize;
            // A byte scan has no geometry to consult, so the one honest
            // check is whether the declared value fits the buffer we hold.
            // A fixed ceiling cannot stand in for it: 1 MiB is exactly one
            // 1024x1024 8-bit frame, and 2 MiB is one 16-bit frame, both
            // ordinary images this fallback used to refuse. The undefined
            // sentinel is not a length either - an encapsulated payload is
            // taken as everything from the value offset onward.
            if length >= UNDEFINED_LENGTH || (length > 0 && length <= header.len() - (abs + 12)) {
                return Some((abs + 12, length));
            }
        }
        search_from = abs + 1; // continue after false positive
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal DICOM with Pixel Data for testing.
    fn make_test_dicom(pixel_bytes: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256 + pixel_bytes.len());

        // Preamble (128 zero bytes) + "DICM"
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");

        // Add a dummy PatientName tag (0010,0010) before Pixel Data
        // Group=0010 Element=0010 VR=PN length=4 value="Test"
        buf.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]); // (0010,0010)
        buf.extend_from_slice(b"PN");
        buf.extend_from_slice(&[0x04, 0x00]); // length=4 (short VR)
        buf.extend_from_slice(b"Test");

        // Pixel Data tag (7FE0,0010) OB with 4-byte length
        let pixel_len = pixel_bytes.len() as u32;
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]); // (7FE0,0010)
        buf.extend_from_slice(b"OB");
        buf.extend_from_slice(&[0x00, 0x00]); // reserved
        buf.extend_from_slice(&pixel_len.to_le_bytes());
        buf.extend_from_slice(pixel_bytes);

        buf
    }

    #[test]
    fn test_extract_pixel_data_present() {
        let pixels = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let dicom = make_test_dicom(&pixels);
        let result = extract_pixel_data(&dicom);
        assert_eq!(result, Some(pixels.as_slice()));
    }

    #[test]
    fn test_extract_pixel_data_empty() {
        let dicom = make_test_dicom(&[]);
        let result = extract_pixel_data(&dicom);
        // Empty pixel data is valid - returns empty slice (not None)
        assert_eq!(result, Some([].as_slice()));
    }

    #[test]
    fn test_extract_pixel_data_no_pixels() {
        // DICOM without Pixel Data tag
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // Just a PatientName tag
        buf.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]); // (0010,0010)
        buf.extend_from_slice(b"PN");
        buf.extend_from_slice(&[0x04, 0x00]); // length=4
        buf.extend_from_slice(b"Test");

        let result = extract_pixel_data(&buf);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_pixel_data_not_dicom() {
        let result = extract_pixel_data(b"not a dicom file at all");
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_pixel_data_too_short() {
        let result = extract_pixel_data(&[0u8; 64]);
        assert_eq!(result, None);
    }

    /// A one-item sequence written with the undefined length sentinel, as
    /// the derived encoder writes (0008,2112): an `SQ` header whose length
    /// is 0xFFFFFFFF, an item whose own length is 0xFFFFFFFF, the item's
    /// elements, then (FFFE,E00D) and (FFFE,E0DD).
    ///
    /// Built byte by byte rather than by the walker under test, so a fixture
    /// cannot agree with a reader by sharing its mistake.
    fn append_undefined_length_sq(buf: &mut Vec<u8>, group: u16, element: u16) {
        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&element.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // (FFFE,E000) item, undefined length.
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE000u16.to_le_bytes());
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // One UI element inside the item, so the item is not empty. The UID
        // is 27 characters, so it takes one NUL pad byte and its length is
        // 28 (PS3.5 6.2: an odd-length value is padded to even).
        buf.extend_from_slice(&0x0008u16.to_le_bytes());
        buf.extend_from_slice(&0x1150u16.to_le_bytes());
        buf.extend_from_slice(b"UI");
        buf.extend_from_slice(&28u16.to_le_bytes());
        buf.extend_from_slice(b"1.2.840.10008.5.1.4.1.1.1.1\0");
        // (FFFE,E00D) item delimiter, (FFFE,E0DD) sequence delimiter.
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE00Du16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE0DDu16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
    }

    /// The minimal instance the prompt specifies: an undefined-length
    /// sequence, then PixelData OW whose length is exactly 1 MiB.
    ///
    /// 1 MiB is not an arbitrary size here: 1024x1024 at 8 bits is exactly
    /// this many bytes, so it sits on the boundary a `length < 1048576` test
    /// rejects while a `length <= buffer` test accepts.
    fn make_undefined_sq_dicom(rows: u16, cols: u16, pixel_len: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1024 + pixel_len);
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        append_undefined_length_sq(&mut buf, 0x0008, 0x2112);
        // The geometry, which is what a reader past the sequence needs.
        for (group, element, value) in [(0x0028u16, 0x0010u16, rows), (0x0028, 0x0011, cols)] {
            buf.extend_from_slice(&group.to_le_bytes());
            buf.extend_from_slice(&element.to_le_bytes());
            buf.extend_from_slice(b"US");
            buf.extend_from_slice(&2u16.to_le_bytes());
            buf.extend_from_slice(&value.to_le_bytes());
        }
        // PixelData OW, the tag the extractor is looking for.
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(pixel_len as u32).to_le_bytes());
        buf.extend(std::iter::repeat_n(0xA5u8, pixel_len));
        buf
    }

    #[test]
    fn test_extract_pixel_data_past_an_undefined_length_sequence() {
        // The defect this pins: the walker added 12 + 0xFFFFFFFF for the
        // sentinel, ran off the end, returned None, and the byte-scan that
        // took over then refused the payload for being a whole mebibyte.
        // The observed symptom was "frame: no PixelData (7FE0,0010)" on an
        // instance whose PixelData is right there.
        //
        // The assertion is on the WALKER, not on the public entry point: the
        // byte scan is a fallback that finds this tag on its own, so a test
        // that only called `extract_pixel_data` would stay green with the
        // walker broken and would be measuring the wrong thing.
        let dicom = make_undefined_sq_dicom(1024, 1024, 1024 * 1024);
        let (start, length) = match find_pixel_data_explicit_vr(&dicom) {
            WalkOutcome::Found(location) => location,
            other => panic!(
                "the sequential walker gets past the undefined-length sequence, got {other:?}"
            ),
        };
        assert_eq!(length, 1024 * 1024, "the declared value length");
        assert_eq!(start, dicom.len() - 1024 * 1024, "the payload offset");

        let pixels = extract_pixel_data(&dicom).expect("PixelData past the sequence");
        assert_eq!(
            pixels.len(),
            1024 * 1024,
            "exactly one 1024x1024 8-bit frame"
        );
        assert!(
            pixels.iter().all(|byte| *byte == 0xA5),
            "the payload itself"
        );
    }

    #[test]
    fn test_extract_pixel_data_at_the_old_ceiling() {
        // The boundary the removed constant sat on. A `<` test against
        // 512*512*4 rejects 1 MiB; a 2 MiB 16-bit frame it rejects by a
        // factor of two. Both are ordinary images.
        //
        // Asserted on the FALLBACK, because that is where the ceiling lived:
        // the walker has no cap to begin with, so a test that routed through
        // it would never exercise the constant this pins.
        for len in [1024 * 1024, 2 * 1024 * 1024] {
            let dicom = make_undefined_sq_dicom(1024, 1024, len);
            let (start, scanned) = find_pixel_data_byte_scan(&dicom)
                .unwrap_or_else(|| panic!("the byte scan must accept {len} bytes"));
            assert_eq!(scanned, len, "the scanned length");
            assert_eq!(start, dicom.len() - len, "the payload offset");

            let pixels = extract_pixel_data(&dicom).expect("and the entry point finds it");
            assert_eq!(pixels.len(), len);
        }
    }

    #[test]
    fn test_a_length_beyond_the_buffer_is_still_refused() {
        // The cap is gone, not the sanity check: a byte scan cannot know the
        // geometry, so "the declared value fits what we hold" is the one
        // honest validation left, and this is the case it exists for.
        //
        // The length is large but DEFINED: the undefined sentinel is not a
        // length to validate at all, and the walker returns it unchanged for
        // an encapsulated payload. This drives the fallback, which is where
        // the fit check lives - so the walker's magic is removed to get
        // there, exactly as a non-standard file would.
        let mut dicom = make_undefined_sq_dicom(1024, 1024, 1024 * 1024);
        let payload_at = dicom.len() - 1024 * 1024;
        let tag = payload_at - 12;
        dicom[tag + 8..tag + 12].copy_from_slice(&0x7FFF_FFF0u32.to_le_bytes());
        dicom.truncate(tag + 12);
        // No DICM magic, so the sequential walker does not run.
        dicom[128..132].copy_from_slice(b"XXXX");
        assert_eq!(extract_pixel_data(&dicom), None);
    }

    #[test]
    fn test_a_defined_length_sequence_is_still_skipped_by_its_length() {
        // The other half of the fix must not regress: a defined-length
        // sequence is a plain jump, and the element after it is found where
        // the length says it is.
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // (0008,2112) SQ, defined length: an 8-byte item header plus 28
        // bytes of NUL-padded UI value = 36.
        buf.extend_from_slice(&0x0008u16.to_le_bytes());
        buf.extend_from_slice(&0x2112u16.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&36u32.to_le_bytes());
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE000u16.to_le_bytes());
        buf.extend_from_slice(&28u32.to_le_bytes());
        buf.extend_from_slice(b"1.2.840.10008.5.1.4.1.1.1.1\0");
        // Then the payload.
        let payload = [0x11u8, 0x22, 0x33, 0x44];
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&payload);
        assert_eq!(extract_pixel_data(&buf), Some(payload.as_slice()));
    }

    #[test]
    fn test_a_delimiter_is_consumed_without_reading_a_vr() {
        // PS3.5 7.5: group FFFE carries no VR. A walker that read one would
        // take two bytes of the 32-bit length as the VR and step four bytes
        // out at every delimiter, so a stray (FFFE,E00D) is the sharpest
        // probe for it.
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // A delimiter at top level, which is where a file with a closed
        // sequence should never have one - exactly why it is a good probe.
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE00Du16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        let payload = [0x55u8, 0x66];
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&payload);
        assert_eq!(extract_pixel_data(&buf), Some(payload.as_slice()));
    }

    #[test]
    fn test_an_icon_image_sequence_is_skipped_for_the_real_pixels() {
        // PS3.3 C.7.6.1.1.5: the Icon Image Sequence (0088,0200) carries its
        // own (7FE0,0010). A walker that enters an undefined-length sequence
        // hands back the ICON as if it were the image, so a caller told 512x512
        // decodes a 64x64 thumbnail. The top-level payload must win.
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");

        // (0088,0200) SQ, undefined length, one item.
        buf.extend_from_slice(&0x0088u16.to_le_bytes());
        buf.extend_from_slice(&0x0200u16.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE000u16.to_le_bytes());
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // The icon's own Pixel Data - deliberately distinguishable.
        let icon = [0xAAu8; 6];
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OB");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(icon.len() as u32).to_le_bytes());
        buf.extend_from_slice(&icon);
        // Item delimiter, then sequence delimiter.
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE00Du16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE0DDu16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());

        // The real top-level payload.
        let payload = [0x11u8, 0x22, 0x33, 0x44, 0x55];
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&payload);

        assert_eq!(extract_pixel_data(&buf), Some(payload.as_slice()));
    }

    #[test]
    fn test_an_icon_image_sequence_alone_yields_no_pixels() {
        // The guard must not fall into a byte scan that finds the icon: with
        // the icon as the only Pixel Data there is nothing to return, and a
        // silent thumbnail is worse than a miss the caller can see.
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        buf.extend_from_slice(&0x0088u16.to_le_bytes());
        buf.extend_from_slice(&0x0200u16.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE000u16.to_le_bytes());
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OB");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&[0xAA; 4]);
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE0DDu16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());

        assert_eq!(extract_pixel_data(&buf), None);
    }

    #[test]
    fn test_an_encapsulated_pixel_data_is_taken_to_the_end() {
        // PS3.5 7.5.1: an encapsulated payload has no defined length. The
        // extractor already handles that by taking everything from the value
        // offset; the fix must not turn the sentinel into an ordinary number
        // and start validating against it.
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OB");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let frames = [0xFEu8, 0xFF, 0x00, 0xE0, 0x01, 0x02, 0x03, 0x04];
        buf.extend_from_slice(&frames);
        assert_eq!(extract_pixel_data(&buf), Some(frames.as_slice()));
    }

    // ---- A2b: extraction-scope regressions ------------------------------

    /// A native element that declares more than the buffer holds is truncated.
    ///
    /// PS3.5 7.1.2 bounds the value by its declared length, so a two-byte tail
    /// is not a four-byte payload: the old "take what we have" made a short
    /// buffer answer for the whole element, and the caller sized a frame from
    /// a value that was not there.
    #[test]
    fn a_truncated_native_payload_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&4u32.to_le_bytes()); // declares four bytes
        buf.extend_from_slice(&[0xAA, 0xBB]); // supplies two
        assert_eq!(extract_pixel_data(&buf), None);
    }

    /// A header-only prefix still locates the region.
    ///
    /// The rendered-frame path parses a prefix that stops at the PixelData
    /// header; the value was never supplied, so there is nothing to truncate.
    /// The distinction is "no value bytes at all" versus "started and stopped".
    #[test]
    fn a_header_only_prefix_still_locates_the_region() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&4096u32.to_le_bytes());
        assert_eq!(extract_pixel_data(&buf), Some([].as_slice()));
    }

    /// A nested Pixel Data inside a private sequence is not the image.
    ///
    /// PS3.5 7.1: Pixel Data is a top-level attribute. The Icon Image Sequence
    /// (0088,0200) is the standard nesting, but the rule is about the level,
    /// not about the tag that encloses it.
    #[test]
    fn a_private_sequences_pixel_data_is_not_the_image() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // A private sequence (0029,1002) SQ, undefined length, one item.
        buf.extend_from_slice(&0x0029u16.to_le_bytes());
        buf.extend_from_slice(&0x1002u16.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // Its own Pixel Data, deliberately distinguishable.
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OB");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&[0xAA; 4]);
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE00Du16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
        buf.extend_from_slice(&0xE0DDu16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());

        // The real top-level payload.
        let payload = [0x11u8, 0x22, 0x33, 0x44];
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&payload);
        assert_eq!(extract_pixel_data(&buf), Some(payload.as_slice()));
    }

    /// A dataset whose ONLY Pixel Data is nested must answer None.
    ///
    /// The sequential walk sees the whole dataset and finds no top-level
    /// (7FE0,0010) - a CLEAN miss. Falling back to the raw byte scan there is
    /// wrong: the scan excludes only Icon Image Sequences, so it returns the
    /// private sequence's Pixel Data as if it were the image. The fallback is
    /// licensed only by a walk that lost sync.
    #[test]
    fn a_clean_miss_does_not_fall_back_to_the_byte_scan() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // A DEFINED-length private sequence holding one defined-length item
        // whose only attribute is Pixel Data. Nothing follows it, so there is no
        // top-level payload to find.
        let mut inner = Vec::new();
        inner.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        inner.extend_from_slice(b"OB");
        inner.extend_from_slice(&[0, 0]);
        inner.extend_from_slice(&4u32.to_le_bytes());
        inner.extend_from_slice(&[0xAA; 4]);

        let mut sq_value = Vec::new();
        sq_value.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]); // (FFFE,E000)
        sq_value.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        sq_value.extend_from_slice(&inner);

        buf.extend_from_slice(&0x0029u16.to_le_bytes());
        buf.extend_from_slice(&0x1002u16.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(sq_value.len() as u32).to_le_bytes());
        buf.extend_from_slice(&sq_value);

        assert_eq!(
            extract_pixel_data(&buf),
            None,
            "a nested payload is not the image, and nothing top-level exists"
        );
    }

    /// A defined-length Item is skipped by its length, not counted as a level.
    #[test]
    fn a_defined_length_sequence_does_not_hide_the_top_level_pixel_data() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        // (0008,1115) SQ, DEFINED length: one item of 8 + 4 bytes.
        buf.extend_from_slice(&0x0008u16.to_le_bytes());
        buf.extend_from_slice(&0x1115u16.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&8u32.to_le_bytes());
        // (0028,0010) US 512 inside the item.
        buf.extend_from_slice(&0x0028u16.to_le_bytes());
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(b"US");
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&512u16.to_le_bytes());
        buf.extend_from_slice(&[0, 0]); // even-length pad for the item

        let payload = [0x77u8, 0x88];
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OW");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&payload);
        assert_eq!(extract_pixel_data(&buf), Some(payload.as_slice()));
    }

    /// An encapsulated value stops at its Sequence Delimitation Item.
    ///
    /// PS3.5 A.4: the value is undefined length, and an element may follow it
    /// (Data Set Trailing Padding). Taking everything to EOF swallowed that
    /// element into the pixel payload.
    #[test]
    fn an_encapsulated_value_stops_at_its_delimiter() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 128]);
        buf.extend_from_slice(b"DICM");
        buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        buf.extend_from_slice(b"OB");
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // Empty BOT item, then one frame item.
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&0u32.to_le_bytes());
        let frame = [0xDEu8, 0xAD, 0xBE, 0xEF];
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        buf.extend_from_slice(&frame);
        buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        buf.extend_from_slice(&0u32.to_le_bytes());
        let value_len = extract_pixel_data(&buf).expect("located").len();
        // Everything from the value offset to the delimiter: two item headers
        // (16) plus the frame (4), and NOT the delimiter itself.
        assert_eq!(value_len, 20, "the trailing element is excluded");

        // A trailing element after the delimiter must not enter the value.
        buf.extend_from_slice(&0x0028u16.to_le_bytes());
        buf.extend_from_slice(&0x0010u16.to_le_bytes());
        buf.extend_from_slice(b"US");
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&512u16.to_le_bytes());
        assert_eq!(
            extract_pixel_data(&buf).expect("located").len(),
            20,
            "the element after the delimiter is still excluded"
        );
    }

    /// A byte pattern alone is not a Pixel Data element.
    ///
    /// The fallback used to return any `E0 7F 10 00 OB|OW` sequence from a
    /// buffer that is not DICOM at all. A bare dataset is still accepted (a
    /// caller may hand over a slice with the preamble stripped), but only when
    /// it walks as complete elements up to the tag.
    #[test]
    fn a_byte_pattern_in_non_dicom_data_is_refused() {
        let mut junk = vec![0x42u8; 200];
        junk.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        junk.extend_from_slice(b"OW");
        junk.extend_from_slice(&[0, 0]);
        junk.extend_from_slice(&4u32.to_le_bytes());
        junk.extend_from_slice(&[0xAA; 4]);
        assert_eq!(extract_pixel_data(&junk), None);
    }
}
