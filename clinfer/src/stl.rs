// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The surface writer: one binary STL mesh as a PS3.3 A.85.1 object.
//!
//! # What it writes
//!
//! The conformance statement (section 5.5) fixes the target: SOP Class
//! `...104.3`, Modality "M3D" (the enumerated value for a manufacturing
//! model, A.85.1.4.3), MIME Type "model/stl" (A.85.1.4.2), and the mesh
//! bytes in Encapsulated Document (0042,0011).
//!
//! # Why the file bytes are copied rather than re-encoded
//!
//! A.85.1 stores an encapsulated document: the mesh is an opaque byte
//! stream that this writer is not asked to understand. So the STL file is
//! validated once (its header and triangle count have to agree with its
//! length - the caller does that before anything is stored) and then moved
//! onto the wire verbatim. Re-encoding triangles here would mean this
//! writer and the processor could disagree about float rounding, and the
//! bytes a viewer loads would not be the bytes the processor produced.
//!
//! # Frame of Reference
//!
//! Frame of Reference UID (0020,0052) is Type 1 in A.85.1: it identifies
//! the origin and axes implicit in the mesh data (A.85.1.4.1 Note 2). The
//! source images' value is copied when they define one. When they do not,
//! an own UID is derived from the same arc as the object's identity: a
//! mesh has a coordinate system whether or not its source did, and an
//! absent Type 1 attribute would be a malformed object rather than an
//! honest one. See the ABI README for what that UID means.

use crate::pr::{
    now_stamp, write_cs, write_da, write_explicit_header, write_is, write_lo, write_pn, write_sh,
    write_string, write_tm, write_ui,
};
use crate::sr::IMPLEMENTATION_CLASS_UID;
use crate::sr::implementation_version_name;

/// Encapsulated STL Storage (PS3.3 A.85.1).
pub const STL_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.104.3";

/// Explicit VR Little Endian, the transfer syntax every writer emits.
pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";

/// The MIME type A.85.1.4.2 enumerates for an STL document.
pub const STL_MIME_TYPE: &str = "model/stl";

/// The modality A.85.1.4.3 enumerates for a manufacturing model.
///
/// "M3D", not "STL": the modality says what kind of object this is, and
/// A.85.1.4.3 has no "STL" value.
pub const STL_MODALITY: &str = "M3D";

/// The character set the copied patient identity is declared in.
pub const SPECIFIC_CHARACTER_SET: &str = crate::sr::SPECIFIC_CHARACTER_SET;

/// The length of a binary STL header, in bytes.
///
/// The 1989 specification fixes this at 80 bytes, followed by a u32
/// triangle count and then 50 bytes per triangle.
pub const HEADER_BYTES: usize = 84;

/// The length of one triangle record, in bytes: 12 floats plus a u16.
pub const TRIANGLE_BYTES: usize = 50;

/// Everything the caller supplies that this writer cannot derive.
#[derive(Debug, Clone)]
pub struct StlIdentity {
    /// (0008,0018) of the object itself.
    pub sop_instance_uid: String,
    /// (0020,000D) of the study the mesh belongs to.
    pub study_uid: String,
    /// (0020,000E) of the series it is stored in.
    pub series_uid: String,
    /// (0020,0052). Type 1 in A.85.1, so this is always written: the
    /// caller passes the source images' value when they define one and a
    /// derived UID when they do not.
    pub frame_of_reference_uid: String,
    /// (0010,0010) copied from the study, as the PR, SR and SEG writers do.
    pub patient_name: Option<String>,
    /// (0010,0020) copied from the study.
    pub patient_id: Option<String>,
    /// (0020,0010) copied from the study, Type 2.
    pub study_id: Option<String>,
    /// (0008,0050) copied from the study, Type 2.
    pub accession_number: Option<String>,
    /// (0008,103E) from the rule, when it names one.
    pub series_description: Option<String>,
    /// The STL file's bytes, copied verbatim.
    pub document: Vec<u8>,
}

/// The number of bytes a binary STL of `triangles` triangles occupies.
///
/// The caller checks the file against this before anything is stored: a
/// file whose length disagrees with its own header is a truncated or
/// concatenated mesh, and storing it would put a document in the study
/// that no viewer can load.
#[must_use]
pub fn document_bytes(triangles: u32) -> u64 {
    HEADER_BYTES as u64 + u64::from(triangles) * TRIANGLE_BYTES as u64
}

