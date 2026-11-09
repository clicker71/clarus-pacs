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
    let pixel_end = pixel_start.saturating_add(pixel_length);

    if pixel_end > dicom_bytes.len() {
        // Pixel data extends beyond buffer - take what we have
        return Some(&dicom_bytes[pixel_start..]);
    }

    Some(&dicom_bytes[pixel_start..pixel_end])
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
    if let Some(result) = find_pixel_data_explicit_vr(header) {
        return Some(result);
    }
    // 2. Fallback: raw byte-scan for (7FE0,0010) in LE
    //    Catches pydicom-generated Explicit VR LE files with non-standard
    //    byte representations (mixed Explicit/Implicit VR, padding, VR="UN"
    //    with special length encoding).
    find_pixel_data_byte_scan(header)
}

/// Phase 1: Explicit VR Little Endian sequential parse.
///
/// Standard DICOM binary layout - walks data elements from offset 132,
/// skipping by VR-determined element sizes. Returns None if the parse
/// loses sync (non-standard encoding, pydicom files, etc.).
fn find_pixel_data_explicit_vr(header: &[u8]) -> Option<(usize, usize)> {
    if header.len() < 132 {
        return None;
    }
    if &header[128..132] != b"DICM" {
        return None;
    }

    let mut offset: usize = 132;
    let data_len = header.len();

    while offset + 8 <= data_len {
        let group = u16::from_le_bytes([header[offset], header[offset + 1]]);
        let element = u16::from_le_bytes([header[offset + 2], header[offset + 3]]);

        // PS3.5 7.5: the item and delimitation tags of group FFFE carry NO
        // VR, so the two bytes a VR would occupy are part of their 32-bit
        // length. Reading a VR here would put the walk four bytes out, and
        // treating the four bytes after the tag as anything but a length
        // would do the same. Each is an eight-byte header, and the walk
        // reads on: an undefined-length sequence is entered, never skipped.
        if group == 0xFFFE {
            offset += 8;
            continue;
        }

        if group == 0x7FE0 && element == 0x0010 {
            let vr = &header[offset + 4..offset + 6];
            if vr == b"OB" || vr == b"OW" {
                if offset + 12 > data_len {
                    return None;
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
                return Some((offset + 12, length));
            }
            return None;
        }

        // Skip to next element
        if offset + 6 > data_len {
            break;
        }

        let vr = &header[offset + 4..offset + 6];
        // PS3.5 7.1.2: these VRs carry a 4-byte length after a 2-byte
        // reserved field, so their header is 12 bytes and not 8. OV, UC and UR
        // are in the same table; treating them as short VRs makes the walker
        // read half of the length as a VR and desynchronise every element
        // after the first one.
        let is_long_vr = matches!(
            vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UR" | b"UT" | b"UN"
        );

        let elem_size = if is_long_vr {
            if offset + 12 > data_len {
                break;
            }
            let length = u32::from_le_bytes([
                header[offset + 8],
                header[offset + 9],
                header[offset + 10],
                header[offset + 11],
            ]) as usize;
            if length == UNDEFINED_LENGTH {
                // PS3.5 7.5.1: the sentinel is the ABSENCE of a length, and
                // only the 32-bit form can express it. A sequence written
                // this way ends at its (FFFE,E0DD), so the walk steps past
                // the header and into the item stream rather than adding a
                // length it does not have - which would carry it past the
                // end of the buffer and lose every element after it.
                //
                // An entered item stream can hold a NESTED (7FE0,0010): the
                // Icon Image Sequence (0088,0200) of PS3.3 C.7.6.1.1.5
                // carries a thumbnail this size, and it is not the image the
                // caller asked for. The items of that sequence are skipped by
                // their delimiters instead of entered, so the walk reaches the
                // top-level Pixel Data that follows it.
                if group == 0x0088 && element == 0x0200 {
                    match skip_undefined_length_sequence(header, offset + 12) {
                        Some(next) => next,
                        None => break,
                    }
                } else {
                    12
                }
            } else {
                12 + length
            }
        } else {
            if offset + 8 > data_len {
                break;
            }
            let length = u16::from_le_bytes([header[offset + 6], header[offset + 7]]) as usize;
            // A 16-bit length cannot express the sentinel (PS3.5 7.5.1), so
            // 0xFFFF here is a real value length and not an escape.
            8 + length
        };

        offset = offset.saturating_add(elem_size);
    }

    None
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
            let long_vr = matches!(
                vr,
                b"OB"
                    | b"OD"
                    | b"OF"
                    | b"OL"
                    | b"OV"
                    | b"OW"
                    | b"SQ"
                    | b"UC"
                    | b"UR"
                    | b"UT"
                    | b"UN"
            );
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

    if header.len() < 140 {
        return None;
    }

    // The Icon Image Sequence (0088,0200) holds a thumbnail Pixel Data of its
    // own (PS3.3 C.7.6.1.1.5). Ranges that fall inside it are skipped: a scan
    // has no stack, so the sequence's extent is located once, up front.
    let icon_ranges = locate_icon_image_sequences(header);

    let mut search_from: usize = 132; // after DICM magic + meta header
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
        let (start, length) = find_pixel_data_explicit_vr(&dicom)
            .expect("the sequential walker gets past the undefined-length sequence");
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
}
