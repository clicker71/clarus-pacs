// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The Segmentation writer: per-frame binary masks as a PS3.3 A.51 object.
//!
//! # What it writes
//!
//! The BINARY form only. The conformance statement (section 5.4) fixes the
//! target: SOP Class `...66.4`, Segmentation Type "BINARY", Bits Allocated 1.
//! FRACTIONAL is reserved for a later build and is refused rather than
//! approximated.
//!
//! # One frame per segment (C.8.20.3.1)
//!
//! Segment Identification Sequence (0062,000A) may contain ONLY A SINGLE
//! ITEM and identifies "this Frame": a BINARY frame is therefore ONE bit
//! plane of ONE segment. A segmentation with N segments over F source
//! images carries N x F frames, and every per-frame functional group names
//! its segment via Referenced Segment Number (0062,000B).
//!
//! The ABI packs every segment's plane into one file per source frame; the
//! worker splits each file into per-segment frames before this writer sees
//! them, so `serialize` only ever records single-plane frames.
//!
//! Segments Overlap (0062,0013) is written only when the producer declares
//! it ("YES" for nested masks): a writer that guessed "NO" for masks that
//! do overlap would be lying, and absence tells a reader to check every
//! pixel.
//!
//! # Why the masks are copied rather than packed here
//!
//! Bits Allocated 1 means the pixel data is bit-packed, and the layout the
//! standard requires (PS3.3 C.8.20.2: rows padded to byte boundaries, bit k
//! of a byte is pixel k) is exactly what `numpy.packbits(axis=1)` produces
//! for a 2-D mask. So the processor writes the packed bytes and this writer
//! copies them VERBATIM into the Pixel Data element.
//!
//! That is a deliberate choice over unpacking and repacking in Rust: it
//! keeps the payload a single memcpy, it removes any chance of this writer
//! disagreeing with the processor about bit order, and it means the byte
//! count the processor declares is the byte count that has to arrive - a
//! check the caller makes before anything is stored.
//!
//! # Sizing
//!
//! One bit plane is `rows * ceil(cols / 8)` bytes: rows are padded to byte
//! boundaries, so a width that is not a multiple of 8 costs a whole extra
//! byte on every row rather than a fraction of one across the frame. That
//! is exactly what `numpy.packbits(..., axis=1)` produces for a 2-D mask.
//!
//! A written frame is exactly one plane ([`plane_bytes`]); an ABI mask
//! FILE is `segments.len() * plane_bytes` ([`frame_bytes`]), one plane per
//! segment, one after the other.

use crate::abi::schema::{Code, Segment};
use crate::pixels::Geometry;
use crate::pr::{
    now_stamp, write_cs, write_da, write_ds, write_explicit_header, write_is, write_item_end,
    write_item_start, write_lo, write_pn, write_sequence_end, write_sequence_start, write_sh,
    write_string, write_tm, write_ui,
};
use crate::sr::IMPLEMENTATION_CLASS_UID;
use crate::sr::implementation_version_name;

/// A workitem code as the ABI's `Code`, so the ONE code-sequence writer in
/// this file serves both the segments' classification codes and the three
/// workitem codes AIW-I Appendix V maps into this object ((0040,4018) ->
/// (0040,0008), (0032,1064) and (0040,100A)).
///
/// The two types carry the same three parts under different names - the ABI
/// calls the Code Value `code`, the workitem reader calls it `value` - and
/// converting here keeps a second sequence writer from drifting away from
/// the first.
fn as_abi_code(code: &crate::dicomweb::WorkitemCode) -> Code {
    Code {
        code: code.value.clone(),
        scheme: code.scheme.clone(),
        meaning: code.meaning.clone(),
    }
}

/// BINARY and FRACTIONAL Segmentation Storage (PS3.3 A.51).
pub const SEG_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.66.4";

/// Explicit VR Little Endian, the transfer syntax every writer emits.
pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";

/// The character set the copied patient identity is declared in.
pub const SPECIFIC_CHARACTER_SET: &str = crate::sr::SPECIFIC_CHARACTER_SET;

/// (0020,0011) Series Number of the series this writer emits.
///
/// The result series in one study are numbered apart, because a viewer
/// lists a study's series BY NUMBER and two series that share one are two
/// rows a reader cannot tell apart. The scheme across the writers is:
///
/// ```text
///   1  the source acquisition series, which the caller reproduces
///   2  the SEG writer's series        (this constant)
///   3  the RTSTRUCT writer's series   (rtstruct::SERIES_NUMBER)
/// ```
///
/// 1 is the source by convention and is fixed, not defaulted: the CT that
/// was sent always carries it. 2 and 3 then separate the two derived
/// series, which is the collision this constant exists to remove - both
/// writers used to write 1, so a segmentation and a structure set derived
/// from one study looked like two copies of the source series.
pub const SERIES_NUMBER: &str = "2";

/// One source image a frame's segmentation was derived from.
///
/// Not `Eq`: the plane position is a float, and claiming an equality this
/// type cannot honour would be a promise the derive does not keep.
#[derive(Debug, Clone, PartialEq)]
pub struct SegSource {
    /// (0008,1150) of the source image.
    pub sop_class_uid: String,
    /// (0008,1155) of the source image.
    pub sop_instance_uid: String,
    /// (0020,000E) of the series the source image belongs to.
    ///
    /// (0008,1115) Referenced Series Sequence groups its instances BY
    /// SERIES (PS3.3 C.7.6.1), so the series UID is what carries the frame
    /// back to the images it was derived from. It is carried per source
    /// rather than once beside the identity because a study's frames can
    /// span series, and two series cannot share one reference item.
    pub series_uid: String,
    /// The source frame's Image Position Patient, when it declares one.
    ///
    /// (0020,9113) Plane Position per frame is a copy of this: a mask
    /// frame covers the image that position names, so the position is what
    /// lets a reader place the mask in the study's own coordinates.
    pub position: Option<[f64; 3]>,
    /// The source frame's Image Orientation Patient, when it declares one.
    ///
    /// (0020,9116) Plane Orientation (Patient) in the SHARED functional
    /// group is a copy of this: the position says where the mask sits, and
    /// the orientation says which way its rows and columns point. A mask
    /// with a position but no orientation cannot be rotated onto the image
    /// plane it covers.
    pub orientation: Option<[f64; 6]>,
}

/// One frame of a BINARY segmentation: a single bit plane, one segment.
///
/// C.8.20.3.1 (Table C.8.20-3) says Segment Identification Sequence
/// (0062,000A) "identifies the characteristics of this Frame" and allows
/// ONLY A SINGLE ITEM in it. A BINARY frame is therefore one bit plane
/// for one segment, and a segmentation with N segments carries N frames
/// per source image - never N planes inside one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct SegFrame {
    /// (0062,000B) Referenced Segment Number: which declared segment this
    /// frame's single bit plane belongs to.
    pub segment_number: u16,
    /// The packed bit plane, rows padded to byte boundaries. Exactly
    /// [`plane_bytes`] of the source geometry.
    pub mask: Vec<u8>,
    /// The source image this frame was derived from. Two frames of two
    /// segments over the same image share one source.
    pub source: SegSource,
}

/// Everything the caller supplies that this writer cannot derive.
#[derive(Debug, Clone)]
pub struct SegIdentity {
    /// (0008,0018) of the segmentation itself.
    pub sop_instance_uid: String,
    /// (0020,000D) of the study the segmentation belongs to.
    pub study_uid: String,
    /// (0020,000E) of the series it is stored in.
    pub series_uid: String,
    /// The Frame of Reference UID (0020,0052) of the source images.
    ///
    /// A.51.1: the Segmentation Instance shares the source Frame of
    /// Reference, so this is copied when the source defines one and left
    /// absent when it does not. Writing an invented value would claim the
    /// masks live in a coordinate system nothing else knows about.
    pub frame_of_reference_uid: Option<String>,
    /// Rows/Columns of the SOURCE frames. The mask of a frame covers that
    /// frame, so the frame's own geometry is what sizes the packed bytes.
    pub source_geometry: Geometry,
    /// (0028,0030) of the source frames: the physical size of a pixel, so
    /// a reader can measure a mask instead of only counting its pixels.
    ///
    /// Written as "1\\1" when the source declared no usable value: Pixel
    /// Measures is Type 1 in the functional group (C.7.6.16.2.1), so a
    /// value has to be present, and a unit spacing is the honest answer
    /// for a source that never said what a pixel measures.
    pub pixel_spacing: Option<[f64; 2]>,
    /// (0018,0088) of the source frames: the distance between the slices
    /// the masks were derived from. Only meaningful when the 3D dimension
    /// organisation is declared, which this writer always declares.
    pub slice_spacing: Option<f64>,
    /// (0062,0009) Segment Algorithm Name, 1C because (0062,0008) is
    /// AUTOMATIC rather than MANUAL. It names the algorithm that produced
    /// the segment, so a reader can tell two automatic results apart.
    pub algorithm_name: String,
    /// (0070,0080) Content Label: a short human-readable name for the
    /// object, Type 1 in the Content Identification macro (C.7.8.1).
    pub content_label: String,
    /// The declared segments, in segment-number order: entry `i` is segment
    /// number `i + 1`, matching the ABI's "bit position + 1" rule.
    pub segments: Vec<Segment>,
    /// The frames, in frame order: one per (source image, segment) pair,
    /// each carrying its segment number and one packed bit plane.
    pub frames: Vec<SegFrame>,
    /// (0062,0013) Segments Overlap, when the producer declares it.
    ///
    /// Absent means the attribute is omitted and a reader has to check
    /// every pixel: that is the truthful claim for a producer that does
    /// not know, and inventing "NO" for nested masks would be a lie.
    pub segments_overlap: Option<String>,
    /// (0010,0010) copied from the study, as the PR and SR writers do.
    pub patient_name: Option<String>,
    /// (0010,0020) copied from the study.
    pub patient_id: Option<String>,
    /// (0010,0021) Issuer of Patient ID, the "Issuer of Patient ID Macro"
    /// row of AIW-I Appendix V Table V-1 ("Source Copy").
    ///
    /// `Some("")` writes a ZERO-LENGTH element - the workitem explicitly
    /// said "no issuer at this site", and Appendix V copies the value
    /// unaltered. `None` omits the element. No fallback exists: no source
    /// on this path carries an issuer.
    pub patient_id_issuer: Option<String>,
    /// (0020,0010) copied from the study, Type 2.
    pub study_id: Option<String>,
    /// (0008,0050) copied from the study, Type 2.
    pub accession_number: Option<String>,
    /// (0008,1010) Station Name, from the workitem's (0040,4028)
    /// Performed Station Name Code Sequence Code Meaning (AIW-I Appendix V
    /// Table V-1 Note 4).
    ///
    /// `None` omits the attribute - Type 3, and there is no fallback
    /// because the mapping runs FROM (0040,4028).
    pub station_name: Option<String>,
    /// (0040,0008) Scheduled Protocol Code Sequence, mapped from the
    /// workitem's (0040,4018) (Appendix V Table V-1: "Copy from: Scheduled
    /// Workitem Code Sequence (0040,4018)"). `None` omits the sequence.
    pub scheduled_protocol_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0040,1001) Requested Procedure ID (Appendix V Table V-1 "Source
    /// Copy").
    pub requested_procedure_id: Option<String>,
    /// (0032,1060) Requested Procedure Description.
    pub requested_procedure_description: Option<String>,
    /// (0032,1064) Requested Procedure Code Sequence, one item.
    pub requested_procedure_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0040,1002) Reason for the Requested Procedure.
    pub reason_for_requested_procedure: Option<String>,
    /// (0040,100A) Reason for Requested Procedure Code Sequence, one item.
    pub reason_for_requested_procedure_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0008,103E) from the rule, when it names one.
    pub series_description: Option<String>,
}

