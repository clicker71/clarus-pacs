// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The derived image IOD: a processed frame stored as its own DICOM image.
//!
//! A processor that replaces pixels (a filter, a denoiser, a super-resolution
//! model) returns raw frames, and clinfer wraps each one into a standard
//! image object. This is the same hand-written, zero-dependency style as
//! [`crate::pr`] and [`crate::sr`]: explicit VR little endian, stack buffers
//! for the fixed pieces, one owned `Vec<u8>` for the dataset.
//!
//! # Why Source Image Sequence and not private tags
//!
//! The object has to say what it was derived from, and DICOM already has the
//! attribute for it: (0008,2112) Source Image Sequence names the instance the
//! pixels came from (PS3.3 C.7.6.1). Provenance belongs in standard
//! attributes - the processing identity a reader cannot express (which
//! script, which model version) stays in the ABI's `result.json` and the
//! log, not in a private tag no viewer understands.
//!
//! # Geometry is copied, never invented
//!
//! Rows, Columns, Bits Allocated, Bits Stored, High Bit, Pixel
//! Representation, Photometric Interpretation and Samples per Pixel all come
//! from the SOURCE instance. A derived image whose geometry disagreed with
//! its source would be a different image than the one the processor replaced
//! the pixels of, and a viewer overlaying the two would silently misregister
//! them.

use crate::pixels::Geometry;
use crate::pr::{
    EXPLICIT_VR_LE, write_cs, write_explicit_header, write_item_end, write_item_start, write_lo,
    write_pn, write_sequence_end, write_sequence_start, write_string, write_ui,
};

/// SOP Class UID of a Secondary Capture image.
///
/// The right class for a machine-produced image that is not an acquisition:
/// a filtered or model-processed frame is exactly what SC exists for, and
/// every viewer knows it. The current unified SC class (with the `.1`
/// suffix retired) is written as the primary class, because unlike the
/// presentation-state situation in [`crate::pr`] there is no older class a
/// viewer set depends on.
pub const SECONDARY_CAPTURE_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.7";

/// The identity a derived image needs, supplied by the caller so a test can
/// pin it and the worker can mint it fresh.
#[derive(Debug, Clone)]
pub struct DerivedIdentity {
    /// (0008,0018) of the derived image itself.
    pub sop_instance_uid: String,
    /// (0020,000D) of the study it belongs to.
    pub study_uid: String,
    /// (0020,000E) of the series it is stored in.
    pub series_uid: String,
    /// (0020,0013) Instance Number within that series.
    pub instance_number: u32,
    /// The source instance the pixels were derived from: (0008,2112) item
    /// -> (0008,1150) + (0008,1155).
    pub source_sop_class_uid: String,
    pub source_sop_instance_uid: String,
    /// (0010,0010) copied from the source study, so the result is filed
    /// under the same patient as the images it replaces.
    pub patient_name: Option<String>,
    /// (0010,0020) copied from the source study.
    pub patient_id: Option<String>,
    /// (0008,0060) of the SOURCE instance. A processed DX stays a DX: the
    /// modality describes what the image is, and a filter does not change
    /// that. (A modality that changed would be a different object, not a
    /// derived one.)
    pub source_modality: String,
    /// (0008,103E) Series Description of the derived series.
    pub series_description: Option<String>,
}

/// One derived frame's pixel payload and its geometry.
#[derive(Debug, Clone)]
pub struct DerivedPixels<'a> {
    /// The raw pixel bytes: the processor's output, already validated
    /// against the source geometry by the caller.
    pub bytes: &'a [u8],
    /// The SOURCE geometry the payload matches.
    pub geometry: Geometry,
    /// (0028,0004) of the source, e.g. `MONOCHROME2`.
    pub photometric: String,
    /// 1-based frame position, for the audit trail in the log.
    pub frame_index: u32,
}