/// The triangle count a binary STL declares, or `None` when the file is
/// too short to declare one.
///
/// This is a read of the file's own header, not a guess: offset 80 holds a
/// little-endian u32. A file shorter than 84 bytes has no count and cannot
/// be an STL at all.
#[must_use]
pub fn declared_triangles(bytes: &[u8]) -> Option<u32> {
    let header = bytes.get(80..84)?;
    Some(u32::from_le_bytes([
        header[0], header[1], header[2], header[3],
    ]))
}

/// Whether a byte stream is a binary STL whose header agrees with its
/// length.
///
/// Both halves are required. A file that is long enough but whose declared
/// count does not match is the case that matters: it is what a partial
/// write or a concatenation looks like, and it is exactly what a viewer
/// trips over.
#[must_use]
pub fn is_well_formed(bytes: &[u8]) -> bool {
    declared_triangles(bytes).is_some_and(|count| document_bytes(count) == bytes.len() as u64)
}

/// Serialize one Encapsulated STL as a Part-10 file.
#[must_use]
pub fn serialize(identity: &StlIdentity) -> Vec<u8> {
    let mut dataset = Vec::with_capacity(identity.document.len() + 1024);

    // -- What this object is ------------------------------------------
    // Ascending tag order, and (0008,0012)/(0008,0013) are SOP Common's
    // Instance Creation Date/Time (C.12.1), NOT the study's. The writer used
    // to put the implementation version name there and the creation stamp
    // on (0008,0021)/(0008,0031), which are Series Date/Time: a reader then
    // saw "clinfer 0.3.0-alpha" as a DA, and the object claimed a series date
    // it never had.
    let (date, time) = now_stamp();
    write_cs(&mut dataset, 0x0008, 0x0005, SPECIFIC_CHARACTER_SET);
    write_cs(&mut dataset, 0x0008, 0x0008, "DERIVED");
    write_da(&mut dataset, 0x0008, 0x0012, &date);
    write_tm(&mut dataset, 0x0008, 0x0013, &time);
    write_ui(&mut dataset, 0x0008, 0x0016, STL_SOP_CLASS_UID);
    write_ui(&mut dataset, 0x0008, 0x0018, &identity.sop_instance_uid);
    // (0008,001B) Original Specialized SOP Class UID: the same class, so a
    // reader that indexed by either name finds one object.
    write_ui(&mut dataset, 0x0008, 0x001B, STL_SOP_CLASS_UID);
    // (0008,0021)/(0008,0031) Series Date/Time: this object is a series of
    // its own, created at the same instant.
    write_da(&mut dataset, 0x0008, 0x0021, &date);
    write_tm(&mut dataset, 0x0008, 0x0031, &time);
    write_sh(
        &mut dataset,
        0x0008,
        0x0050,
        identity.accession_number.as_deref().unwrap_or(""),
    );
    // "M3D" is the value A.85.1.4.3 enumerates for an Encapsulated STL.
    write_cs(&mut dataset, 0x0008, 0x0060, STL_MODALITY);
    write_lo(&mut dataset, 0x0008, 0x0070, crate::sr::MANUFACTURER);
    if let Some(description) = &identity.series_description {
        write_lo(&mut dataset, 0x0008, 0x103E, description);
    }
    // (0008,1090) Manufacturer's Model Name: Enhanced General Equipment
    // is Type 1 in A.85.1, so it is always written. It names what produced
    // the mesh, which on this path is the processor that clinfer ran.
    write_lo(&mut dataset, 0x0008, 0x1090, crate::sr::MANUFACTURER);

    // -- Patient and study identity ----------------------------------
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
    write_ui(&mut dataset, 0x0020, 0x000D, &identity.study_uid);
    write_ui(&mut dataset, 0x0020, 0x000E, &identity.series_uid);
    write_sh(
        &mut dataset,
        0x0020,
        0x0010,
        identity.study_id.as_deref().unwrap_or(""),
    );
    write_is(&mut dataset, 0x0020, 0x0011, "1");
    write_is(&mut dataset, 0x0020, 0x0013, "1");

    // -- Frame of Reference (C.7.4.1) --------------------------------
    // Type 1 in A.85.1 (A.85.1.4.1 Note 2): the mesh's axes are relative
    // to this, so it is always written. The caller supplies the source
    // images' value when they define one.
    write_ui(
        &mut dataset,
        0x0020,
        0x0052,
        &identity.frame_of_reference_uid,
    );

    // -- Encapsulated Document (C.24.2) ------------------------------
    // (0042,0011) is the STL byte stream, copied verbatim as OB with a
    // defined length: the size is known before the element is written, and
    // the 1989 format is not encapsulated in a DICOM sequence.
    write_explicit_header(&mut dataset, 0x0042, 0x0011, b"OB", identity.document.len());
    dataset.extend_from_slice(&identity.document);
    // (0042,0012) MIME Type of Encapsulated Document: enumerated by
    // A.85.1.4.2, and the value is what tells a viewer which external
    // application can open the payload.
    write_lo(&mut dataset, 0x0042, 0x0012, STL_MIME_TYPE);

    finish(identity, dataset)
}