/// The number of bytes ONE BIT PLANE of a frame occupies.
///
/// Rows are padded to byte boundaries (PS3.3 C.8.20.2), so a width of 500
/// costs 63 bytes per row rather than 62.5 rounded down across the frame.
#[must_use]
pub fn plane_bytes(geometry: &Geometry) -> usize {
    let row_bytes = (geometry.cols as usize).div_ceil(8);
    row_bytes * geometry.rows as usize
}

/// The number of bytes one ABI MASK FILE occupies: one bit plane per
/// declared segment.
///
/// The processor packs every segment's plane into one file per source
/// frame; the writer then emits ONE FRAME PER SEGMENT (C.8.20.3.1), so
/// this is the file size the ABI seam checks, while [`plane_bytes`] is
/// what each written frame's mask is.
#[must_use]
pub fn frame_bytes(geometry: &Geometry, segments: usize) -> usize {
    plane_bytes(geometry) * segments
}

/// Serialize one BINARY segmentation as a Part-10 file.
///
/// The segment number of `identity.segments[i]` is `i + 1`, which is the
/// same rule that makes bit `k` of a mask byte segment `k + 1`: the two
/// orderings are one ordering, so they are not passed separately.
///
/// Each `identity.frames` mask must already have been checked against
/// [`plane_bytes`]: this writer records what it is given rather than
/// second-guessing it, and a mask of the wrong size is the caller's to
/// refuse (the ABI seam does, before anything is stored).
#[must_use]
pub fn serialize(identity: &SegIdentity) -> Vec<u8> {
    let mut dataset = Vec::with_capacity(4096);

    // -- What this object is ------------------------------------------
    // Group 0008, in ASCENDING tag order (PS3.5 7.1). A reader may stop at the
    // first descent, so the order is part of the encoding and not a matter of
    // style; the module layout that groups these differently is a drafting
    // convenience, not a wire requirement.
    write_cs(&mut dataset, 0x0008, 0x0005, SPECIFIC_CHARACTER_SET);
    // (0008,0008) Image Type is Type 1 in C.8.20-2 with two values: a
    // segmentation is derived data, and it is the primary result of the
    // processing that produced it rather than a secondary capture of one.
    write_cs(&mut dataset, 0x0008, 0x0008, "DERIVED\\PRIMARY");
    // (0008,0012)/(0008,0013) Instance Creation Date/Time: a segmentation is
    // created now. A DA into (0008,0012) and a TM into (0008,0013), never a
    // UID or a version string.
    let (date, time) = now_stamp();
    write_da(&mut dataset, 0x0008, 0x0012, &date);
    write_tm(&mut dataset, 0x0008, 0x0013, &time);
    write_ui(&mut dataset, 0x0008, 0x0016, SEG_SOP_CLASS_UID);
    write_ui(&mut dataset, 0x0008, 0x0018, &identity.sop_instance_uid);
    // (0008,001B) Original Specialized SOP Class UID is not written. It is
    // defined for objects derived from one of the SPECIALIZED classes, and
    // an A.51 segmentation is not one: repeating the SOP Class UID here
    // would claim a derivation that did not happen.
    // (0008,0020)/(0008,0030) Study Date/Time and (0008,0090) Referring
    // Physician Name are Type 2 in the General Study module (C.7.2.1): the
    // attribute has to be present, and an empty value is how a writer says it
    // does not know. A segmentation summarises an existing study rather than
    // being acquired during one, so its own study date is the study's, which
    // this writer is not given.
    write_da(&mut dataset, 0x0008, 0x0020, "");
    // (0008,0021)/(0008,0031): the content date/time is the creation instant,
    // the same one (0008,0012)/(0008,0013) carry.
    write_da(&mut dataset, 0x0008, 0x0021, &date);
    write_tm(&mut dataset, 0x0008, 0x0030, "");
    write_tm(&mut dataset, 0x0008, 0x0031, &time);
    write_sh(
        &mut dataset,
        0x0008,
        0x0050,
        identity.accession_number.as_deref().unwrap_or(""),
    );
    write_cs(&mut dataset, 0x0008, 0x0060, "SEG");
    write_lo(&mut dataset, 0x0008, 0x0070, crate::sr::MANUFACTURER);
    write_pn(&mut dataset, 0x0008, 0x0090, "");
    // (0008,1010) Station Name, from the workitem's (0040,4028) Performed
    // Station Name Code Sequence Code Meaning (AIW-I Appendix V Table V-1
    // Note 4). Type 3, omitted when the workitem carries no such sequence:
    // the mapping runs FROM (0040,4028), so there is no fallback.
    if let Some(station) = &identity.station_name {
        write_sh(&mut dataset, 0x0008, 0x1010, station);
    }
    if let Some(description) = &identity.series_description {
        write_lo(&mut dataset, 0x0008, 0x103E, description);
    }
    // (0008,1090) Manufacturer Model Name, Type 2 in Enhanced General
    // Equipment. Model name, serial number and software versions all describe
    // the implementation that encoded the object, because on this path that
    // implementation is what stands in for a station.
    write_lo(&mut dataset, 0x0008, 0x1090, crate::sr::MANUFACTURER);
    // -- Referenced Series (C.7.6.1) ---------------------------------
    // (0008,1115) groups the source instances BY SERIES, which is why the
    // sources carry a series UID: one item per distinct source series, and
    // one (0008,114A) item per image inside it. A reader follows this to
    // the pixels a mask describes, so a frame with no reference here is a
    // mask with no image to be laid over.
    //
    // PS3.5 7.1 requires the Data Set's elements in ascending tag order, and
    // (0008,1115) sorts inside group 0008 - so it belongs here, before the
    // patient block, not after the content-identification group.
    write_referenced_series_sequence(&mut dataset, &identity.frames);

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
    // (0010,0021) Issuer of Patient ID - AIW-I Appendix V Table V-1,
    // "Issuer of Patient ID Macro" ("Source Copy"). It sorts between
    // PatientID and the study attributes that follow.
    //
    // `Some("")` writes the zero-length element (the workitem explicitly
    // said "no issuer at this site", and the value is copied unaltered);
    // `None` omits it. No fallback: no source on this path carries one.
    if let Some(issuer) = &identity.patient_id_issuer {
        write_lo(&mut dataset, 0x0010, 0x0021, issuer);
    }
    // (0018,1000)/(0018,1020) Device Serial Number and Software Versions:
    // Type 1 in A.51 (Table A.51-1) but Type 2 with the same values in the
    // M3D writer, so the two are named rather than optional. They describe
    // the implementation that encoded the object, because on this path that
    // implementation is what stands in for a station.
    write_lo(&mut dataset, 0x0018, 0x1000, crate::sr::MANUFACTURER);
    write_lo(&mut dataset, 0x0018, 0x1020, &implementation_version_name());
    // -- Study / Series identity (group 0020) -------------------------
    // PS3.5 7.1: elements ascend by tag, so the whole of group 0020 is written
    // before any 0032 or 0040 attribute. Emitting the study/series UIDs after
    // the requested-procedure rows produced a Data Set whose tags ran
    // backwards, which strict readers reject outright.
    write_ui(&mut dataset, 0x0020, 0x000D, &identity.study_uid);
    write_ui(&mut dataset, 0x0020, 0x000E, &identity.series_uid);
    // (0020,0010) Study ID is Type 2 in the General Study module (C.7.2.1).
    // It lives in group 0020, so it travels with the rest of that group rather
    // than sitting in the patient block.
    write_sh(
        &mut dataset,
        0x0020,
        0x0010,
        identity.study_id.as_deref().unwrap_or(""),
    );
    // (0020,0011) Series Number: this writer's own number, so the
    // segmentation is not listed as a copy of the source series. See
    // SERIES_NUMBER for the scheme across the writers.
    write_is(&mut dataset, 0x0020, 0x0011, SERIES_NUMBER);
    write_is(&mut dataset, 0x0020, 0x0013, "1");

    // -- Frame of Reference (C.7.4.1) --------------------------------
    // A.51.1: the segmentation shares the source Frame of Reference. Only
    // written when the source defined one; an absent tag says the masks
    // have no shared coordinate system, which is true rather than invented.
    // Both attributes live in group 0020, so they travel with the rest of it.
    if let Some(frame_of_reference) = &identity.frame_of_reference_uid {
        write_ui(&mut dataset, 0x0020, 0x0052, frame_of_reference);
    }
    // (0020,1040) Position Reference Indicator is Type 2 in C.7.4.1: present
    // but legitimately empty, because the source study's own table position
    // is not something this writer is given.
    write_lo(&mut dataset, 0x0020, 0x1040, "");

    // -- Multi-frame Dimension (C.7.6.17) ----------------------------
    // A mask stack is an organised multi-frame object, and the dimension
    // module is what tells a reader that frame k of the object corresponds
    // to a known index rather than being an anonymous slot. Its attributes
    // live in group 0020, so they are written here, inside that group's run.
    write_dimension_module(&mut dataset, &identity.sop_instance_uid);

    // -- Image Pixel (C.7.6.3) ---------------------------------------
    // Group 0028 sorts before 0032 and 0040, so the pixel module comes next.
    // Bits Allocated 1 is what "bit-packed" means; Bits Stored 1 and High
    // Bit 0 follow from it.
    write_us(&mut dataset, 0x0028, 0x0002, 1);
    // (0028,0004) Photometric Interpretation: a segmentation is a set of
    // bitmaps, so MONOCHROME2 regardless of what the source was.
    write_cs(&mut dataset, 0x0028, 0x0004, "MONOCHROME2");
    // (0028,0006) Planar Configuration is deliberately NOT written: it is
    // 1C and required only when Samples Per Pixel is greater than one,
    // and a BINARY segmentation always has one sample.
    // (0028,0008) Number of Frames: one per (source image, segment) pair,
    // because a BINARY frame is one bit plane of one segment (C.8.20.3.1).
    let frames = u32::try_from(identity.frames.len()).unwrap_or(0);
    write_is(&mut dataset, 0x0028, 0x0008, &frames.to_string());
    write_us(
        &mut dataset,
        0x0028,
        0x0010,
        u16::try_from(identity.source_geometry.rows).unwrap_or(0),
    );
    write_us(
        &mut dataset,
        0x0028,
        0x0011,
        u16::try_from(identity.source_geometry.cols).unwrap_or(0),
    );
    write_us(&mut dataset, 0x0028, 0x0100, 1);
    write_us(&mut dataset, 0x0028, 0x0101, 1);
    write_us(&mut dataset, 0x0028, 0x0102, 0);
    write_us(&mut dataset, 0x0028, 0x0103, 0);
    // (0028,2110) Lossy Image Compression is Type 1 in C.8.20-2. A BINARY
    // segmentation is a bit plane per segment, so there is no lossy step
    // that could have produced it and "00" is the only true value.
    // (0028,2112)/(0028,2114) are conditional on that value being 01, so
    // they are deliberately absent rather than empty.
    write_cs(&mut dataset, 0x0028, 0x2110, "00");

    // -- Requested procedure (groups 0032/0040) ----------------------
    // (0032,1060) Requested Procedure Description and (0032,1064) Requested
    // Procedure Code Sequence - the requested-procedure "Source Copy" rows
    // of AIW-I Appendix V Table V-1. Written only when the workitem states
    // them; there is no fallback (see the report).
    if let Some(description) = &identity.requested_procedure_description {
        write_lo(&mut dataset, 0x0032, 0x1060, description);
    }
    if let Some(code) = &identity.requested_procedure_code {
        write_code_sequence(&mut dataset, 0x0032, 0x1064, &as_abi_code(code));
    }
    // (0040,0008) Scheduled Protocol Code Sequence, mapped from the
    // workitem's (0040,4018), then the remaining requested-procedure rows.
    if let Some(code) = &identity.scheduled_protocol_code {
        write_code_sequence(&mut dataset, 0x0040, 0x0008, &as_abi_code(code));
    }
    if let Some(id) = &identity.requested_procedure_id {
        write_sh(&mut dataset, 0x0040, 0x1001, id);
    }
    if let Some(reason) = &identity.reason_for_requested_procedure {
        write_lo(&mut dataset, 0x0040, 0x1002, reason);
    }
    if let Some(code) = &identity.reason_for_requested_procedure_code {
        write_code_sequence(&mut dataset, 0x0040, 0x100A, &as_abi_code(code));
    }

    // -- Segmentation Series (C.8.20.1) ------------------------------
    // (0062,0001) Segmentation Type: the BINARY form, which is what the
    // conformance statement targets and what the 1-bit packing means.
    write_cs(&mut dataset, 0x0062, 0x0001, "BINARY");
    // (0062,0002) Segment Sequence: one item per declared segment. It sorts
    // before (0062,0013), so it is written first.
    write_sequence_start(&mut dataset, 0x0062, 0x0002);
    for (index, segment) in identity.segments.iter().enumerate() {
        let number = u16::try_from(index + 1).unwrap_or(u16::MAX);
        write_item_start(&mut dataset);
        // PS3.5 7.1 orders the ITEM's elements by tag too, and a segment
        // item is an Item Data Set like any other. Written ascending:
        // (0062,0003) category, (0062,0004) number, (0062,0005) label,
        // (0062,0008) algorithm type, (0062,0009) algorithm name,
        // (0062,000D) CIELab value, (0062,000F) property type.
        write_code_sequence(&mut dataset, 0x0062, 0x0003, &segment.category);
        // (0062,0004) Segment Number: 1..N, and the mask bit for a segment
        // is its number minus one.
        write_us(&mut dataset, 0x0062, 0x0004, number);
        write_lo(&mut dataset, 0x0062, 0x0005, &segment.label);
        // (0062,0008) Segment Algorithm Type: AUTOMATIC unless the
        // processor said otherwise.
        write_cs(&mut dataset, 0x0062, 0x0008, &segment.algorithm);
        // (0062,0009) Segment Algorithm Name is 1C here - the condition is
        // that the algorithm type is not MANUAL, which the caller's own
        // values make true. It names WHICH algorithm produced the segment,
        // which is the difference between two automatic results that a
        // reader would otherwise have no way to tell apart.
        write_lo(&mut dataset, 0x0062, 0x0009, &identity.algorithm_name);
        if let Some(color) = segment.color {
            // (0062,000D) Recommended Display CIELab Value, three US.
            write_explicit_header(&mut dataset, 0x0062, 0x000D, b"US", 6);
            for component in color {
                dataset.extend_from_slice(&component.to_le_bytes());
            }
        }
        write_code_sequence(&mut dataset, 0x0062, 0x000F, &segment.property_type);
        write_item_end(&mut dataset);
    }
    write_sequence_end(&mut dataset);
    // (0062,0013) Segments Overlap: written only when the producer
    // declares it. "NO" claims two masks can never share a pixel, which
    // only the producer can know - a nested-mask demo where the outer
    // segment contains the inner one is "YES".
    if let Some(overlap) = &identity.segments_overlap {
        write_cs(&mut dataset, 0x0062, 0x0013, overlap);
    }

    // -- Content Identification (C.7.8.1) ----------------------------
    // (0070,0080) Content Label is Type 1: it names the object for a human,
    // and an empty value is not an acceptable value for it. The caller
    // guarantees a non-empty one (first segment label), so this writer
    // records it rather than inventing a placeholder.
    write_lo(&mut dataset, 0x0070, 0x0080, &identity.content_label);
    // (0070,0081) Content Description is Type 2, so it is always present
    // and legitimately empty when the rule names no series description.
    write_lo(
        &mut dataset,
        0x0070,
        0x0081,
        identity.series_description.as_deref().unwrap_or(""),
    );

    // -- Shared and per-frame functional groups (C.8.20.2) ----------
    write_shared_functional_groups(&mut dataset, identity);
    write_per_frame_functional_groups(&mut dataset, identity);
    // (0028,9110) Pixel Measures is inside the per-frame sequence, so the
    // top-level Rows/Columns above stay the authoritative size.

    // -- Pixel Data --------------------------------------------------
    // The packed planes, concatenated in frame order and copied verbatim.
    // One buffer, one memcpy per frame, no per-pixel loop: the processor
    // already produced exactly these bytes.
    let payload: usize = identity.frames.iter().map(|frame| frame.mask.len()).sum();
    // PS3.5 8.1: the Value Field of Pixel Data shall be an EVEN number of
    // bytes, and in a native multi-frame image the frames are not padded
    // individually - the padding goes after the complete Value Field. An odd
    // sum therefore needs one trailing pad byte and an even declared length.
    // Declaring the odd sum instead left the next element one byte early,
    // which a reader resolves by reading value bytes as a VR.
    let pad = payload % 2;
    // (7FE0,0010) Pixel Data, OW with a defined length: a bit-packed
    // payload's size is known before the element is written, and OW is
    // the word form the 1-bit case is encoded in (the payload is byte
    // identical to OB; the VR is what tells a native decoder the packed
    // bits are 1-bit words rather than bytes). The frames' single planes
    // are concatenated in frame order, which is the layout C.8.20.3.1
    // gives each frame a segment name for.
    write_explicit_header(&mut dataset, 0x7FE0, 0x0010, b"OW", payload + pad);
    for frame in &identity.frames {
        dataset.extend_from_slice(&frame.mask);
    }
    if pad == 1 {
        // Zero-fill: the pad is "not part of the image and shall not be
        // considered significant" (PS3.5 8.1), and 0 leaves a BINARY mask
        // segment at BACKGROUND rather than marking a pixel.
        dataset.push(0);
    }

    // -- SOP Common (C.12.1) -----------------------------------------
    // (0008,0012)/(0008,0013) Instance Creation Date/Time were written
    // with the series date/time above: SOP Common carries the same
    // creation instant, so they are not written twice.

    finish(identity, dataset)
}