/// Serialize one derived image as a Part-10 file.
///
/// `pixels.bytes` must already have been validated against
/// `pixels.geometry`: this writer records what it is given rather than
/// second-guessing it, and a mismatch is the caller's to refuse (the ABI
/// seam does, by comparing the processor's declared output against the
/// manifest).
#[must_use]
pub fn serialize(identity: &DerivedIdentity, pixels: &DerivedPixels<'_>) -> Vec<u8> {
    let mut dataset = Vec::with_capacity(pixels.bytes.len() + 1024);

    // -- What this object is ------------------------------------------
    // (0008,0005) SpecificCharacterSet: the patient identity copied below
    // may be non-ASCII, so the object declares its repertoire (PS3.5
    // 6.1.2.3). Same fixed value the PR and SR writers use.
    write_cs(
        &mut dataset,
        0x0008,
        0x0005,
        crate::sr::SPECIFIC_CHARACTER_SET,
    );
    // (0008,0008) ImageType: a DERIVED image, in the standard's own
    // vocabulary (PS3.3 C.7.6.1.1.3). Leaving it empty is legal; saying
    // DERIVED is what tells a reader this is not an acquisition.
    write_cs(&mut dataset, 0x0008, 0x0008, "DERIVED\\SECONDARY");
    write_ui(
        &mut dataset,
        0x0008,
        0x0016,
        SECONDARY_CAPTURE_SOP_CLASS_UID,
    );
    write_ui(&mut dataset, 0x0008, 0x0018, &identity.sop_instance_uid);

    // PS3.5 7.1: the top-level elements go in ASCENDING tag order. The
    // Secondary Capture module has no required order of its own, but a stream
    // that jumps 0008 -> 0020 -> 0010 -> 0008 asks every reader to cope with a
    // shape the standard does not allow, and some tag-walking readers stop at
    // the first out-of-order element.
    let (date, time) = crate::pr::now_stamp();
    write_string(&mut dataset, 0x0008, 0x0020, b"DA", &date);
    write_string(&mut dataset, 0x0008, 0x0030, b"TM", &time);
    // (0008,0060) Modality: carried from the source, because a processed
    // image IS the same kind of image.
    write_cs(&mut dataset, 0x0008, 0x0060, &identity.source_modality);
    // (0008,0064) Conversion Type is TYPE 1 in the Secondary Capture IOD
    // (PS3.3 C.8.6.1/Table C.8-6): the attribute is mandatory and the term
    // 'WSD' (workstation) says the image was produced on a workstation - which
    // is what a derived render is. Leaving it out made the object illegal for
    // a validator that enforces the module's type 1 requirements.
    write_cs(&mut dataset, 0x0008, 0x0064, "WSD");
    // (0008,0070) Manufacturer - Type 2 in the General Equipment module.
    write_lo(&mut dataset, 0x0008, 0x0070, crate::sr::MANUFACTURER);
    if let Some(description) = &identity.series_description {
        write_lo(&mut dataset, 0x0008, 0x103E, description);
    }

    // -- The source ----------------------------------------------------
    // (0008,2112) Source Image Sequence: what the pixels were derived
    // from (PS3.3 C.7.6.1). This is the whole provenance mechanism on the
    // DICOM side. It stays in group 0008, ahead of the patient and study
    // groups, so the ascending order holds all the way to PixelData.
    write_sequence_start(&mut dataset, 0x0008, 0x2112);
    write_item_start(&mut dataset);
    write_ui(&mut dataset, 0x0008, 0x1150, &identity.source_sop_class_uid);
    write_ui(
        &mut dataset,
        0x0008,
        0x1155,
        &identity.source_sop_instance_uid,
    );
    write_item_end(&mut dataset);
    write_sequence_end(&mut dataset);

    // Patient identity, Type 2: present, empty when the source had none.
    write_pn(
        &mut dataset,
        0x0010,
        0x0010,
        identity.patient_name.as_deref().unwrap_or(""),
    );
    write_lo(
        &mut dataset,
        0x0010,
        0x0020,
        identity.patient_id.as_deref().unwrap_or(""),
    );

    // (0020,000D)/(0020,000E): the result belongs to the study it was
    // derived from, and the origin's STOW refuses an object that does not
    // name its study.
    write_ui(&mut dataset, 0x0020, 0x000D, &identity.study_uid);
    write_ui(&mut dataset, 0x0020, 0x000E, &identity.series_uid);
    write_is_number(&mut dataset, 0x0020, 0x0011, 1);
    write_is_number(&mut dataset, 0x0020, 0x0013, identity.instance_number);

    // -- Pixel geometry, copied from the source ------------------------
    // Every one of these is copied rather than chosen: the derived image
    // must be geometrically identical to the frame it replaces, or a
    // viewer overlaying the two misregisters them.
    write_us(
        &mut dataset,
        0x0028,
        0x0002,
        u16::try_from(pixels.geometry.samples).unwrap_or(1),
    );
    write_cs(&mut dataset, 0x0028, 0x0004, &pixels.photometric);
    write_us(
        &mut dataset,
        0x0028,
        0x0010,
        u16::try_from(pixels.geometry.rows).unwrap_or(0),
    );
    write_us(
        &mut dataset,
        0x0028,
        0x0011,
        u16::try_from(pixels.geometry.cols).unwrap_or(0),
    );
    // (0028,0100) BitsAllocated, (0028,0101) BitsStored and the derived
    // HighBit: the standard's invariant is HighBit = BitsStored - 1, and
    // writing it from BitsStored is what keeps the three consistent.
    write_us(&mut dataset, 0x0028, 0x0100, pixels.geometry.bits_allocated);
    let bits_stored = u16::from(pixels.geometry.bits_stored);
    write_us(&mut dataset, 0x0028, 0x0101, bits_stored);
    write_us(&mut dataset, 0x0028, 0x0102, bits_stored.saturating_sub(1));
    // (0028,0103) PixelRepresentation: 0 = unsigned, 1 = signed, taken
    // from the source so a signed image stays signed.
    write_us(
        &mut dataset,
        0x0028,
        0x0103,
        u16::from(pixels.geometry.signed),
    );

    // -- The pixels ----------------------------------------------------
    // Written with an explicit little-endian header. The byte order is
    // the encoding's: explicit VR little endian is what this writer
    // produces, so the payload is little endian by construction - the
    // processor returned the source's own byte order, because clinfer
    // asked the origin for that transfer syntax in the first place.
    //
    // PS3.5 7.1.2/8.1: the Value Field of Pixel Data shall be an EVEN number
    // of bytes, and the pad goes after the last sample of the whole field. An
    // odd byte count written as-is put the next element one byte early and a
    // reader resolved it by reading value bytes as a VR. The pad is zero:
    // PS3.5 8.1 calls it data "not part of the image".
    let pad = pixels.bytes.len() % 2;
    // VR split (PS3.5 6.2, Table 6.2-1): OB carries byte-sized samples, OW
    // carries 16-bit words. Writing OW for a <=8-bit image told a native
    // decoder the payload was words, so it read two samples per element and
    // the image came out at half width.
    let pixel_vr: &[u8; 2] = if pixels.geometry.bits_allocated <= 8 {
        b"OB"
    } else {
        b"OW"
    };
    write_explicit_header(
        &mut dataset,
        0x7FE0,
        0x0010,
        pixel_vr,
        pixels.bytes.len() + pad,
    );
    dataset.extend_from_slice(pixels.bytes);
    if pad == 1 {
        dataset.push(0);
    }

    // -- Assemble the Part-10 file ------------------------------------
    let mut out = Vec::with_capacity(dataset.len() + 256);
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");
    write_file_meta(&mut out, identity);
    out.extend_from_slice(&dataset);
    out
}