/// Wrap the dataset in the Part-10 file meta group.
fn finish(identity: &StlIdentity, dataset: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(dataset.len() + 512);
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");
    let mut meta = Vec::with_capacity(256);
    write_string(&mut meta, 0x0002, 0x0001, b"OB", "01");
    write_string(&mut meta, 0x0002, 0x0002, b"UI", STL_SOP_CLASS_UID);
    write_string(&mut meta, 0x0002, 0x0003, b"UI", &identity.sop_instance_uid);
    write_string(&mut meta, 0x0002, 0x0010, b"UI", EXPLICIT_VR_LE);
    write_string(&mut meta, 0x0002, 0x0012, b"UI", IMPLEMENTATION_CLASS_UID);
    write_string(
        &mut meta,
        0x0002,
        0x0013,
        b"SH",
        &implementation_version_name(),
    );
    write_explicit_header(&mut out, 0x0002, 0x0000, b"UL", 4);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(&dataset);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal well-formed binary STL: an 80-byte header, a count, and
    /// `triangles` 50-byte records of zeros.
    fn stl(triangles: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 80];
        bytes[0..5].copy_from_slice(b"mesh!");
        bytes.extend_from_slice(&triangles.to_le_bytes());
        bytes.extend(std::iter::repeat_n(
            0u8,
            triangles as usize * TRIANGLE_BYTES,
        ));
        bytes
    }

    fn identity(document: Vec<u8>) -> StlIdentity {
        StlIdentity {
            sop_instance_uid: "1.2.3.4.5".to_string(),
            study_uid: "1.2.3.4.6".to_string(),
            series_uid: "1.2.3.4.7".to_string(),
            frame_of_reference_uid: "1.2.3.4.8".to_string(),
            patient_name: Some("Fixture^STL".to_string()),
            patient_id: Some("STL-FIXTURE".to_string()),
            study_id: Some("STUDY-1".to_string()),
            accession_number: Some("ACC-1".to_string()),
            series_description: Some("Surface".to_string()),
            document,
        }
    }

    fn find(bytes: &[u8], group: u16, element: u16) -> Option<usize> {
        let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
        bytes
            .windows(4)
            .position(|window| window == needle.as_slice())
    }

    /// The value bytes of one element, located by tag.
    ///
    /// Only the short-VR form is handled, which is every element this
    /// writer emits except the document itself. UI values are padded with
    /// NUL and every other string VR with a space (PS3.5 6.2), so both are
    /// stripped - a helper that only trimmed whitespace would report a
    /// padded UID as a different UID.
    fn value(bytes: &[u8], group: u16, element: u16) -> Option<String> {
        let at = find(bytes, group, element)?;
        let vr = &bytes[at + 4..at + 6];
        let length = usize::from(u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]));
        let raw = bytes.get(at + 8..at + 8 + length)?;
        assert!(vr != b"OB" && vr != b"OW", "not a short-VR element");
        Some(
            String::from_utf8_lossy(raw)
                .trim_end_matches(['\0', ' '])
                .to_string(),
        )
    }

    /// The (group, element) of every top-level element of the dataset, in
    /// file order, with the encapsulated document treated as one element.
    ///
    /// Walked rather than searched: the point is the ORDER, and a window
    /// search would find every byte pair that looks like a tag, including
    /// the ones inside values.
    fn top_level_tags(bytes: &[u8]) -> Vec<(u16, u16)> {
        // The Part-10 preamble and the file meta group come first; the
        // dataset starts after the meta group length the group announces.
        let mut at = 132usize;
        let group_length = usize::try_from(u32::from_le_bytes([
            bytes[at + 8],
            bytes[at + 9],
            bytes[at + 10],
            bytes[at + 11],
        ]))
        .expect("the meta group length");
        at += 12 + group_length;
        let mut out = Vec::new();
        while at + 8 <= bytes.len() {
            let group = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            let element = u16::from_le_bytes([bytes[at + 2], bytes[at + 3]]);
            let vr = &bytes[at + 4..at + 6];
            let long = matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN");
            let value_at;
            let length;
            if long {
                length = usize::try_from(u32::from_le_bytes([
                    bytes[at + 8],
                    bytes[at + 9],
                    bytes[at + 10],
                    bytes[at + 11],
                ]))
                .expect("a long-VR length");
                value_at = at + 12;
            } else {
                length = usize::from(u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]));
                value_at = at + 8;
            }
            out.push((group, element));
            at = value_at + length;
        }
        out
    }

    #[test]
    fn a_document_occupies_its_header_and_its_triangles() {
        assert_eq!(document_bytes(0), 84);
        assert_eq!(document_bytes(1), 134);
        assert_eq!(document_bytes(12), 684);
    }

    #[test]
    fn a_count_that_disagrees_with_the_length_is_not_well_formed() {
        // The declared count is read from the header, so a file one byte
        // short of its own promise is refused: that is what a partial
        // write looks like, and it is the case a viewer trips over.
        let mut bytes = stl(3);
        assert!(is_well_formed(&bytes));
        bytes.pop();
        assert!(!is_well_formed(&bytes));
        // And a file with no count at all cannot be well formed.
        assert!(!is_well_formed(&[0u8; 80]));
        assert!(!is_well_formed(&[]));
    }

    #[test]
    fn the_creation_stamp_is_sop_common_and_the_version_is_not_a_date() {
        // (0008,0012)/(0008,0013) are SOP Common's Instance Creation
        // Date/Time. (0008,0013) used to carry the implementation VERSION
        // NAME, which is a short string, not a TM: a reader that parsed it
        // as a time got nothing, and the object claimed a creation time it
        // never stated.
        let bytes = serialize(&identity(stl(1)));
        let date = value(&bytes, 0x0008, 0x0012).expect("(0008,0012)");
        assert_eq!(date.len(), 8, "instance creation date is YYYYMMDD: {date}");
        assert!(date.chars().all(|c| c.is_ascii_digit()), "{date}");
        let time = value(&bytes, 0x0008, 0x0013).expect("(0008,0013)");
        assert!(
            time.len() == 6 && time.chars().all(|c| c.is_ascii_digit()),
            "instance creation time is HHMMSS: {time}"
        );
        // The version name is not in the dataset at all: it belongs to the
        // file meta group (0002,0013), which `finish` writes. The dataset
        // is everything after the meta group.
        let meta_end = {
            let at = 132usize;
            let group_length = usize::try_from(u32::from_le_bytes([
                bytes[at + 8],
                bytes[at + 9],
                bytes[at + 10],
                bytes[at + 11],
            ]))
            .expect("the meta group length");
            at + 12 + group_length
        };
        assert!(
            !String::from_utf8_lossy(&bytes[meta_end..]).contains("clinfer 0."),
            "(0008,0013) carries a TIME, not the implementation version name"
        );
        // And the series pair carries the same instant, not an empty value.
        assert_eq!(
            value(&bytes, 0x0008, 0x0021).as_deref(),
            Some(date.as_str())
        );
        assert_eq!(
            value(&bytes, 0x0008, 0x0031).as_deref(),
            Some(time.as_str())
        );
    }

    #[test]
    fn the_top_level_elements_are_in_ascending_tag_order() {
        // PS3.5 7.1: a data set's elements are in ascending tag order. The
        // writer used to emit (0008,0050) and the patient rows before
        // (0008,0016), and (0020,000D) before (0020,0010), so a strict
        // reader could stop early.
        let bytes = serialize(&identity(stl(1)));
        let order = top_level_tags(&bytes);
        assert!(order.len() > 10, "the object has a body: {order:?}");
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted, "top-level tags ascend");
        // Spot-check that the list really is the dataset's: the document
        // element (0042,0011) is the last thing before the file meta.
        assert!(
            order.contains(&(0x0042, 0x0011)),
            "the encapsulated document is in the list: {order:?}"
        );
    }

    #[test]
    fn the_declared_triangle_count_comes_from_offset_eighty() {
        let bytes = stl(7);
        assert_eq!(declared_triangles(&bytes), Some(7));
        // A count that is only in the right place is still read: this is a
        // header read, not a validation, and the two are checked apart so
        // a mismatch can be named.
        let mut lying = vec![0u8; 84];
        lying[80..84].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(declared_triangles(&lying), Some(99));
        assert!(!is_well_formed(&lying));
    }

    #[test]
    fn the_document_is_copied_verbatim() {
        let document = stl(2);
        let bytes = serialize(&identity(document.clone()));
        let at = find(&bytes, 0x0042, 0x0011).expect("the document element is written");
        assert_eq!(&bytes[at + 4..at + 6], b"OB");
        let length =
            u32::from_le_bytes([bytes[at + 8], bytes[at + 9], bytes[at + 10], bytes[at + 11]])
                as usize;
        assert_eq!(length, document.len(), "the length is the file's own");
        // Byte for byte: a re-encode would round floats and this is the
        // one thing the encoder must not do.
        assert_eq!(&bytes[at + 12..at + 12 + length], document.as_slice());
    }

    #[test]
    fn the_modality_is_m3d_and_the_mime_type_is_model_stl() {
        let bytes = serialize(&identity(stl(1)));
        // "M3D", not "STL": A.85.1.4.3 enumerates no "STL" value, and a
        // modality that says "STL" would be a value no reader expects.
        assert_eq!(value(&bytes, 0x0008, 0x0060).as_deref(), Some("M3D"));
        assert_eq!(value(&bytes, 0x0042, 0x0012).as_deref(), Some("model/stl"));
    }

    #[test]
    fn the_sop_class_is_the_encapsulated_stl_storage_class() {
        let bytes = serialize(&identity(stl(1)));
        assert_eq!(
            value(&bytes, 0x0008, 0x0016).as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.104.3")
        );
        // (0008,001B) Original Specialized SOP Class UID: the same class,
        // so a reader that indexed by either name finds one object.
        assert_eq!(
            value(&bytes, 0x0008, 0x001B).as_deref(),
            Some(STL_SOP_CLASS_UID)
        );
        // And the FILE META group names the same class. A PS3.10 reader
        // opens the meta group first and decides what the object is from
        // (0002,0002); a file whose meta named a different class would be
        // rejected before the dataset was ever read.
        assert!(
            bytes[128..].starts_with(b"DICM"),
            "the Part-10 preamble is present"
        );
        assert_eq!(
            value(&bytes, 0x0002, 0x0002).as_deref(),
            Some(STL_SOP_CLASS_UID),
            "the file meta names the STL storage class"
        );
    }

    #[test]
    fn the_frame_of_reference_is_written_because_it_is_type_one() {
        // A.85.1 makes (0020,0052) Type 1: a mesh's axes are relative to
        // something, so an absent tag would be a malformed object.
        let bytes = serialize(&identity(stl(1)));
        assert_eq!(value(&bytes, 0x0020, 0x0052).as_deref(), Some("1.2.3.4.8"));
    }

    #[test]
    fn the_equipment_names_this_writer() {
        let bytes = serialize(&identity(stl(1)));
        assert_eq!(value(&bytes, 0x0008, 0x0070).as_deref(), Some("clinfer"));
        // (0008,1090) is Type 1 in Enhanced General Equipment: written,
        // not omitted, because the module requires it.
        assert_eq!(value(&bytes, 0x0008, 0x1090).as_deref(), Some("clinfer"));
    }

    #[test]
    fn the_dataset_is_inside_the_part_ten_wrapper() {
        let document = stl(4);
        let bytes = serialize(&identity(document.clone()));
        let preamble = &bytes[..132];
        assert!(preamble[128..].starts_with(b"DICM"), "DICM at offset 128");
        // The document has to survive the wrapper: it is the payload, and
        // the meta group is a header in front of it, not a transformation.
        assert!(
            bytes
                .windows(document.len())
                .any(|window| window == document.as_slice()),
            "the document is present in full"
        );
        assert!(bytes.len() > document.len() + 132);
    }
}