/// Wrap the dataset in the Part-10 file meta group.
fn finish(identity: &SegIdentity, dataset: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(dataset.len() + 512);
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");
    let mut meta = Vec::with_capacity(256);
    write_string(&mut meta, 0x0002, 0x0001, b"OB", "01");
    write_string(&mut meta, 0x0002, 0x0002, b"UI", SEG_SOP_CLASS_UID);
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

/// (5200,9229) Shared Functional Groups Sequence.
///
/// One item, carrying what every frame has in common: the pixel measures
/// (a mask frame is the same size as its source, so the source's own
/// spacing is the physical one) and the segmentation this object is.
fn write_shared_functional_groups(dataset: &mut Vec<u8>, identity: &SegIdentity) {
    write_sequence_start(dataset, 0x5200, 0x9229);
    write_item_start(dataset);
    // (0028,9110) Pixel Measures: the source's Pixel Spacing, so a reader
    // that measures a mask measures the image it covers. "1\1" is written
    // only when the source declared no usable spacing: the attribute is
    // Type 1 here, so it has to be present, and a unit spacing is the
    // honest answer rather than a fabricated physical one.
    write_sequence_start(dataset, 0x0028, 0x9110);
    write_item_start(dataset);
    write_ds(
        dataset,
        0x0028,
        0x0030,
        &spacing_text(identity.pixel_spacing),
    );
    if let Some(thickness) = identity.slice_spacing {
        // (0018,0088) Spacing Between Slices: how far apart the slices the
        // masks came from were, which is what makes the stack a volume.
        write_ds(dataset, 0x0018, 0x0088, &decimal_text(thickness));
    }
    write_item_end(dataset);
    write_sequence_end(dataset);
    // (0020,9116) Plane Orientation (Patient): the source images' own
    // orientation, copied once into the shared item. All frames of one
    // segmentation cover images acquired the same way, so the orientation
    // they share belongs here; a reader rotates the bit planes onto the
    // image plane with it, which a position alone cannot give it.
    if let Some(orientation) = identity
        .frames
        .first()
        .map(|frame| &frame.source)
        .and_then(|source| source.orientation)
    {
        write_sequence_start(dataset, 0x0020, 0x9116);
        write_item_start(dataset);
        write_ds(dataset, 0x0020, 0x0037, &six_text(&orientation));
        write_item_end(dataset);
        write_sequence_end(dataset);
    }
    // (0062,000A) Segment Identification deliberately lives in the
    // PER-FRAME functional groups: C.8.20.3.1 identifies "this Frame",
    // and a frame is what names its segment (single item, 0062,000B).
    write_item_end(dataset);
    write_sequence_end(dataset);
}

/// (5200,9230) Per-frame Functional Groups Sequence.
///
/// One item per frame, each naming the source image the mask was derived
/// from. This is what ties mask frame `i` to a real image: without it a
/// reader has a stack of bitmaps with no idea what they cover.
fn write_per_frame_functional_groups(dataset: &mut Vec<u8>, identity: &SegIdentity) {
    write_sequence_start(dataset, 0x5200, 0x9230);
    // The derivation and purpose codes are the same in every frame, so
    // they are built ONCE: `derivation_code()` returns owned Strings, and
    // calling it per frame would be three allocations per frame per
    // segment for values that never change.
    let derivation = derivation_code();
    let purpose = purpose_code();
    for (index, frame) in identity.frames.iter().enumerate() {
        let source = &frame.source;
        write_item_start(dataset);
        // (0020,9111) Frame Content: (0020,9157) Dimension Index Values is
        // what the dimension module's single pointer resolves to, so this
        // frame is identified as frame `index` rather than an anonymous
        // slot. It is 1-based, matching the manifest's own frame numbering.
        write_sequence_start(dataset, 0x0020, 0x9111);
        write_item_start(dataset);
        let mut stack: [u8; 32] = [0; 32];
        let value = u32::try_from(index + 1).unwrap_or(u32::MAX);
        write_ds(dataset, 0x0020, 0x9157, decimal_into(&mut stack, value));
        write_item_end(dataset);
        write_sequence_end(dataset);
        // (0020,9113) Plane Position (Patient): the source image's own
        // position, copied frame by frame. A mask frame covers exactly the
        // image that position names, so it is what places the mask in the
        // study's coordinate system instead of leaving it floating.
        if let Some(position) = &source.position {
            let mut position_buf = [0u8; 64];
            write_sequence_start(dataset, 0x0020, 0x9113);
            write_item_start(dataset);
            write_ds(
                dataset,
                0x0020,
                0x0032,
                triple_into(&mut position_buf, position),
            );
            write_item_end(dataset);
            write_sequence_end(dataset);
        }
        // (0028,9110) Pixel Measures: Rows/Columns come from the source
        // geometry the caller validated, so a frame cannot claim a size
        // the packed bytes do not have.
        write_sequence_start(dataset, 0x0028, 0x9110);
        write_item_start(dataset);
        write_us(
            dataset,
            0x0028,
            0x0010,
            u16::try_from(identity.source_geometry.rows).unwrap_or(0),
        );
        write_us(
            dataset,
            0x0028,
            0x0011,
            u16::try_from(identity.source_geometry.cols).unwrap_or(0),
        );
        let mut spacing_buf = [0u8; 64];
        write_ds(
            dataset,
            0x0028,
            0x0030,
            spacing_into(&mut spacing_buf, identity.pixel_spacing),
        );
        write_item_end(dataset);
        write_sequence_end(dataset);
        // (0008,9124) Derivation Image Sequence -> (0008,2112) Source Image
        // Sequence: the source the frame's mask was computed from, plus the
        // codes A.51.5.1 requires for the derivation and the purpose.
        write_sequence_start(dataset, 0x0008, 0x9124);
        write_item_start(dataset);
        // (0008,9215) Derivation Code Sequence: the segmentation was
        // derived from the source by segmenting it.
        write_code_sequence(dataset, 0x0008, 0x9215, &derivation);
        write_sequence_start(dataset, 0x0008, 0x2112);
        write_item_start(dataset);
        write_ui(dataset, 0x0008, 0x1150, &source.sop_class_uid);
        write_ui(dataset, 0x0008, 0x1155, &source.sop_instance_uid);
        // (0040,A170) Purpose of Reference Code Sequence: the source image
        // is referenced as the input of the processing operation, which is
        // the only relationship a segmentation's source has.
        write_code_sequence(dataset, 0x0040, 0xA170, &purpose);
        write_item_end(dataset);
        write_sequence_end(dataset);
        write_item_end(dataset);
        write_sequence_end(dataset);
        // (0062,000A) Segment Identification Sequence: ONE item, the
        // segment this frame's single bit plane belongs to. C.8.20.3.1
        // allows only a single item and a single Referenced Segment
        // Number per frame; a reader (Weasis included) keys its contour
        // for the frame to exactly this number.
        write_sequence_start(dataset, 0x0062, 0x000A);
        write_item_start(dataset);
        write_us(dataset, 0x0062, 0x000B, frame.segment_number);
        write_item_end(dataset);
        write_sequence_end(dataset);
        write_item_end(dataset);
    }
    write_sequence_end(dataset);
}

/// (0008,9215) Derivation Code Sequence. A.51.5.1 mandates (113076, DCM,
/// "Segmentation") for a segmentation's derivation.
fn derivation_code() -> Code {
    Code {
        code: "113076".to_string(),
        scheme: "DCM".to_string(),
        meaning: "Segmentation".to_string(),
    }
}

/// (0040,A170) Purpose of Reference Code Sequence. A.51.5.1 mandates
/// (121322, DCM, "Source Image for Image Processing Operation").
fn purpose_code() -> Code {
    Code {
        code: "121322".to_string(),
        scheme: "DCM".to_string(),
        meaning: "Source Image for Image Processing Operation".to_string(),
    }
}

/// (0008,1115) Referenced Series Sequence.
///
/// One item per DISTINCT source series, each holding a (0008,114A)
/// Referenced Instance Sequence with one item per source image. The
/// grouping is the standard's (C.7.6.1): a series item is what a reader
/// follows to reach a series' images, and putting an image into the wrong
/// series item would send it looking for pixels in a series it is not in.
fn write_referenced_series_sequence(dataset: &mut Vec<u8>, frames: &[SegFrame]) {
    write_sequence_start(dataset, 0x0008, 0x1115);
    // One pass to find the distinct series in first-seen order, then one
    // pass per series to write its instances. The series count is the
    // study's, not the frame count, so the pair of loops stays small.
    let mut seen: Vec<&str> = Vec::new();
    for frame in frames {
        if !seen.contains(&frame.source.series_uid.as_str()) {
            seen.push(&frame.source.series_uid);
        }
    }
    for series_uid in &seen {
        write_item_start(dataset);
        write_ui(dataset, 0x0020, 0x000E, series_uid);
        write_sequence_start(dataset, 0x0008, 0x114A);
        // Several frames (one per segment) can derive from the SAME
        // source image, but the reference lists INSTANCES: each source
        // SOP is written once even though its frames repeat it.
        let mut written: Vec<&str> = Vec::new();
        for frame in frames {
            let source = &frame.source;
            if source.series_uid.as_str() != *series_uid
                || written.contains(&source.sop_instance_uid.as_str())
            {
                continue;
            }
            written.push(&source.sop_instance_uid);
            write_item_start(dataset);
            write_ui(dataset, 0x0008, 0x1150, &source.sop_class_uid);
            write_ui(dataset, 0x0008, 0x1155, &source.sop_instance_uid);
            write_item_end(dataset);
        }
        write_sequence_end(dataset);
        write_item_end(dataset);
    }
    write_sequence_end(dataset);
}

/// (0020,9221)/(0020,9222) Multi-frame Dimension module (C.7.6.17).
///
/// One dimension organisation UID and one index item pointing at
/// (0020,9157) Dimension Index Values, which each frame's Frame Content
/// carries. `organization` is derived from the object's own SOP Instance
/// UID so the same object always declares the same organisation and a
/// regenerated one declares a new one rather than colliding with it.
fn write_dimension_module(dataset: &mut Vec<u8>, sop_instance_uid: &str) {
    write_sequence_start(dataset, 0x0020, 0x9221);
    write_item_start(dataset);
    write_ui(
        dataset,
        0x0020,
        0x9164,
        &dimension_organization_uid(sop_instance_uid),
    );
    write_item_end(dataset);
    write_sequence_end(dataset);
    write_sequence_start(dataset, 0x0020, 0x9222);
    write_item_start(dataset);
    // (0020,9165) Dimension Index Pointer names the attribute whose values
    // index this dimension: the frame's Dimension Index Values.
    write_at(dataset, 0x0020, 0x9165, &[0x20, 0x00, 0x57, 0x91]);
    // (0020,9167) Functional Group Pointer names the functional group the
    // pointer is resolved inside: Frame Content.
    write_at(dataset, 0x0020, 0x9167, &[0x20, 0x00, 0x11, 0x91]);
    write_item_end(dataset);
    write_sequence_end(dataset);
}

/// The dimension organisation UID of one segmentation.
///
/// `2.25.<digest>`: the SHA-1-derived arc, minted from the object's own SOP
/// Instance UID so the value is stable for an object and distinct between
/// two objects, without a random source or a registry. The digest is a
/// decimal rendering of a 64-bit FNV-1a hash rather than a real digest,
/// because the value only has to be unique among the objects this writer
/// produces and a cryptographic hash would need a dependency the crate
/// deliberately does not carry.
fn dimension_organization_uid(sop_instance_uid: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in b"seg-dimension:".iter().chain(sop_instance_uid.as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("2.25.{hash}")
}

/// (0020,9165)/(0020,9167) Dimension Index Pointer and Functional Group
/// Pointer, both AT: two little-endian US values, group then element.
fn write_at(out: &mut Vec<u8>, group: u16, element: u16, value: &[u8; 4]) {
    write_explicit_header(out, group, element, b"AT", 4);
    out.extend_from_slice(value);
}

/// One coded concept, as a Code Sequence with one item.
///
/// The three attributes are the minimum a Code Sequence item can carry and
/// still be interpretable: a code with no scheme is not resolvable, and a
/// code with no meaning cannot be shown to a reader.
fn write_code_sequence(out: &mut Vec<u8>, group: u16, element: u16, code: &Code) {
    write_sequence_start(out, group, element);
    write_item_start(out);
    write_sh(out, 0x0008, 0x0100, &code.code);
    write_sh(out, 0x0008, 0x0102, &code.scheme);
    write_lo(out, 0x0008, 0x0104, &code.meaning);
    write_item_end(out);
    write_sequence_end(out);
}

/// A single US value.
fn write_us(out: &mut Vec<u8>, group: u16, element: u16, value: u16) {
    write_explicit_header(out, group, element, b"US", 2);
    out.extend_from_slice(&value.to_le_bytes());
}

/// (0028,0030) Pixel Spacing as DS text: "row\\column", the order the
/// standard defines it in.
///
/// Falls back to "1\\1" when the source declared no usable spacing, because
/// Pixel Measures is Type 1 and an absent value is not an option. A unit
/// spacing is the one value that says "a pixel is a pixel" rather than
/// claiming a physical size nothing measured.
fn spacing_text(spacing: Option<[f64; 2]>) -> String {
    match spacing {
        Some([row, column]) => format!("{}\\{}", decimal_text(row), decimal_text(column)),
        None => "1\\1".to_string(),
    }
}

/// Six DS values as text, backslash-separated as the standard writes an
/// Image Orientation Patient: row direction cosines, then column. Used
/// once per object (the SHARED functional group), so the `String` it
/// returns is not a per-frame cost.
fn six_text(values: &[f64; 6]) -> String {
    let mut text = decimal_text(values[0]);
    for component in &values[1..] {
        text.push('\\');
        text.push_str(&decimal_text(*component));
    }
    text
}

/// One DS value as text, within the sixteen characters the VR allows.
///
/// Rendering lives in `pr::ds_text` so the three writers cannot disagree
/// about what a DS is: the format prefers plain digits (`1.5`, not
/// `1.500000`) and switches to exponent form only when the plain form
/// would exceed the limit.
///
/// The precision is chosen for what the value MEANS. Spacing and slice
/// thickness are millimetre-scale measurements, so three decimals is a
/// thousandth of a millimetre - finer than any scanner resolves and short
/// enough that the exponent branch is unreachable for a real geometry.
fn decimal_text(value: f64) -> String {
    crate::pr::ds_text(value, 6, 3)
}

/// The same rendering as [`decimal_text`], into a caller-owned buffer.
///
/// The per-frame loop runs once per frame and each call would otherwise
/// allocate a `String`: writing into a caller's stack buffer keeps the
/// frame path allocation-free, which is the discipline the rest of the
/// external path already follows.
fn decimal_into(buffer: &mut [u8; 32], value: u32) -> &str {
    let mut cursor = buffer.len();
    let mut remaining = value;
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    // The buffer was filled from the back with at least one digit, so the
    // range is valid UTF-8 by construction.
    std::str::from_utf8(&buffer[cursor..]).unwrap_or("0")
}

/// A `core::fmt::Write` over a fixed caller-owned buffer: `write!` into
/// it formats without touching the heap.
///
/// Shared with `pr`'s DS renderer, which formats into the same kind of
/// buffer for the same reason: a per-value `String` in a per-frame loop is
/// an allocation per frame.
pub(crate) struct StackBuf<'a> {
    pub(crate) buf: &'a mut [u8],
    pub(crate) len: usize,
}

impl core::fmt::Write for StackBuf<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// One f64 as DS text into a caller-owned buffer - the allocation-free
/// twin of [`decimal_text`], because the per-frame functional group path
/// writes two DS values per frame and a `String` per value would be an
/// allocation per frame per segment.
fn decimal_f64_into(buffer: &mut [u8; 32], value: f64) -> &str {
    let mut wide = [0u8; crate::pr::DS_MAX_CHARS * 2];
    let text = crate::pr::ds_text_into(&mut wide, value, 6, 3);
    let len = text.len().min(buffer.len());
    buffer[..len].copy_from_slice(&text.as_bytes()[..len]);
    std::str::from_utf8(&buffer[..len]).unwrap_or("0")
}

/// (0028,0030) Pixel Spacing as DS text into a caller-owned buffer:
/// "row\\column" or the "1\\1" fallback, formatted without a heap
/// allocation so the per-frame Pixel Measures item costs nothing per
/// frame.
fn spacing_into(buffer: &mut [u8; 64], spacing: Option<[f64; 2]>) -> &str {
    let Some([row, column]) = spacing else {
        return "1\\1";
    };
    let mut row_buf = [0u8; 32];
    let mut column_buf = [0u8; 32];
    let row = decimal_f64_into(&mut row_buf, row);
    let column = decimal_f64_into(&mut column_buf, column);
    let mut offset = 0;
    buffer[offset..offset + row.len()].copy_from_slice(row.as_bytes());
    offset += row.len();
    buffer[offset] = b'\\';
    offset += 1;
    buffer[offset..offset + column.len()].copy_from_slice(column.as_bytes());
    offset += column.len();
    std::str::from_utf8(&buffer[..offset]).unwrap_or("1\\1")
}

/// Three DS values as text (an Image Position Patient) into a
/// caller-owned buffer, so the per-frame Plane Position item costs
/// nothing per frame.
fn triple_into<'a>(buffer: &'a mut [u8; 64], values: &[f64; 3]) -> &'a str {
    let mut parts = [[0u8; 32]; 3];
    let (head, rest) = parts.split_at_mut(1);
    let (middle, tail) = rest.split_at_mut(1);
    let texts = [
        decimal_f64_into(&mut head[0], values[0]),
        decimal_f64_into(&mut middle[0], values[1]),
        decimal_f64_into(&mut tail[0], values[2]),
    ];
    let mut offset = 0;
    for (index, text) in texts.iter().enumerate() {
        if index > 0 {
            buffer[offset] = b'\\';
            offset += 1;
        }
        buffer[offset..offset + text.len()].copy_from_slice(text.as_bytes());
        offset += text.len();
    }
    std::str::from_utf8(&buffer[..offset]).unwrap_or("0\\0\\0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::schema::Segment;

    /// The dataset, past the preamble, the magic and the meta group.
    fn dataset(bytes: &[u8]) -> &[u8] {
        let length = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]) as usize;
        &bytes[132 + 12 + length..]
    }

    /// The top-level tags, in the order they were written.
    ///
    /// Each element is measured by its own header; the walk stops where a
    /// sequence's items begin, because those are not top-level elements.
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
            if length == 0xFFFF_FFFF {
                // An undefined-length sequence: its items are not top-level
                // elements, and the writer emits it near the end, so the walk
                // has done its job.
                break;
            }
            at += head + length;
            if !length.is_multiple_of(2) {
                at += 1;
            }
        }
        tags
    }

    #[test]
    fn the_top_level_elements_are_in_ascending_tag_order() {
        // PS3.5 7.1: the data set is an ordered list in ascending tag order.
        // This writer groups attributes by module, so groups landed out of
        // order (0070 before 0020, PixelData before 0008).
        let bytes = serialize(&identity(vec![vec![0xFFu8; 2]], 1));
        let tags = top_level_tags(&bytes);
        assert!(tags.len() > 15, "the walk reached the end: {tags:?}");
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
        // (0008,0008) Image Type must precede Pixel Data.
        let image_type = tags.iter().position(|t| *t == (0x0008, 0x0008));
        assert!(image_type.is_some(), "Image Type is written: {tags:?}");
    }

    /// The whole Data Set ascends, not just the part before the first sequence.
    ///
    /// `top_level_tags` stops at an undefined-length SQ, which the writer emits
    /// in the middle - so an ordering defect in the groups AFTER it (the
    /// content-identification 0070 rows, the pixel module in 0028, the
    /// referenced-series 0008,1115 sequence that used to sit at the very end)
    /// was invisible to it. This walks every element, skipping sequence
    /// CONTENTS by their declared length, and fails on the first descent.
    #[test]
    fn every_top_level_element_ascends_including_after_the_sequences() {
        let bytes = serialize(&identity(vec![vec![0xFFu8; 2]], 1));
        let tags = all_top_level_tags(&bytes);
        assert!(
            tags.len() > 25,
            "the walk must reach the pixel module and beyond: {tags:?}"
        );
        for pair in tags.windows(2) {
            assert!(
                pair[0] < pair[1],
                "the Data Set descends: {:04X},{:04X} before {:04X},{:04X} (all tags: {tags:?})",
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1
            );
        }
        // The sequences that used to be misplaced must still be present.
        assert!(
            tags.contains(&(0x0008, 0x1115)),
            "the referenced-series sequence is written: {tags:?}"
        );
        assert!(
            tags.contains(&(0x0070, 0x0080)),
            "the content label is written: {tags:?}"
        );
    }

    /// Every top-level tag, in written order, skipping sequence contents.
    ///
    /// A sequence is skipped structurally, not by length: the writer emits
    /// every SQ with the undefined-length sentinel and an explicit delimiter,
    /// and sequence items nest (a referenced-series item contains a
    /// referenced-instance SQ). The walk therefore keeps a depth counter:
    /// inside a sequence it only follows item headers and their delimiters,
    /// and it resumes reading elements once the outermost delimiter is seen.
    fn all_top_level_tags(bytes: &[u8]) -> Vec<(u16, u16)> {
        const LONG_VR: &[&[u8; 2]] = &[
            b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT",
            b"UV",
        ];
        let data = dataset(bytes);
        let mut tags = Vec::new();
        let mut at = 0usize;
        // How many undefined-length sequences are currently open.
        let mut depth = 0usize;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
            if group == 0xFFFE {
                // Item or delimiter: item start/end are 8-byte headers with a
                // 32-bit length; a delimiter ends one level.
                let len =
                    u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]])
                        as usize;
                at += 8;
                match element {
                    0xE000 => {
                        // Item start. The writer emits every item with the
                        // undefined-length sentinel, so it opens one level that
                        // its own (FFFE,E00D) closes; a defined length would be
                        // skipped by its size instead.
                        if len == 0xFFFF_FFFF {
                            depth += 1;
                        } else {
                            at += len;
                        }
                    }
                    0xE00D | 0xE0DD => {
                        depth = depth.saturating_sub(1);
                    }
                    _ => return tags,
                }
                continue;
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
            if depth == 0 {
                tags.push((group, element));
            }
            if length == 0xFFFF_FFFF {
                depth += 1;
                at += head;
                continue;
            }
            at += head + length;
        }
        tags
    }

    fn geometry() -> Geometry {
        Geometry {
            rows: 2,
            cols: 8,
            samples: 1,
            bits_allocated: 16,
            bits_stored: 16,
            frames: 1,
            inverted: false,
            signed: false,
        }
    }

    fn code(meaning: &str) -> Code {
        Code {
            code: "M-01000".to_string(),
            scheme: "SRT".to_string(),
            meaning: meaning.to_string(),
        }
    }

    fn identity(frames: Vec<Vec<u8>>, sources: usize) -> SegIdentity {
        let source = |index: usize| SegSource {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.1.1".to_string(),
            sop_instance_uid: format!("1.2.3.4.10.{index}"),
            series_uid: "1.2.3.4.9".to_string(),
            position: Some([1.5, 2.5, f64::from(index as u32) * 3.0]),
            orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
        };
        let frames = frames
            .into_iter()
            .enumerate()
            .map(|(index, mask)| {
                let source_index = index.min(sources.saturating_sub(1));
                SegFrame {
                    segment_number: 1,
                    mask,
                    source: source(source_index),
                }
            })
            .collect();
        SegIdentity {
            sop_instance_uid: "1.2.3.4.5".to_string(),
            study_uid: "1.2.3.4.6".to_string(),
            series_uid: "1.2.3.4.7".to_string(),
            frame_of_reference_uid: Some("1.2.3.4.8".to_string()),
            source_geometry: geometry(),
            // Deliberately not the writer's "1\\1" fallback: a test that
            // seed the fallback could not tell the fallback from a copy.
            pixel_spacing: Some([0.7, 0.7]),
            slice_spacing: Some(2.5),
            algorithm_name: "threshold".to_string(),
            content_label: "Masks".to_string(),
            segments: vec![Segment {
                label: "MASK".to_string(),
                category: code("Morphologically Altered Structure"),
                property_type: code("Morphologically Altered Structure"),
                color: Some([255, 0, 0]),
                algorithm: "AUTOMATIC".to_string(),
            }],
            frames,
            segments_overlap: None,
            patient_name: Some("Fixture^SEG".to_string()),
            patient_id: Some("SEG-FIXTURE".to_string()),
            patient_id_issuer: Some("SITE-A".to_string()),
            study_id: Some("STUDY-1".to_string()),
            accession_number: Some("ACC-1".to_string()),
            station_name: Some("Fixture Station".to_string()),
            scheduled_protocol_code: Some(crate::dicomweb::WorkitemCode {
                value: "CAD".to_string(),
                scheme: "99CLARUS".to_string(),
                meaning: "AI detection".to_string(),
            }),
            requested_procedure_id: Some("RP-1".to_string()),
            requested_procedure_description: Some("Chest CT".to_string()),
            requested_procedure_code: Some(crate::dicomweb::WorkitemCode {
                value: "RP-CHEST".to_string(),
                scheme: "99CLARUS".to_string(),
                meaning: "Chest procedure".to_string(),
            }),
            reason_for_requested_procedure: Some("Screening".to_string()),
            reason_for_requested_procedure_code: Some(crate::dicomweb::WorkitemCode {
                value: "REASON-1".to_string(),
                scheme: "99CLARUS".to_string(),
                meaning: "Screening indication".to_string(),
            }),
            series_description: Some("Masks".to_string()),
        }
    }

    fn find(bytes: &[u8], group: u16, element: u16) -> Option<usize> {
        let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
        bytes
            .windows(4)
            .position(|window| window == needle.as_slice())
    }

    /// The first offset in `bytes` at which `needle` appears.
    fn find_subslice(bytes: &[u8], needle: &[u8]) -> Option<usize> {
        bytes
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn one_plane_costs_rows_times_the_padded_row_width() {
        // 8 columns is exactly one byte per row.
        assert_eq!(plane_bytes(&geometry()), 2);
        // 9 columns pads each row to two bytes.
        let mut wide = geometry();
        wide.cols = 9;
        assert_eq!(plane_bytes(&wide), 4);
        // 512 and 1024 divide cleanly, which is the realistic case.
        let mut big = geometry();
        big.rows = 512;
        big.cols = 512;
        assert_eq!(plane_bytes(&big), 512 * 64);
    }

    #[test]
    fn a_mask_file_costs_one_plane_per_segment() {
        // One segment is the threshold filter's case, and it reduces to
        // the documented ceil(W*H/8) shape.
        assert_eq!(frame_bytes(&geometry(), 1), 2);
        // Three segments: three bit planes, one after the other in the
        // ABI file; the writer then emits one frame per plane.
        assert_eq!(frame_bytes(&geometry(), 3), 6);
    }

    #[test]
    fn the_file_meta_names_the_binary_segmentation_class() {
        let bytes = serialize(&identity(vec![vec![0b1010_1010, 0]], 1));
        assert_eq!(&bytes[128..132], b"DICM");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains(SEG_SOP_CLASS_UID),
            "meta carries the SEG class"
        );
        assert!(text.contains("SEG"), "modality");
    }

    #[test]
    fn the_station_name_comes_from_the_workitem_and_is_omitted_without_one() {
        // AIW-I Appendix V Table V-1 Note 4: station name is mapped FROM
        // the workitem's (0040,4028). Type 3, so there is no fallback.
        let bytes = serialize(&identity(vec![vec![0b1010_1010, 0]], 1));
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("Fixture Station"),
            "(0040,4028) Code Meaning is copied"
        );

        let mut without = identity(vec![vec![0b1010_1010, 0]], 1);
        without.station_name = None;
        let bytes = serialize(&without);
        assert!(
            !String::from_utf8_lossy(&bytes).contains("Fixture Station"),
            "no (0040,4028) means no Station Name value"
        );
    }

    /// T1/T2/T3: the tail sprint's Appendix V mappings.
    #[test]
    fn the_appendix_v_requestor_identity_is_copied_when_the_workitem_states_it() {
        let bytes = serialize(&identity(vec![vec![0b1010_1010, 0]], 1));
        let has_tag = |group: u16, element: u16| {
            let g = group.to_le_bytes();
            let e = element.to_le_bytes();
            bytes.windows(4).any(|w| w == [g[0], g[1], e[0], e[1]])
        };
        for (group, element) in [
            (0x0010, 0x0021),
            (0x0040, 0x0008),
            (0x0040, 0x1001),
            (0x0040, 0x1002),
            (0x0040, 0x100A),
            (0x0032, 0x1060),
            (0x0032, 0x1064),
        ] {
            assert!(
                has_tag(group, element),
                "({group:04X},{element:04X}) is present"
            );
        }
        let text = String::from_utf8_lossy(&bytes);
        for value in ["SITE-A", "CAD", "RP-1", "Screening", "Chest CT"] {
            assert!(text.contains(value), "{value} is carried");
        }
    }

    #[test]
    fn a_silent_workitem_writes_no_issuer_and_no_requested_procedure_rows() {
        let mut absent = identity(vec![vec![0b1010_1010, 0]], 1);
        absent.patient_id_issuer = None;
        absent.scheduled_protocol_code = None;
        absent.requested_procedure_id = None;
        absent.requested_procedure_description = None;
        absent.requested_procedure_code = None;
        absent.reason_for_requested_procedure = None;
        absent.reason_for_requested_procedure_code = None;
        let bytes = serialize(&absent);
        let has_tag = |group: u16, element: u16| {
            let g = group.to_le_bytes();
            let e = element.to_le_bytes();
            bytes.windows(4).any(|w| w == [g[0], g[1], e[0], e[1]])
        };
        for (group, element) in [
            (0x0010, 0x0021),
            (0x0040, 0x0008),
            (0x0040, 0x1001),
            (0x0040, 0x1002),
            (0x0040, 0x100A),
            (0x0032, 0x1060),
            (0x0032, 0x1064),
        ] {
            assert!(
                !has_tag(group, element),
                "({group:04X},{element:04X}) is omitted when the workitem is silent"
            );
        }

        let mut empty = identity(vec![vec![0b1010_1010, 0]], 1);
        empty.patient_id_issuer = Some(String::new());
        let bytes = serialize(&empty);
        let g = 0x0010u16.to_le_bytes();
        let e = 0x0021u16.to_le_bytes();
        assert!(
            bytes.windows(4).any(|w| w == [g[0], g[1], e[0], e[1]]),
            "an explicit empty issuer is still an element, not omitted"
        );
    }

    #[test]
    fn the_segmentation_is_binary_and_overlap_is_only_what_is_declared() {
        let bytes = serialize(&identity(vec![vec![0b1010_1010, 0]], 1));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("BINARY"), "(0062,0001) is BINARY");
        assert!(text.contains("MONOCHROME2"), "photometric");
        // (0062,0013) is absent unless the producer declares it: a writer
        // that guessed "NO" for nested masks would be lying, so the
        // default is to say nothing (a reader must check every pixel).
        assert_eq!(
            text_at(&bytes, 0x0062, 0x0013),
            None,
            "(0062,0013) is omitted when the producer did not declare overlap"
        );
        assert_eq!(
            text_at(&bytes, 0x0062, 0x0001).as_deref(),
            Some("BINARY"),
            "(0062,0001) is the BINARY type"
        );
    }

    /// One element's value as text, for the assertions that must not be
    /// satisfied by a substring found elsewhere in the object.
    fn text_at(bytes: &[u8], group: u16, element: u16) -> Option<String> {
        let at = find(bytes, group, element)?;
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        Some(
            String::from_utf8_lossy(&bytes[at + 8..at + 8 + length])
                .trim_end_matches(['\0', ' '])
                .to_string(),
        )
    }

    #[test]
    fn the_pixel_data_is_the_packed_mask_verbatim() {
        let mask = vec![0b1010_1010u8, 0b0101_0101];
        let bytes = serialize(&identity(vec![mask.clone()], 1));
        let at = find(&bytes, 0x7FE0, 0x0010).expect("(7FE0,0010)");
        // OW: the 1-bit payload is a stream of packed words, and the VR
        // is what native decoders key on.
        assert_eq!(&bytes[at + 4..at + 6], b"OW", "pixel data is OW");
        let length =
            u32::from_le_bytes([bytes[at + 8], bytes[at + 9], bytes[at + 10], bytes[at + 11]]);
        assert_eq!(length as usize, mask.len(), "a defined length");
        assert_eq!(
            &bytes[at + 12..at + 12 + mask.len()],
            mask.as_slice(),
            "verbatim, byte for byte"
        );
        // (0028,0006) Planar Configuration is absent: one sample has no
        // plane order to declare.
        assert!(
            find(&bytes, 0x0028, 0x0006).is_none(),
            "no Planar Configuration for a single sample"
        );
    }

    #[test]
    fn two_frames_concatenate_in_frame_order() {
        let first = vec![0xAAu8, 0xAA];
        let second = vec![0x55u8, 0x55];
        let bytes = serialize(&identity(vec![first.clone(), second.clone()], 2));
        let at = find(&bytes, 0x7FE0, 0x0010).expect("(7FE0,0010)");
        assert_eq!(&bytes[at + 12..at + 14], first.as_slice());
        assert_eq!(&bytes[at + 14..at + 16], second.as_slice());
    }

    #[test]
    fn each_frame_names_its_own_source_image() {
        let bytes = serialize(&identity(vec![vec![1, 2], vec![3, 4]], 2));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("1.2.3.4.10.0"), "frame 1's source");
        assert!(text.contains("1.2.3.4.10.1"), "frame 2's source");
    }

    #[test]
    fn the_frame_of_reference_is_copied_from_the_source() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        let at = find(&bytes, 0x0020, 0x0052).expect("(0020,0052)");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        let value = String::from_utf8_lossy(&bytes[at + 8..at + 8 + length])
            .trim_end_matches(['\0', ' '])
            .to_string();
        assert_eq!(value, "1.2.3.4.8");
    }

    #[test]
    fn an_absent_frame_of_reference_is_omitted_rather_than_invented() {
        let mut seg = identity(vec![vec![1, 2]], 1);
        seg.frame_of_reference_uid = None;
        let bytes = serialize(&seg);
        assert!(find(&bytes, 0x0020, 0x0052).is_none());
    }

    #[test]
    fn the_segment_carries_its_coded_property_and_algorithm() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("M-01000"), "the code value");
        assert!(text.contains("SRT"), "the coding scheme");
        assert!(
            text.contains("Morphologically Altered Structure"),
            "the meaning, twice: category and type"
        );
        assert!(text.contains("AUTOMATIC"), "(0062,0008)");
        assert!(text.contains("MASK"), "the segment label");
    }

    #[test]
    fn a_declared_colour_is_written_and_an_absent_one_is_omitted() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        let at = find(&bytes, 0x0062, 0x000D).expect("(0062,000D)");
        assert_eq!(&bytes[at + 4..at + 6], b"US");
        assert_eq!(
            u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]),
            6,
            "three US values"
        );
        assert_eq!(&bytes[at + 8..at + 10], &255u16.to_le_bytes(), "L");
        assert_eq!(&bytes[at + 10..at + 12], &0u16.to_le_bytes(), "a");
        assert_eq!(&bytes[at + 12..at + 14], &0u16.to_le_bytes(), "b");

        let mut plain = identity(vec![vec![1, 2]], 1);
        plain.segments[0].color = None;
        let bytes = serialize(&plain);
        assert!(find(&bytes, 0x0062, 0x000D).is_none());
    }

    #[test]
    fn the_number_of_frames_is_the_frame_count() {
        let bytes = serialize(&identity(vec![vec![1, 2], vec![3, 4], vec![5, 6]], 3));
        let at = find(&bytes, 0x0028, 0x0008).expect("(0028,0008)");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        let value = String::from_utf8_lossy(&bytes[at + 8..at + 8 + length])
            .trim()
            .to_string();
        assert_eq!(value, "3");
    }

    #[test]
    fn the_series_number_separates_this_series_from_the_source_and_the_rtstruct() {
        // A viewer lists a study's series BY NUMBER, so two series in one
        // study that share a number are two rows a reader cannot tell
        // apart. The segmentation used to write 1, the same number the
        // source CT carries, which is what made it look like a copy of the
        // study rather than a result derived from it.
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        let at = find(&bytes, 0x0020, 0x0011).expect("(0020,0011)");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        let value = String::from_utf8_lossy(&bytes[at + 8..at + 8 + length])
            .trim()
            .to_string();
        assert_eq!(
            value, SERIES_NUMBER,
            "(0020,0011) is this writer's own series number"
        );
        assert_eq!(
            value, "2",
            "the SEG series number is 2, pinned by literal: CT is 1, the \
             RTSTRUCT is 3, and this is the second series in the study"
        );
        // The three numbers in one study are pairwise distinct, which is
        // the property the scheme exists to provide.
        assert_ne!(
            "2",
            crate::rtstruct::SERIES_NUMBER,
            "the SEG is not the RTSTRUCT"
        );
        assert_ne!("2", "1", "the SEG is not the source acquisition");
        assert_ne!(
            crate::rtstruct::SERIES_NUMBER,
            "1",
            "the RTSTRUCT is not the source acquisition either"
        );
    }

    #[test]
    fn bits_allocated_is_one_and_the_high_bit_follows() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        for (group, element, expected) in [
            (0x0028u16, 0x0100u16, 1u16),
            (0x0028, 0x0101, 1),
            (0x0028, 0x0102, 0),
        ] {
            let at = find(&bytes, group, element)
                .unwrap_or_else(|| panic!("({group:04X},{element:04X})"));
            assert_eq!(
                u16::from_le_bytes([bytes[at + 8], bytes[at + 9]]),
                expected,
                "({group:04X},{element:04X})"
            );
        }
    }

    #[test]
    fn the_file_meta_group_length_spans_the_meta_elements() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert_eq!(&bytes[132..134], &[0x02, 0x00]);
        assert_eq!(&bytes[136..138], b"UL");
        let declared = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]);
        let meta_end = 144 + declared as usize;
        assert_eq!(
            &bytes[meta_end..meta_end + 4],
            &[0x08, 0x00, 0x05, 0x00],
            "the dataset starts with (0008,0005)"
        );
    }

    /// The value of a text element, as a reader would see it.
    fn text_or_panic(bytes: &[u8], group: u16, element: u16) -> String {
        text_at(bytes, group, element)
            .unwrap_or_else(|| panic!("({group:04X},{element:04X}) is absent"))
    }

    #[test]
    fn the_image_type_is_derived_and_primary() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert_eq!(text_or_panic(&bytes, 0x0008, 0x0008), "DERIVED\\PRIMARY");
    }

    #[test]
    fn a_segmentation_declares_its_compression_is_lossless() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert_eq!(text_or_panic(&bytes, 0x0028, 0x2110), "00");
        // (0028,2112)/(0028,2114) are conditional on a lossy value, so a
        // lossless object must not carry them at all.
        assert!(find(&bytes, 0x0028, 0x2112).is_none());
        assert!(find(&bytes, 0x0028, 0x2114).is_none());
    }

    #[test]
    fn the_enhanced_general_equipment_names_the_implementation() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert_eq!(
            text_or_panic(&bytes, 0x0008, 0x1090),
            crate::sr::MANUFACTURER
        );
        assert_eq!(
            text_or_panic(&bytes, 0x0018, 0x1000),
            crate::sr::MANUFACTURER
        );
        assert_eq!(
            text_or_panic(&bytes, 0x0018, 0x1020),
            implementation_version_name()
        );
    }

    #[test]
    fn the_content_label_is_the_series_description_and_never_empty() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert_eq!(text_or_panic(&bytes, 0x0070, 0x0080), "Masks");
        // (0070,0081) is Type 2, so it is present and legitimately empty.
        assert_eq!(text_or_panic(&bytes, 0x0070, 0x0081), "Masks");
    }

    #[test]
    fn the_segment_carries_its_algorithm_name() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert_eq!(text_or_panic(&bytes, 0x0062, 0x0009), "threshold");
    }

    #[test]
    fn the_derivation_names_the_segmentation_code_and_the_source_purpose() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert!(find(&bytes, 0x0008, 0x9215).is_some(), "(0008,9215)");
        assert!(find(&bytes, 0x0040, 0xA170).is_some(), "(0040,A170)");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("113076"), "the derivation code value");
        assert!(text.contains("121322"), "the purpose code value");
        assert!(text.contains("Segmentation"), "the derivation meaning");
        assert!(
            text.contains("Source Image for Image Processing Operation"),
            "the purpose meaning"
        );
    }

    #[test]
    fn the_referenced_series_groups_its_instances() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert!(find(&bytes, 0x0008, 0x1115).is_some(), "(0008,1115)");
        assert!(find(&bytes, 0x0008, 0x114A).is_some(), "(0008,114A)");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("1.2.3.4.9"), "the source series UID");
    }

    #[test]
    fn two_source_series_get_two_reference_items() {
        let mut seg = identity(vec![vec![1, 2], vec![3, 4]], 2);
        seg.frames[1].source.series_uid = "1.2.3.4.99".to_string();
        let bytes = serialize(&seg);
        // Two items in the Referenced Series Sequence, so the second
        // series UID appears; a writer that keyed on the first series for
        // everything would drop it.
        assert!(
            String::from_utf8_lossy(&bytes).contains("1.2.3.4.99"),
            "the second source series"
        );
    }

    #[test]
    fn the_dimension_module_points_at_the_frame_index_values() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert!(find(&bytes, 0x0020, 0x9221).is_some(), "(0020,9221)");
        assert!(find(&bytes, 0x0020, 0x9222).is_some(), "(0020,9222)");
        assert!(find(&bytes, 0x0020, 0x9157).is_some(), "(0020,9157)");
        // (0020,9165) Dimension Index Pointer, AT, targeting (0020,9157).
        let at = find(&bytes, 0x0020, 0x9165).expect("(0020,9165)");
        assert_eq!(&bytes[at + 4..at + 6], b"AT");
        assert_eq!(&bytes[at + 8..at + 12], &[0x20, 0x00, 0x57, 0x91]);
        // (0020,9167) Functional Group Pointer, AT, targeting Frame Content.
        let at = find(&bytes, 0x0020, 0x9167).expect("(0020,9167)");
        assert_eq!(&bytes[at + 8..at + 12], &[0x20, 0x00, 0x11, 0x91]);
    }

    #[test]
    fn the_frame_content_indexes_the_frames_from_one() {
        let bytes = serialize(&identity(vec![vec![1, 2], vec![3, 4]], 2));
        // Two Frame Content items, one per frame. Each carries (0020,9157)
        // as a DS whose value is the 1-based frame index, so the rendered
        // object must contain both values as whole DS elements. The check
        // is on the DS elements themselves: a byte scan for the tag pair
        // can match inside Pixel Data, so the sequence is walked instead.
        let at = find(&bytes, 0x0020, 0x9111).expect("(0020,9111) Frame Content");
        let mut seen: Vec<String> = Vec::new();
        // Walk forward from the first Frame Content item, reading every
        // (0020,9157) DS until the per-frame sequence ends. The tag is
        // little endian in an explicit VR header, so it is found by its
        // own 8-byte signature rather than a 4-byte one.
        let mut cursor = at;
        while cursor + 8 <= bytes.len() {
            if bytes[cursor..cursor + 4] == [0x20, 0x00, 0x57, 0x91]
                && bytes[cursor + 4..cursor + 6] == *b"DS"
            {
                let length = u16::from_le_bytes([bytes[cursor + 6], bytes[cursor + 7]]) as usize;
                if cursor + 8 + length > bytes.len() {
                    break;
                }
                seen.push(
                    String::from_utf8_lossy(&bytes[cursor + 8..cursor + 8 + length])
                        .trim_end_matches(['\0', ' '])
                        .to_string(),
                );
                cursor += 8 + length;
                continue;
            }
            cursor += 1;
        }
        assert_eq!(seen, ["1", "2"], "one index per frame, from one");
    }

    #[test]
    fn the_shared_group_carries_the_source_orientation() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        // (0020,9116) Plane Orientation (Patient) Sequence, one item whose
        // (0020,0037) copies the source's six direction cosines.
        let at = find(&bytes, 0x0020, 0x9116).expect("(0020,9116)");
        assert_eq!(&bytes[at + 4..at + 6], b"SQ", "(0020,9116) is a sequence");
        let item = find(&bytes[at..], 0x0020, 0x0037).expect("(0020,0037)");
        let at = at + item;
        assert_eq!(&bytes[at + 4..at + 6], b"DS");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        assert_eq!(
            String::from_utf8_lossy(&bytes[at + 8..at + 8 + length]).trim_end(),
            "1\\0\\0\\0\\1\\0",
            "the source orientation is copied verbatim"
        );
        // A source with no orientation must not invent one.
        let mut without = identity(vec![vec![1, 2]], 1);
        without.frames[0].source.orientation = None;
        let bytes = serialize(&without);
        assert!(
            find(&bytes, 0x0020, 0x9116).is_none(),
            "no orientation is written when the source declared none"
        );
    }

    #[test]
    fn the_creation_date_and_time_are_dates_not_uids() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        for tag in [(0x0008u16, 0x0012u16), (0x0008, 0x0021)] {
            let at = find(&bytes, tag.0, tag.1).expect("(0008,0012)/(0008,0021)");
            assert_eq!(&bytes[at + 4..at + 6], b"DA", "creation date is DA");
            let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
            let value = String::from_utf8_lossy(&bytes[at + 8..at + 8 + length]);
            assert_eq!(value.len(), 8, "creation date is YYYYMMDD, got {value:?}");
            assert!(value.chars().all(|c| c.is_ascii_digit()));
        }
        for tag in [(0x0008u16, 0x0013u16), (0x0008, 0x0031)] {
            let at = find(&bytes, tag.0, tag.1).expect("(0008,0013)/(0008,0031)");
            assert_eq!(&bytes[at + 4..at + 6], b"TM", "creation time is TM");
        }
    }

    #[test]
    fn the_pixel_measures_carry_the_source_spacing() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        // Read at the element's own offset: "2.5" also appears in a plane
        // position, so a substring search would be satisfied by the wrong
        // element and could not tell a copied spacing from a coincidental
        // number elsewhere in the object.
        let at = find(&bytes, 0x0018, 0x0088).expect("(0018,0088)");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        assert_eq!(
            String::from_utf8_lossy(&bytes[at + 8..at + 8 + length]).trim_end(),
            "2.5",
            "the spacing between slices"
        );
        // (0028,0030) appears once per frame and once in the shared group;
        // every occurrence has to carry the copied spacing.
        let spacing = [0x28u8, 0x00, 0x30, 0x00];
        let mut total = 0;
        while let Some(found) = find_subslice(&bytes[total..], &spacing) {
            let at = total + found;
            let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
            assert_eq!(
                String::from_utf8_lossy(&bytes[at + 8..at + 8 + length]).trim_end(),
                "0.7\\0.7",
                "every Pixel Measures carries the source spacing"
            );
            total = at + 4;
        }
        assert!(total > 0, "Pixel Measures was written");
    }

    #[test]
    fn a_frame_without_spacing_falls_back_to_a_unit_spacing() {
        let mut seg = identity(vec![vec![1, 2]], 1);
        seg.pixel_spacing = None;
        seg.slice_spacing = None;
        let bytes = serialize(&seg);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("1\\1"), "the unit spacing fallback");
        assert!(find(&bytes, 0x0018, 0x0088).is_none(), "no slice spacing");
    }

    #[test]
    fn the_plane_position_is_copied_per_frame() {
        let bytes = serialize(&identity(vec![vec![1, 2], vec![3, 4]], 2));
        assert!(find(&bytes, 0x0020, 0x9113).is_some(), "(0020,9113)");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("1.5\\2.5\\0"), "frame 1's position");
        assert!(text.contains("1.5\\2.5\\3"), "frame 2's position");
    }

    #[test]
    fn a_frame_without_a_position_has_no_plane_position_item() {
        let mut seg = identity(vec![vec![1, 2]], 1);
        seg.frames[0].source.position = None;
        let bytes = serialize(&seg);
        assert!(find(&bytes, 0x0020, 0x9113).is_none());
    }

    #[test]
    fn the_segmentation_functional_group_names_the_segment() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert!(find(&bytes, 0x0062, 0x000A).is_some(), "(0062,000A)");
        assert!(find(&bytes, 0x0062, 0x000B).is_some(), "(0062,000B)");
    }

    #[test]
    fn two_segments_get_one_frame_each_named_after_its_segment() {
        let mut seg = identity(vec![vec![1, 2]], 1);
        seg.segments.push(seg.segments[0].clone());
        let mut second = seg.frames[0].clone();
        second.segment_number = 2;
        second.mask = vec![3, 4];
        seg.frames.push(second);
        let bytes = serialize(&seg);
        // (0028,0008) Number of Frames counts (source, segment) pairs.
        assert_eq!(text_or_panic(&bytes, 0x0028, 0x0008), "2");
        // Two (0062,000B) Referenced Segment Number elements, values 1
        // and 2, each inside its own per-frame item.
        let mut seen: Vec<u16> = Vec::new();
        let mut cursor = 0;
        let needle = [0x62u8, 0x00, 0x0B, 0x00, b'U', b'S', 0x02, 0x00];
        while let Some(found) = find_subslice(&bytes[cursor..], &needle) {
            let at = cursor + found;
            seen.push(u16::from_le_bytes([bytes[at + 8], bytes[at + 9]]));
            cursor = at + 4;
        }
        assert_eq!(seen, [1, 2], "one frame per segment, in segment order");
    }

    #[test]
    fn the_segments_overlap_is_written_only_when_declared() {
        // Undeclared: the attribute is omitted rather than guessed, so a
        // reader knows to check every pixel.
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert!(
            find(&bytes, 0x0062, 0x0013).is_none(),
            "absent when undeclared"
        );
        // Declared: copied verbatim - nested masks are "YES".
        let mut seg = identity(vec![vec![1, 2]], 1);
        seg.segments_overlap = Some("YES".to_string());
        let bytes = serialize(&seg);
        assert_eq!(text_or_panic(&bytes, 0x0062, 0x0013), "YES");
    }

    #[test]
    fn the_general_study_attributes_are_present_and_empty() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        for (group, element) in [
            (0x0008u16, 0x0020u16),
            (0x0008, 0x0030),
            (0x0008, 0x0090),
            (0x0020, 0x1040),
        ] {
            let at = find(&bytes, group, element)
                .unwrap_or_else(|| panic!("({group:04X},{element:04X}) is absent"));
            assert_eq!(
                u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]),
                0,
                "({group:04X},{element:04X}) is zero length"
            );
        }
    }

    #[test]
    fn the_original_specialized_sop_class_uid_is_not_written() {
        let bytes = serialize(&identity(vec![vec![1, 2]], 1));
        assert!(
            find(&bytes, 0x0008, 0x001B).is_none(),
            "a segmentation is not derived from a specialized class"
        );
    }
}