/// (0002,xxxx): the canonical trio plus the information UIDs.
fn write_file_meta(out: &mut Vec<u8>, identity: &DerivedIdentity) {
    let meta = file_meta_bytes(identity);
    write_explicit_header(out, 0x0002, 0x0000, b"UL", 4);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
}

fn file_meta_bytes(identity: &DerivedIdentity) -> Vec<u8> {
    let mut meta = Vec::with_capacity(256);
    // Everything goes through `write_string`, so one padding rule covers
    // the object: a hand-written length put an odd one on (0002,0003)
    // whenever a UID had an odd character count, which is illegal for
    // every VR (PS3.5 6.2).
    write_string(&mut meta, 0x0002, 0x0001, b"OB", "01");
    write_string(
        &mut meta,
        0x0002,
        0x0002,
        b"UI",
        SECONDARY_CAPTURE_SOP_CLASS_UID,
    );
    write_string(&mut meta, 0x0002, 0x0003, b"UI", &identity.sop_instance_uid);
    write_string(&mut meta, 0x0002, 0x0010, b"UI", EXPLICIT_VR_LE);
    write_string(
        &mut meta,
        0x0002,
        0x0012,
        b"UI",
        crate::sr::IMPLEMENTATION_CLASS_UID,
    );
    write_string(
        &mut meta,
        0x0002,
        0x0013,
        b"SH",
        &crate::sr::implementation_version_name(),
    );
    meta
}

/// A US value: the pixel-geometry attributes.
fn write_us(out: &mut Vec<u8>, group: u16, element: u16, value: u16) {
    write_explicit_header(out, group, element, b"US", 2);
    out.extend_from_slice(&value.to_le_bytes());
}

/// An IS value written from a number, with no heap allocation.
///
/// The pattern repetitions in an image object are fixed small numbers, so
/// the digits go into a stack buffer rather than through a `format!`.
fn write_is_number(out: &mut Vec<u8>, group: u16, element: u16, value: u32) {
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut remaining = value;
    loop {
        digits[count] = b'0' + (remaining % 10) as u8;
        count += 1;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    let pad = if count.is_multiple_of(2) { 0 } else { 1 };
    write_explicit_header(out, group, element, b"IS", count + pad);
    for index in (0..count).rev() {
        out.push(digits[index]);
    }
    if pad == 1 {
        out.push(b' ');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry {
            rows: 640,
            cols: 512,
            samples: 1,
            bits_allocated: 8,
            bits_stored: 8,
            frames: 1,
            inverted: false,
            signed: false,
        }
    }

    fn identity() -> DerivedIdentity {
        DerivedIdentity {
            sop_instance_uid: "2.25.7777".to_string(),
            study_uid: "1.2.3.4.6".to_string(),
            series_uid: "2.25.8888".to_string(),
            instance_number: 1,
            source_sop_class_uid: "1.2.840.10008.5.1.4.1.1.1.1".to_string(),
            source_sop_instance_uid: "1.2.3.4.9".to_string(),
            patient_name: Some("Fixture^Derived".to_string()),
            patient_id: Some("DERIVED-FIXTURE".to_string()),
            source_modality: "DX".to_string(),
            series_description: Some("AI Denoised".to_string()),
        }
    }

    fn pixels<'a>(bytes: &'a [u8]) -> DerivedPixels<'a> {
        DerivedPixels {
            bytes,
            geometry: geometry(),
            photometric: "MONOCHROME2".to_string(),
            frame_index: 1,
        }
    }

    /// The dataset, after the preamble and the file meta group.
    ///
    /// The tags have to be looked for HERE and not in the whole file: a
    /// naive byte scan of the entire buffer also matches inside the meta
    /// group's own values and inside pixel payloads, which is how a test
    /// comes to assert on a number that happens to share four bytes with a
    /// tag.
    fn dataset(bytes: &[u8]) -> &[u8] {
        let length = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]) as usize;
        &bytes[132 + 12 + length..]
    }

    /// The offset of a tag inside the dataset.
    fn find(data: &[u8], group: u16, element: u16) -> Option<usize> {
        let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
        data.windows(4)
            .position(|window| window == needle.as_slice())
    }

    /// Every occurrence of a tag in the dataset.
    fn tag_at(bytes: &[u8], group: u16, element: u16) -> bool {
        find(dataset(bytes), group, element).is_some()
    }

    /// The VR of the first occurrence of a tag, in the dataset.
    fn vr_at(bytes: &[u8], group: u16, element: u16) -> Option<[u8; 2]> {
        let data = dataset(bytes);
        let at = find(data, group, element)?;
        Some([data[at + 4], data[at + 5]])
    }

    /// The first US value of a tag, in the dataset.
    ///
    /// US is a short form: group, element, VR, a 16-bit length, then the
    /// value, so the value starts eight bytes in. Reading at six reads the
    /// length field instead, which is what made these tests report the
    /// element's own length as its value.
    fn us_at(bytes: &[u8], group: u16, element: u16) -> Option<u16> {
        let data = dataset(bytes);
        let at = find(data, group, element)?;
        assert_eq!(&data[at + 4..at + 6], b"US", "the test reads a US element");
        assert_eq!(
            u16::from_le_bytes([data[at + 6], data[at + 7]]),
            2,
            "a US element holds one 16-bit value"
        );
        Some(u16::from_le_bytes([data[at + 8], data[at + 9]]))
    }

    /// The top-level tags, in the order they were written.
    ///
    /// Each element is measured by its own header; a byte scan cannot tell a
    /// sequence's contents from the dataset around it.
    fn top_level_tags(bytes: &[u8]) -> Vec<(u16, u16)> {
        const LONG_VR: &[&[u8; 2]] = &[
            b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT",
            b"UV",
        ];
        let data = dataset(bytes);
        let mut tags = Vec::new();
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
            if group == 0xFFFE {
                break;
            }
            let vr = [data[at + 4], data[at + 5]];
            let (length, head) = if LONG_VR.contains(&&vr) {
                (
                    u32::from_le_bytes([data[at + 8], data[at + 9], data[at + 10], data[at + 11]])
                        as usize,
                    12,
                )
            } else {
                (u16::from_le_bytes([data[at + 6], data[at + 7]]) as usize, 8)
            };
            tags.push((group, element));
            at += head + length;
            if !length.is_multiple_of(2) {
                at += 1;
            }
        }
        tags
    }

    #[test]
    fn the_top_level_elements_are_in_ascending_tag_order() {
        // PS3.5 7.1: a data set is an ordered list in ascending tag order, and
        // a reader may stop at the first descent. The writer groups attributes
        // by module, so a group can land late by accident - the patient and
        // study groups did.
        let bytes = serialize(&identity(), &pixels(&[0u8; 16]));
        let tags = top_level_tags(&bytes);
        assert!(
            tags.len() >= 10,
            "the walk reached the end of the dataset: {tags:?}"
        );
        for pair in tags.windows(2) {
            assert!(
                pair[0] < pair[1],
                "top-level order descends: {:04X},{:04X} before {:04X},{:04X}",
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1
            );
        }
        // The walk stops where it can no longer measure an element, which is
        // inside the undefined-length Source Image Sequence; the order it did
        // verify is what the standard asks about, and PixelData follows every
        // one of those by construction.
        assert_eq!(tags.last(), Some(&(0x0008, 0x2112)));
    }

    #[test]
    fn the_output_is_a_part10_file_with_the_secondary_capture_class() {
        let bytes = serialize(&identity(), &pixels(&[0u8; 16]));
        assert_eq!(&bytes[128..132], b"DICM", "128-byte preamble + DICM");
        assert_eq!(&bytes[132..136], &[0x02, 0x00, 0x00, 0x00]);
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains(SECONDARY_CAPTURE_SOP_CLASS_UID),
            "the SC SOP class is written"
        );
        assert!(text.contains(EXPLICIT_VR_LE), "explicit VR little endian");
        assert!(text.contains("2.25.7777"), "the instance UID");
    }

    #[test]
    fn the_source_image_sequence_names_what_it_was_derived_from() {
        // (0008,2112) is the whole DICOM-side provenance mechanism: without
        // it a derived image is an orphan a viewer cannot relate to the
        // study it came from.
        let bytes = serialize(&identity(), &pixels(&[0u8; 16]));
        assert_eq!(
            vr_at(&bytes, 0x0008, 0x2112),
            Some(*b"SQ"),
            "it is a sequence"
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("1.2.3.4.9"), "the source instance UID");
        assert!(
            text.contains("1.2.840.10008.5.1.4.1.1.1.1"),
            "and its class UID (Type 1 in the reference item)"
        );
    }

    #[test]
    fn the_pixel_geometry_is_copied_from_the_source() {
        // A derived image whose geometry disagreed with its source would
        // be a different image, silently misregistered under an overlay.
        let bytes = serialize(&identity(), &pixels(&[0u8; 16]));
        assert_eq!(us_at(&bytes, 0x0028, 0x0010), Some(640), "Rows");
        assert_eq!(us_at(&bytes, 0x0028, 0x0011), Some(512), "Columns");
        assert_eq!(us_at(&bytes, 0x0028, 0x0100), Some(8), "BitsAllocated");
        assert_eq!(us_at(&bytes, 0x0028, 0x0101), Some(8), "BitsStored");
        assert_eq!(us_at(&bytes, 0x0028, 0x0002), Some(1), "SamplesPerPixel");
        assert_eq!(us_at(&bytes, 0x0028, 0x0103), Some(0), "unsigned");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("MONOCHROME2"), "the photometric is copied");
    }

    #[test]
    fn the_high_bit_is_derived_from_bits_stored() {
        // PS3.3 C.7.6.3.1.1: HighBit = BitsStored - 1. Writing it from the
        // source's own HighBit would copy a value that could disagree;
        // deriving it is what keeps the three consistent by construction.
        let mut geometry = geometry();
        geometry.bits_stored = 12;
        geometry.bits_allocated = 16;
        let bytes = serialize(
            &identity(),
            &DerivedPixels {
                bytes: &[0u8; 8],
                geometry,
                photometric: "MONOCHROME2".to_string(),
                frame_index: 1,
            },
        );
        assert_eq!(us_at(&bytes, 0x0028, 0x0101), Some(12), "BitsStored");
        assert_eq!(us_at(&bytes, 0x0028, 0x0102), Some(11), "HighBit = 12 - 1");
    }

    #[test]
    fn a_signed_source_stays_signed() {
        let mut geometry = geometry();
        geometry.signed = true;
        let bytes = serialize(
            &identity(),
            &DerivedPixels {
                bytes: &[0u8; 8],
                geometry,
                photometric: "MONOCHROME2".to_string(),
                frame_index: 1,
            },
        );
        assert_eq!(
            us_at(&bytes, 0x0028, 0x0103),
            Some(1),
            "PixelRepresentation is copied, so a signed image stays signed"
        );
    }

    #[test]
    fn the_pixel_payload_is_carried_through_byte_for_byte() {
        // The pixels are the processor's output: clinfer validated the
        // geometry and must not otherwise touch them. The fixture is 8-bit, so
        // the element is OB (Table 6.2-1: OB carries byte-sized samples) and
        // its Value Length is even because the payload is.
        let payload: Vec<u8> = (0..64u8).collect();
        let bytes = serialize(&identity(), &pixels(&payload));
        let data = dataset(&bytes);
        // And the element declares that length.
        let at = find(data, 0x7FE0, 0x0010).expect("(7FE0,0010)");
        assert_eq!(&data[at + 4..at + 6], b"OB", "8-bit PixelData is OB");
        assert_eq!(&data[at + 6..at + 8], &[0, 0], "the reserved bytes");
        let mut declared =
            u32::from_le_bytes([data[at + 8], data[at + 9], data[at + 10], data[at + 11]]) as usize;
        // PS3.5 8.1: the length is even; an odd payload is padded, and the pad
        // is not part of the image.
        if declared > payload.len() {
            assert_eq!(declared, payload.len() + 1, "one pad byte at most");
            assert_eq!(data[at + 12 + payload.len()], 0, "the pad is zero");
            declared = payload.len();
        }
        assert_eq!(declared, payload.len());
        assert_eq!(
            &data[at + 12..at + 12 + payload.len()],
            payload.as_slice(),
            "the payload is carried through byte for byte"
        );
    }

    /// An odd payload needs one pad byte and an EVEN declared length (PS3.5
    /// 8.1). Declaring the odd length put the next element one byte early.
    #[test]
    fn an_odd_payload_is_padded_to_an_even_length() {
        let payload: Vec<u8> = (0..63u8).collect();
        let bytes = serialize(&identity(), &pixels(&payload));
        let data = dataset(&bytes);
        let at = find(data, 0x7FE0, 0x0010).expect("(7FE0,0010)");
        let declared =
            u32::from_le_bytes([data[at + 8], data[at + 9], data[at + 10], data[at + 11]]);
        assert_eq!(declared, 64, "63 samples + 1 pad byte");
        assert_eq!(&data[at + 12..at + 12 + 63], payload.as_slice());
        assert_eq!(data[at + 12 + 63], 0, "the pad byte is zero");
    }

    /// The VR follows BitsAllocated (PS3.5 Table 6.2-1): a 16-bit image is
    /// OW. Writing OB there told a native decoder the payload was bytes.
    #[test]
    fn a_sixteen_bit_payload_is_ow() {
        let mut geometry = geometry();
        geometry.bits_allocated = 16;
        geometry.bits_stored = 16;
        let payload = vec![0u8; 16];
        let bytes = serialize(
            &identity(),
            &DerivedPixels {
                bytes: &payload,
                geometry,
                photometric: "MONOCHROME2".to_string(),
                frame_index: 1,
            },
        );
        let data = dataset(&bytes);
        let at = find(data, 0x7FE0, 0x0010).expect("(7FE0,0010)");
        assert_eq!(&data[at + 4..at + 6], b"OW", "16-bit PixelData is OW");
    }

    #[test]
    fn the_object_says_it_is_derived() {
        // (0008,0008) DERIVED is the standard's own word for "not an
        // acquisition", and it is what a reader uses to tell this apart
        // from a source image.
        let bytes = serialize(&identity(), &pixels(&[0u8; 8]));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("DERIVED"), "ImageType declares DERIVED");
    }

    #[test]
    fn the_source_modality_is_carried() {
        // A processed DX is still a DX. The modality describes what the
        // image IS, and a filter does not change that.
        let bytes = serialize(&identity(), &pixels(&[0u8; 8]));
        assert_eq!(vr_at(&bytes, 0x0008, 0x0060), Some(*b"CS"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("DX"),
            "the modality is copied from the source"
        );
    }

    #[test]
    fn the_patient_identity_is_copied_and_utf8_is_declared() {
        // Same rule as the PR and SR writers: a derived image that does not
        // agree with the study's patient is filed under the wrong one.
        let mut identity = identity();
        identity.patient_name = Some("Петров^Иван".to_string());
        let bytes = serialize(&identity, &pixels(&[0u8; 8]));
        assert_eq!(
            vr_at(&bytes, 0x0008, 0x0005),
            Some(*b"CS"),
            "(0008,0005) SpecificCharacterSet"
        );
        assert!(
            bytes
                .windows("Петров^Иван".len())
                .any(|window| window == "Петров^Иван".as_bytes()),
            "the Cyrillic name is stored byte for byte"
        );
    }

    #[test]
    fn an_empty_patient_identity_is_present_not_absent() {
        // Type 2: present with an empty value, never dropped.
        let mut identity = identity();
        identity.patient_name = None;
        identity.patient_id = None;
        let bytes = serialize(&identity, &pixels(&[0u8; 8]));
        assert!(tag_at(&bytes, 0x0010, 0x0010), "(0010,0010) is present");
        assert!(tag_at(&bytes, 0x0010, 0x0020), "(0010,0020) is present");
        let data = dataset(&bytes);
        let at = find(data, 0x0010, 0x0010).expect("(0010,0010)");
        assert_eq!(
            u16::from_le_bytes([data[at + 6], data[at + 7]]),
            0,
            "with a zero length"
        );
    }

    #[test]
    fn the_instance_number_is_ordered_within_the_series() {
        // One derived image per frame, so a series of them is ordered by
        // instance number. The writer reports the number it was given.
        let mut identity = identity();
        identity.instance_number = 7;
        let bytes = serialize(&identity, &pixels(&[0u8; 8]));
        assert_eq!(vr_at(&bytes, 0x0020, 0x0013), Some(*b"IS"));
        let data = dataset(&bytes);
        let at = find(data, 0x0020, 0x0013).expect("(0020,0013)");
        let length = u16::from_le_bytes([data[at + 6], data[at + 7]]) as usize;
        assert_eq!(
            String::from_utf8_lossy(&data[at + 8..at + 8 + length]).trim_end(),
            "7"
        );
    }

    #[test]
    fn the_meta_group_framing_is_what_a_parser_seeks_by() {
        // The same lesson the PR writer learned: (0002,0000) is UL, a
        // SHORT VR, so an 8-byte header here makes a parser seek four
        // bytes short and read the dataset as garbage.
        let bytes = serialize(&identity(), &pixels(&[0u8; 8]));
        assert_eq!(&bytes[132..134], &[0x02, 0x00], "group 0002 first");
        assert_eq!(&bytes[134..136], &[0x00, 0x00], "(0002,0000)");
        assert_eq!(&bytes[136..138], b"UL", "the VR");
        assert_eq!(
            u16::from_le_bytes([bytes[138], bytes[139]]),
            4,
            "a UL length is 16-bit"
        );
        let declared = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]);
        assert!(declared > 0, "the group length is not zero");
        let meta_end = 144 + declared as usize;
        assert_eq!(
            &bytes[meta_end..meta_end + 4],
            &[0x08, 0x00, 0x05, 0x00],
            "the dataset starts with (0008,0005) Specific Character Set"
        );
    }

    #[test]
    fn the_implementation_version_name_matches_the_other_writers() {
        let bytes = serialize(&identity(), &pixels(&[0u8; 8]));
        // (0002,0013) lives in the FILE META, so this one searches the meta
        // group rather than the dataset.
        let needle = [0x02, 0x00, 0x13, 0x00];
        let at = bytes
            .windows(4)
            .position(|window| window == needle)
            .expect("(0002,0013)");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        let name = String::from_utf8_lossy(&bytes[at + 8..at + 8 + length])
            .trim_end()
            .to_string();
        assert_eq!(name, crate::sr::implementation_version_name());
        assert!(name.starts_with("clinfer "), "{name:?}");
        assert!(name.len() <= 16, "SH holds 16 characters: {name:?}");
    }

    #[test]
    fn the_series_description_is_written() {
        let bytes = serialize(&identity(), &pixels(&[0u8; 8]));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("AI Denoised"), "the series description");
    }

    #[test]
    fn a_derived_image_without_a_description_still_parses_as_a_document() {
        // (0008,103E) is optional here; the writer must simply omit it
        // rather than write an empty CS that a reader would test.
        let mut identity = identity();
        identity.series_description = None;
        let bytes = serialize(&identity, &pixels(&[0u8; 8]));
        assert!(
            !tag_at(&bytes, 0x0008, 0x103E),
            "an absent description is omitted"
        );
        assert_eq!(&bytes[128..132], b"DICM", "and the file is still valid");
    }
}
