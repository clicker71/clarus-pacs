// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The RT Structure Set writer: the PS3.3 A.19 IOD, SOP Class ...481.3.
//!
//! # What it writes
//!
//! One structure set per study: a set of ROIs, each drawn by closed planar
//! polygons in PATIENT millimetres. The ABI hands over `(column, row)`
//! pixel pairs keyed by the manifest's 1-based frame index, because that is
//! the only coordinate space the workdir manifest can express; this writer
//! converts each point with Equation C.7.6.2.1-1 using the frame's own
//! `(0020,0032)`, `(0020,0037)` and `(0028,0030)`, which the worker already
//! holds per frame.
//!
//! # Why the conversion lives here and not in the script
//!
//! A processor asked for millimetres would have to fetch every instance a
//! second time (the manifest carries no geometry) and re-implement the
//! DICOM geometry rule in a language clinfer does not control - which is a
//! new failure mode where the script's idea of a slice disagrees with this
//! sidecar's. Neither is necessary: clinfer fetched the frames and has the
//! geometry in hand, so it converts once, in one function, unit-tested
//! against the equation.
//!
//! # The four wire facts
//!
//! Each is a regression pin a named test asserts:
//!
//! - P1 ContourNumber (3006,0048) is always written and always unique
//!   within its Contour Sequence. It is Type 3 in 2026d, but the same table
//!   row carries a value constraint ("shall be unique within the Contour
//!   Sequence") and a viewer without it shows "Operation failed".
//! - P2 ROIDisplayColor (3006,002A) is an IS triplet of integers 0..=255.
//! - P3 ContourData is patient millimetres through Equation C.7.6.2.1-1,
//!   with the standard pairing: the column index scales with the COLUMN
//!   resolution and the ROW direction cosine, the row index with the ROW
//!   resolution and the COLUMN direction cosine.
//! - P4 the reference chain 3006,0010 -> 3006,0012 -> 3006,0014 ->
//!   3006,0016, whose INNER (0008,1150) is CT Image Storage while the OUTER
//!   (3006,0012) item's (0008,1150) is the Study class - the C.8.8.5.4 trap.
//!
//! # Write order
//!
//! Strictly ascending tags. That is not a preference: both the ROI Contour
//! and the RT ROI Observations sequences reference an ROI by
//! `ReferencedROINumber` (3006,0084), and the ROI's identity is established
//! by (3006,0022) in Structure Set ROI Sequence. Ascending order produces
//! 3006,0020 < 3006,0039 < 3006,0080 by itself, so the sequences are
//! emitted in the order they are built rather than assembled and sorted.

use crate::abi::schema::RtRoi;
use crate::pr::{
    now_stamp, write_cs, write_da, write_ds, write_explicit_header, write_is, write_item_end,
    write_item_start, write_lo, write_pn, write_sequence_end, write_sequence_start, write_sh,
    write_st, write_string, write_tm, write_ui,
};
use crate::sr::IMPLEMENTATION_CLASS_UID;
use crate::sr::implementation_version_name;

/// RT Structure Set Storage (PS3.3 A.19).
pub const RT_STRUCT_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.481.3";

/// Explicit VR Little Endian, the transfer syntax every writer emits.
pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";

/// The character set the copied patient identity is declared in.
pub const SPECIFIC_CHARACTER_SET: &str = crate::sr::SPECIFIC_CHARACTER_SET;

/// The SOP Class named by the (3006,0012) study item's (0008,1150).
///
/// DETACHED STUDY MANAGEMENT SOP CLASS, `1.2.840.10008.3.1.2.3.1`
/// (PS3.4 K.1). C.8.8.5.4 says the item's class is that "of the Study
/// itself, not of the Instances within the Study", and the Study itself - a
/// study is not an object that was ever stored - is named by the Detached
/// Study Management class. RayStation, Eclipse and dicom-tools all write
/// this value in (3006,0012), and viewers key on the class to file the
/// object as a structure set rather than as a segmentation.
///
/// The (0008,1155) beside it is the Study Instance UID, because
/// (3006,0012) references the STUDY. The (0008,0016) SOP Class of this
/// structure set is NOT what goes here.
///
/// Two tempting mistakes, both wrong:
///
/// - CT Image Storage, because the INNER (3006,0016) Contour Image items
///   ARE CT Image Storage. The two positions are different facts.
/// - `1.2.840.10008.3.1.1.1`, which is MODALITY WORKLIST INFORMATION
///   MODEL - FIND (PS3.4 K.6), a query model rather than a storage class.
///   Writing it here is what this constant used to do, and a viewer that
///   trusted it classified the object by a query model.
pub const STUDY_SOP_CLASS_UID: &str = "1.2.840.10008.3.1.2.3.1";

/// The geometric type every contour this writer emits carries.
///
/// CLOSED_PLANAR only: a hole is expressed as the keyhole shape in the
/// polygons themselves (C.8.8.6.3 offers both the XOR technique and the
/// keyhole technique). Supporting CLOSEDPLANAR_XOR would mean a per-ROI
/// declaration and an encoder branch, and nothing in the target viewers
/// needs it - so an ROI that mixes the two geometric types cannot be
/// expressed, by construction.
pub const CONTOUR_GEOMETRIC_TYPE: &str = "CLOSED_PLANAR";

/// (0020,0011) Series Number of the series this writer emits.
///
/// The result series in one study are numbered apart, because a viewer
/// lists a study's series BY NUMBER and two series that share one are two
/// rows a reader cannot tell apart. The scheme across the writers is:
///
/// ```text
///   1  the source acquisition series, which the caller reproduces
///   2  the SEG writer's series        (seg::SERIES_NUMBER)
///   3  the RTSTRUCT writer's series   (this constant)
/// ```
///
/// 1 is the source by convention and is fixed, not defaulted: the CT that
/// was sent always carries it. 2 and 3 then separate the two derived
/// series, which is the collision this constant exists to remove - both
/// writers used to write 1, so a segmentation and a structure set derived
/// from one study looked like two copies of the source series.
pub const SERIES_NUMBER: &str = "3";

/// The last-resort (3006,0002) Structure Set Label.
///
/// Type 1, so it must never be empty. Used when a derived candidate would be
/// empty: an absent model id, or a cut that leaves nothing. The value is the
/// IOD's own modality word, which is what a reader sees for a structure set
/// whose producer could not name itself.
pub const RTSTRUCT_FALLBACK_LABEL: &str = "RTSTRUCT";

/// The value written when a polygon cannot be formatted.
///
/// Unreachable: the caller sizes the buffer with
/// [`contour_buffer_bytes`], which is exactly what [`contour_points_into`]
/// requires. It exists so the fallback is a legal single origin rather than
/// an EMPTY `ContourData`, which is not a value the standard allows and
/// would make the object unreadable.
const EMPTY_CONTOUR_TEXT: &str = "0\\0\\0";

/// One referenced source slice a contour lies on.
///
/// Not `Eq`: the geometry is floats, and claiming an equality this type
/// cannot honour would be a promise the derive does not keep.
#[derive(Debug, Clone, PartialEq)]
pub struct RtSource {
    /// (0008,1150) of the source image, from the frame's own instance.
    pub sop_class_uid: String,
    /// (0008,1155) of the source image.
    pub sop_instance_uid: String,
    /// (0020,000E) of the series the source image belongs to.
    ///
    /// (3006,0014) RT Referenced Series Sequence groups its slices BY
    /// SERIES, so the series UID is what carries a contour back to the
    /// images it was traced on.
    pub series_uid: String,
    /// (0020,0032) Image Position Patient.
    ///
    /// Needed for two things: the contour's z values come from it, and it
    /// is the origin of the pixel-to-patient conversion. A contour with no
    /// position cannot be placed in patient space, so a frame that declares
    /// none is refused by the caller before this writer runs.
    pub position: Option<[f64; 3]>,
    /// (0020,0037) Image Orientation Patient: row cosines, then column.
    pub orientation: Option<[f64; 6]>,
    /// (0028,0030) Pixel Spacing: row resolution, then column resolution.
    ///
    /// The order matters and is the source of the tool's swap: the standard
    /// pairs the COLUMN index with the COLUMN resolution, which is
    /// `PixelSpacing[1]`.
    pub pixel_spacing: Option<[f64; 2]>,
}

/// One contour of one ROI: its polygon in patient millimetres, and the
/// slice it was traced on.
#[derive(Debug, Clone, PartialEq)]
pub struct RtContour {
    /// The patient-space points, already converted.
    pub points: Vec<[f64; 3]>,
    /// The source slice the contour lies on.
    pub source: RtSource,
}

/// One ROI: its declared attributes and its contours.
#[derive(Debug, Clone, PartialEq)]
pub struct RtRoiEntry {
    /// (3006,0022) ROI Number: `position + 1` in the emitted list.
    pub number: u16,
    /// The declared ROI, as the model stated it.
    pub roi: RtRoi,
    /// The contours, in the order the model supplied them.
    pub contours: Vec<RtContour>,
}

/// Everything the caller supplies that this writer cannot derive.
#[derive(Debug, Clone)]
pub struct RtIdentity {
    /// (0008,0018) of the structure set itself.
    pub sop_instance_uid: String,
    /// (0020,000D) of the study the structure set belongs to. COPIED, not
    /// invented: the object is stored into its source study, which is what
    /// makes the Common Instance Reference condition true.
    pub study_uid: String,
    /// (0020,000E) of the series it is stored in.
    pub series_uid: String,
    /// The Frame of Reference UID (0020,0052) of the source images.
    ///
    /// Note 2 of Table A.19.3-1 makes the Frame of Reference module Usage
    /// `U`, so it is written when the source declares one and omitted when
    /// the source declares none. Every ROI's (3006,0024) must equal this
    /// value (C.8.8.5.1), which is why there is exactly one.
    pub frame_of_reference_uid: Option<String>,
    /// (3006,0002) Structure Set Label: SH, non-empty, at most 16 bytes.
    pub label: String,
    /// (3006,0004) Structure Set Name, when the model or the rule names one.
    pub name: Option<String>,
    /// The ROIs, in ROI-number order: entry `i` is ROI number `i + 1`.
    pub rois: Vec<RtRoiEntry>,
    /// (0008,2111) Derivation Description: the one provenance channel the
    /// A.19 modules provide. Unlike SEG, the RT SS IOD has no segment
    /// algorithm name analogue, so free text is the only place a reader can
    /// learn which model produced the structure set.
    pub derivation_description: String,
    /// (0010,0010) copied from the workitem, else the study.
    pub patient_name: Option<String>,
    /// (0010,0020) copied from the workitem, else the study.
    pub patient_id: Option<String>,
    /// (0010,0021) Issuer of Patient ID, workitem-only.
    ///
    /// `Some("")` writes a ZERO-LENGTH element - the workitem explicitly
    /// said "no issuer at this site" and Appendix V copies the value
    /// unaltered. `None` omits it. No fallback: no source on this path
    /// carries an issuer.
    pub patient_id_issuer: Option<String>,
    /// (0010,0030) Patient's Birth Date, Type 2.
    pub patient_birth_date: Option<String>,
    /// (0010,0040) Patient's Sex, Type 2.
    pub patient_sex: Option<String>,
    /// (0020,0010) Study ID, Type 2.
    pub study_id: Option<String>,
    /// (0008,0050) Accession Number, Type 2.
    pub accession_number: Option<String>,
    /// (0008,0090) Referring Physician's Name, Type 2.
    ///
    /// Written EMPTY rather than dropped when neither the workitem nor the
    /// study carries it: Type 2 means present with a possibly empty value,
    /// and dropping it would be a false statement about the workitem.
    pub referring_physician: Option<String>,
    /// (0008,1010) Station Name, from the workitem's (0040,4028).
    ///
    /// `None` omits it - Type 3, and there is no study fallback because
    /// Appendix V Table V-1 Note 4 maps it FROM (0040,4028).
    pub station_name: Option<String>,
    /// (0008,103E) from the rule, when it names one.
    pub series_description: Option<String>,
    /// The distinct source series UIDs, in first appearance order.
    ///
    /// Precomputed by the caller: (3006,0014) needs one item per DISTINCT
    /// source series, and the writer should not walk every contour to
    /// rediscover that set.
    pub source_series: Vec<String>,
    /// The distinct source slices, in first appearance order: one
    /// ContourImageSequence item each.
    ///
    /// Also precomputed: C.8.8.5.1 wants the referenced slices listed once,
    /// not once per contour that touches them.
    pub contour_images: Vec<RtSource>,
}

impl RtIdentity {
    /// Whether this IOD carries pixel data.
    ///
    /// Always false, and it is a method rather than a comment because the
    /// ABSENCE is what distinguishes this writer from the SEG writer: an RT
    /// Structure Set has no Image Pixel module, no Number of Frames and no
    /// Pixel Data. Everything geometric in it lives in the contours.
    #[must_use]
    pub fn has_pixel_data(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// The pixel-to-patient conversion (Equation C.7.6.2.1-1)
// ---------------------------------------------------------------------------

/// Convert one `(column, row)` pixel point into patient millimetres.
///
/// Equation C.7.6.2.1-1 (`C.7.6.2.1@PS3.3`):
///
/// ```text
/// x = ipp[0] + col * dc * iop[0] + row * dr * iop[3]
/// y = ipp[1] + col * dc * iop[1] + row * dr * iop[4]
/// z = ipp[2] + col * dc * iop[2] + row * dr * iop[5]
/// ```
///
/// where `iop[0..3]` is the ROW direction cosine and `iop[3..6]` the COLUMN
/// direction cosine, `dc` the column resolution `PixelSpacing[1]` and `dr`
/// the row resolution `PixelSpacing[0]`.
///
/// That pairing is the standard's: the column index scales with the column
/// resolution and the ROW cosine, the row index with the row resolution and
/// the COLUMN cosine. Pairing `col` with `spacing[0]` instead - which is
/// what the synthetic tool does - agrees only for isotropic pixels, which
/// is why no viewer could tell the difference until a fixture had both a
/// non-identity IOP and anisotropic spacing.
///
/// A frame with no position or no orientation is refused by the CALLER: a
/// contour with no frame of reference has no patient coordinates at all,
/// and inventing an origin would place a clinical structure in the wrong
/// place. The defaults here exist so this function is total, not because a
/// caller is expected to rely on them.
#[must_use]
pub fn to_patient(source: &RtSource, column: f64, row: f64) -> [f64; 3] {
    let ipp = source.position.unwrap_or([0.0; 3]);
    let iop = source.orientation.unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    // PixelSpacing is (row resolution, column resolution) in that order.
    let spacing = source.pixel_spacing.unwrap_or([1.0, 1.0]);
    let dr = spacing[0];
    let dc = spacing[1];
    [
        ipp[0] + column * dc * iop[0] + row * dr * iop[3],
        ipp[1] + column * dc * iop[1] + row * dr * iop[4],
        ipp[2] + column * dc * iop[2] + row * dr * iop[5],
    ]
}

/// The buffer size [`contour_points_into`] needs for `count` points.
///
/// Three DS values per point, each at most 32 bytes as rendered, plus one
/// backslash between values. A DS value is legally at most 16 characters;
/// the 32-byte slot is the scratch width the formatter uses, so this is
/// generous by construction rather than by estimate.
///
/// `const` so a caller can size a STACK buffer at compile time, which is
/// what the serializer does for an ordinary polygon.
#[must_use]
pub const fn contour_buffer_bytes(count: usize) -> usize {
    count * 3 * 32 + count * 3
}

/// Convert a whole polygon, writing `x\y\z\...` into a caller-owned buffer.
///
/// The ONE genuinely new formatter: `ContourData` is a DS of three values
/// per point, so a polygon of N points is 3N values - not the fixed three
/// `triple_into` writes. The rounding rule is `decimal_f64_into`'s:
/// shortest round-trip decimal with trailing zeros trimmed, non-finite
/// rendered as `0`.
///
/// The buffer must be at least [`contour_buffer_bytes`] bytes. Nothing is
/// allocated per point. Returns `None` when the buffer is too small, which
/// is a caller error rather than a data error.
#[must_use]
pub fn contour_points_into<'a>(buffer: &'a mut [u8], points: &[[f64; 3]]) -> Option<&'a str> {
    if buffer.len() < contour_buffer_bytes(points.len()) {
        return None;
    }
    let mut offset = 0;
    let mut scratch = [0u8; 32];
    for (index, point) in points.iter().enumerate() {
        for (component, value) in point.iter().enumerate() {
            if index > 0 || component > 0 {
                buffer[offset] = b'\\';
                offset += 1;
            }
            let text = decimal_f64_into(&mut scratch, *value);
            buffer[offset..offset + text.len()].copy_from_slice(text.as_bytes());
            offset += text.len();
        }
    }
    std::str::from_utf8(&buffer[..offset]).ok()
}

/// One f64 as DS text with trailing zeros trimmed, into a caller-owned
/// buffer.
///
/// The allocation-free twin of [`seg`]'s `decimal_text`, for the same
/// reason: a polygon is converted value by value and a `String` per value
/// would be an allocation per value. Both go through `pr::ds_text_into`,
/// so the contour coordinates and the functional-group values are rendered
/// by one implementation and therefore cannot disagree about the sixteen
/// characters DS allows.
///
/// Contour coordinates are patient-space millimetres, where a thousandth of
/// a millimetre is far below the resolution anything acquired them at.
fn decimal_f64_into(buffer: &mut [u8; 32], value: f64) -> &str {
    let mut wide = [0u8; crate::pr::DS_MAX_CHARS * 2];
    let text = crate::pr::ds_text_into(&mut wide, value, 6, 3);
    let len = text.len().min(buffer.len());
    buffer[..len].copy_from_slice(&text.as_bytes()[..len]);
    std::str::from_utf8(&buffer[..len]).unwrap_or("0")
}

// ---------------------------------------------------------------------------
// The serializer
// ---------------------------------------------------------------------------

/// Serialize one structure set as a Part-10 file.
///
/// `identity.rois[i]` carries ROI number `i + 1`, which is the same
/// "position + 1" rule that binds a segment to a segment number and a mask
/// bit to its segment. A structure set whose every ROI vanished cannot reach
/// here: the caller checks `has_rtstruct()` before calling, and the schema
/// refuses an empty ROI array outright.
///
/// # Errors
///
/// A contour whose `ContourData` would exceed the 16-bit length of its own
/// VR. `ContourData` is a DS - a SHORT VR - so its value field is at most
/// 65534 bytes however many points a processor supplies, and a polygon past
/// that has no correct encoding as one DS element. This is reported rather
/// than truncated: a truncated length leaves the value bytes to be read as
/// the next element's VR, which is a file that parses into garbage instead
/// of failing, and an RS that looks complete while carrying a mangled
/// contour is worse than no RS at all. There is no place in an RT Structure
/// Set to record "a contour was dropped here", so dropping one silently is
/// not an option either; the caller cancels the workitem and reports the
/// reason.
pub fn serialize(identity: &RtIdentity) -> Result<Vec<u8>, String> {
    let mut dataset = Vec::with_capacity(4096);

    // -- What this object is ------------------------------------------
    // TOP-LEVEL ELEMENTS ARE WRITTEN IN ASCENDING TAG ORDER. PS3.5 7.1 makes
    // that the order of a data set, and a reader is entitled to rely on it;
    // the writer used to emit the General Study rows, then the Patient
    // rows, then (0020,000D), which is out of order three times over.
    //
    // The grouping below is by MODULE for the comments' sake only; each
    // write lands where the tag order puts it.
    //
    // (0008,0005) Specific Character Set FIRST: the character set
    // declaration has to precede the bytes it governs, and the RS copies a
    // patient identity that can be Cyrillic or CJK. It is also the lowest
    // top-level tag, so the two rules agree.
    write_cs(&mut dataset, 0x0008, 0x0005, SPECIFIC_CHARACTER_SET);
    // (0008,0012)/(0008,0013) Instance Creation Date/Time (C.12.1).
    let (date, time) = now_stamp();
    write_da(&mut dataset, 0x0008, 0x0012, &date);
    write_tm(&mut dataset, 0x0008, 0x0013, &time);
    write_ui(&mut dataset, 0x0008, 0x0016, RT_STRUCT_SOP_CLASS_UID);
    write_ui(&mut dataset, 0x0008, 0x0018, &identity.sop_instance_uid);
    // (0008,001B) Original Specialized SOP Class UID is not written: it is
    // defined for objects derived from one of the SPECIALIZED classes, and
    // an A.19 structure set is not one.
    // (0008,0020)/(0008,0030) Study Date/Time are Type 2 and the study's
    // own, which this writer is not given: the RS summarises an existing
    // study rather than being acquired during one.
    // (0008,0021)/(0008,0031) Series Date/Time: the series is this
    // writer's own, created at the same instant as the instance.
    //
    // The pairs are interleaved because they SORT that way: (0008,0020),
    // (0008,0021), (0008,0030), (0008,0031).
    write_da(&mut dataset, 0x0008, 0x0020, "");
    write_da(&mut dataset, 0x0008, 0x0021, &date);
    write_tm(&mut dataset, 0x0008, 0x0030, "");
    write_tm(&mut dataset, 0x0008, 0x0031, &time);
    write_sh(
        &mut dataset,
        0x0008,
        0x0050,
        identity.accession_number.as_deref().unwrap_or(""),
    );
    // (0008,0060) Modality is fixed by the IOD, not chosen: C.8.8.1.1 says
    // "Enumerated Values if RT Structure Set IOD: RTSTRUCT". Note that the
    // A.19.3 table selects the RT Series module and NOT General Series, so
    // General Series rows such as Laterality are deliberately not written.
    write_cs(&mut dataset, 0x0008, 0x0060, "RTSTRUCT");
    write_lo(&mut dataset, 0x0008, 0x0070, crate::sr::MANUFACTURER);
    // (0008,0090) is Type 2: written EMPTY rather than dropped, on the same
    // rule the SEG writer documents.
    write_pn(
        &mut dataset,
        0x0008,
        0x0090,
        identity.referring_physician.as_deref().unwrap_or(""),
    );
    // (0008,103E) Series Description - an A.19.3 "Source Copy" row, written
    // only when the caller copied one. The Study Description row is not
    // carried on this identity type, so nothing is written for it rather
    // than a guess.
    if let Some(description) = &identity.series_description {
        write_lo(&mut dataset, 0x0008, 0x103E, description);
    }
    // (0008,1070) Operators' Name is deliberately NOT written. PS3.3
    // Table C.7-8 lists no such row for the General Equipment Module - the
    // group runs (0008,1041) then (0008,1090) - so the attribute is not part
    // of this IOD at all, and writing it would attribute the object to an
    // operator who does not exist. The device is named by (0008,1090) below.
    // (0008,1115) Referenced Series Sequence, Common Instance Reference
    // (C.12.2): Type 1C, conditioned on "Required if this Instance
    // references Instances in this Study". The RS references slices of its
    // own study, so the condition holds and both the sequence and its inner
    // instance list are mandatory.
    //
    // It is written HERE because (0008,1115) sorts among the (0008,xxxx)
    // rows, not after the (3006,xxxx) structure-set sequences where it
    // used to be emitted.
    //
    // (0008,2111) Derivation Description belongs to the same group: General
    // Reference (C.12.4) is Usage U with every row Type 3, and this text is
    // the only provenance channel A.19 provides. Source Image Sequence from
    // that module is deliberately NOT written: it would duplicate every
    // referenced slice a second time, and the structure already carries the
    // reference with more precision, per contour, through
    // ContourImageSequence. Because (0042,0013) inside it is therefore
    // absent, the A.19.4 CID 7019 constraint has nothing to bind and no
    // partial version of it is written either.
    // (0008,1090) Manufacturer's Model Name: Enhanced General Equipment is
    // Type 1 in the Enhanced RT Structure Set IOD, so it always names the
    // device that wrote the object.
    write_lo(&mut dataset, 0x0008, 0x1090, &implementation_version_name());
    write_referenced_series(&mut dataset, identity);
    // (0008,2111) Derivation Description: the RS is a derived object and
    // this text is the only provenance channel A.19 provides.
    write_st(
        &mut dataset,
        0x0008,
        0x2111,
        &identity.derivation_description,
    );

    // -- Patient (C.7.1.1) --------------------------------------------
    // All four Type-2 rows are written even when empty: Type 2 means
    // present with a possibly empty value, and omitting the element would
    // claim the writer never considered it.
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
    // (0010,0021) Issuer of Patient ID - AIW-I Appendix V Table V-1
    // "Issuer of Patient ID Macro" ("Source Copy"). `Some("")` writes the
    // zero-length element (the workitem said "no issuer at this site");
    // `None` omits it. No fallback: no source on this path carries one.
    if let Some(issuer) = &identity.patient_id_issuer {
        write_lo(&mut dataset, 0x0010, 0x0021, issuer);
    }
    write_da(
        &mut dataset,
        0x0010,
        0x0030,
        identity.patient_birth_date.as_deref().unwrap_or(""),
    );
    write_cs(
        &mut dataset,
        0x0010,
        0x0040,
        identity.patient_sex.as_deref().unwrap_or(""),
    );

    // -- General Equipment (C.7.5.1) ----------------------------------
    // (0008,0080)/(0008,1040) are Type 3 and nothing on this path observes
    // them, so they are omitted rather than filled with a guess.
    write_lo(&mut dataset, 0x0018, 0x1020, &implementation_version_name());

    // -- General Study / RT Series identity (C.7.2.1, C.8.8.1) --------
    write_ui(&mut dataset, 0x0020, 0x000D, &identity.study_uid);
    write_ui(&mut dataset, 0x0020, 0x000E, &identity.series_uid);
    write_sh(
        &mut dataset,
        0x0020,
        0x0010,
        identity.study_id.as_deref().unwrap_or(""),
    );
    // (0020,0011) Series Number: this writer's own number, so the structure
    // set is not listed as a copy of the source series. See SERIES_NUMBER
    // for the scheme across the writers.
    write_is(&mut dataset, 0x0020, 0x0011, SERIES_NUMBER);
    // (0020,0013) Instance Number: one structure set per study, so 1.
    write_is(&mut dataset, 0x0020, 0x0013, "1");

    // -- Frame of Reference (C.7.4.1) ---------------------------------
    // Usage U (Table A.19.3-1 Note 2): written when the source declares a
    // Frame of Reference and omitted when it declares none. An invented
    // value would claim the contours live in a coordinate system nothing
    // else knows about.
    if let Some(frame_of_reference) = &identity.frame_of_reference_uid {
        write_ui(&mut dataset, 0x0020, 0x0052, frame_of_reference);
    }
    // (0020,1040) Position Reference Indicator is Type 2: present but
    // legitimately empty, because the source study's table position is not
    // something this writer is given.
    write_lo(&mut dataset, 0x0020, 0x1040, "");

    // -- Structure Set (C.8.8.5) --------------------------------------
    // (3006,0002) is Type 1: it names the object for a human, and the
    // caller guarantees a non-empty, length-bounded value.
    write_sh(&mut dataset, 0x3006, 0x0002, &identity.label);
    if let Some(name) = &identity.name {
        write_lo(&mut dataset, 0x3006, 0x0004, name);
    }
    // (3006,0008)/(3006,0009) are Type 2: the structure set's own date and
    // time, which is the instant it was computed - the same instant SOP
    // Common's Instance Creation pair above carries.
    write_da(&mut dataset, 0x3006, 0x0008, &date);
    write_tm(&mut dataset, 0x3006, 0x0009, &time);

    // (3006,0010) Referenced Frame of Reference Sequence: EXACTLY ONE
    // item. C.8.8.5.1: "each Frame of Reference used to express the
    // coordinates of an ROI shall be listed in the Referenced Frame of
    // Reference Sequence (3006,0010) once and only once." clinfer walks one
    // matched series set of one study, so there is one FoR and one item.
    write_referenced_frame_of_reference(&mut dataset, identity);

    // (3006,0020) Structure Set ROI Sequence: one item per ROI. This MUST
    // precede the two sequences below, because they reference an ROI by
    // (3006,0084) and the ROI's identity is established here by
    // (3006,0022). Ascending tag order gives that for free.
    write_sequence_start(&mut dataset, 0x3006, 0x0020);
    for entry in &identity.rois {
        write_roi_item(&mut dataset, identity, entry);
    }
    write_sequence_end(&mut dataset);

    // (3006,0039) ROI Contour Sequence.
    write_sequence_start(&mut dataset, 0x3006, 0x0039);
    for entry in &identity.rois {
        write_roi_contour_item(&mut dataset, entry)?;
    }
    write_sequence_end(&mut dataset);

    // (3006,0080) RT ROI Observations Sequence.
    write_sequence_start(&mut dataset, 0x3006, 0x0080);
    for entry in &identity.rois {
        write_rt_roi_observation_item(&mut dataset, entry);
    }
    write_sequence_end(&mut dataset);

    Ok(finish(identity, dataset))
}

/// Wrap the dataset in the Part-10 file meta group.
fn finish(identity: &RtIdentity, dataset: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(dataset.len() + 512);
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");
    let mut meta = Vec::with_capacity(256);
    write_string(&mut meta, 0x0002, 0x0001, b"OB", "01");
    write_string(&mut meta, 0x0002, 0x0002, b"UI", RT_STRUCT_SOP_CLASS_UID);
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

/// (3006,0010) Referenced Frame of Reference Sequence: exactly one item.
///
/// The chain C.8.8.5.1 requires, innermost last:
///
/// ```text
/// (3006,0010) ReferencedFrameOfReferenceSequence   one item
///   (0020,0052) FrameOfReferenceUID                = the source's FoR
///   (3006,0012) RTReferencedStudySequence          one item
///     (0008,1150) ReferencedSOPClassUID            = 1.2.840.10008.3.1.2.3.1
///     (0008,1155) ReferencedSOPInstanceUID         = the study UID
///     (3006,0014) RTReferencedSeriesSequence       one item per series
///       (0020,000E) SeriesInstanceUID              = the source series
///       (3006,0016) ContourImageSequence           one item per slice
///         (0008,1150) ReferencedSOPClassUID         = CT Image Storage
///         (0008,1155) ReferencedSOPInstanceUID      = the slice UID
/// ```
///
/// The two SOP Class positions are DIFFERENT facts and C.8.8.5.4 is
/// emphatic about the outer one: the study item's (0008,1150) is the SOP
/// Class of the STUDY, "not of the Instances within the Study ... even if
/// all of the Instances within the Study are of the same Instance SOP
/// Class". The inner items ARE CT Image Storage, which is exactly what
/// makes writing CT Image Storage in the outer position such an easy
/// mistake. The outer value is spelled out in [`STUDY_SOP_CLASS_UID`], which
/// also names the query model that value must NOT be.
fn write_referenced_frame_of_reference(dataset: &mut Vec<u8>, identity: &RtIdentity) {
    write_sequence_start(dataset, 0x3006, 0x0010);
    write_item_start(dataset);
    // (0020,0052) Frame of Reference UID. Inside the referenced-FoR item
    // it is the SOURCE's value, unchanged, and it is the same value every
    // ROI carries in (3006,0024).
    if let Some(frame_of_reference) = &identity.frame_of_reference_uid {
        write_ui(dataset, 0x0020, 0x0052, frame_of_reference);
    }
    // (3006,0012) RT Referenced Study Sequence.
    write_sequence_start(dataset, 0x3006, 0x0012);
    write_item_start(dataset);
    // The STUDY SOP class: Detached Study Management, never CT Image
    // Storage and never the Modality Worklist FIND model. See
    // STUDY_SOP_CLASS_UID.
    write_ui(dataset, 0x0008, 0x1150, STUDY_SOP_CLASS_UID);
    write_ui(dataset, 0x0008, 0x1155, &identity.study_uid);
    // (3006,0014) RT Referenced Series Sequence: one item per distinct
    // source series, each carrying the slices of that series.
    write_sequence_start(dataset, 0x3006, 0x0014);
    for series_uid in &identity.source_series {
        write_item_start(dataset);
        write_ui(dataset, 0x0020, 0x000E, series_uid);
        // (3006,0016) Contour Image Sequence: the referenced slices OF THIS
        // SERIES. A study's frames can span series, and a reference item
        // cannot: the sequence groups BY SERIES.
        write_sequence_start(dataset, 0x3006, 0x0016);
        for source in identity
            .contour_images
            .iter()
            .filter(|source| &source.series_uid == series_uid)
        {
            write_item_start(dataset);
            // The INNER class IS the instance's own class (CT Image
            // Storage on this path), which is the opposite rule from the
            // study item above.
            write_ui(
                dataset,
                0x0008,
                0x1150,
                if source.sop_class_uid.is_empty() {
                    CT_IMAGE_STORAGE_UID
                } else {
                    &source.sop_class_uid
                },
            );
            write_ui(dataset, 0x0008, 0x1155, &source.sop_instance_uid);
            write_item_end(dataset);
        }
        write_sequence_end(dataset);
        write_item_end(dataset);
    }
    write_sequence_end(dataset);
    write_item_end(dataset);
    write_sequence_end(dataset);
    write_item_end(dataset);
    write_sequence_end(dataset);
}

/// (0008,1115) Referenced Series Sequence with its inner instance list.
///
/// Same grouping rule as the frame-of-reference chain, and it is the same
/// fact written twice on purpose: C.12.2 carries the study-level reference
/// a reader follows to find the images this object was derived from.
fn write_referenced_series(dataset: &mut Vec<u8>, identity: &RtIdentity) {
    write_sequence_start(dataset, 0x0008, 0x1115);
    for series_uid in &identity.source_series {
        write_item_start(dataset);
        write_ui(dataset, 0x0020, 0x000E, series_uid);
        write_sequence_start(dataset, 0x0008, 0x114A);
        for source in identity
            .contour_images
            .iter()
            .filter(|source| &source.series_uid == series_uid)
        {
            write_item_start(dataset);
            write_ui(
                dataset,
                0x0008,
                0x1150,
                if source.sop_class_uid.is_empty() {
                    CT_IMAGE_STORAGE_UID
                } else {
                    &source.sop_class_uid
                },
            );
            write_ui(dataset, 0x0008, 0x1155, &source.sop_instance_uid);
            write_item_end(dataset);
        }
        write_sequence_end(dataset);
        write_item_end(dataset);
    }
    write_sequence_end(dataset);
}

/// (3006,0020) Structure Set ROI Sequence: one item per ROI.
fn write_roi_item(dataset: &mut Vec<u8>, identity: &RtIdentity, entry: &RtRoiEntry) {
    write_item_start(dataset);
    // (3006,0022) ROI Number: 1..N with no gaps, assigned as position + 1.
    //
    // Formatted into a stack buffer rather than through `to_string`: this
    // is the per-ROI path, and an allocation here would cost one heap
    // round-trip per ROI for a two-digit number.
    let mut number_buffer = [0u8; 20];
    write_is(
        dataset,
        0x3006,
        0x0022,
        is_into(&mut number_buffer, usize::from(entry.number)),
    );
    // (3006,0024) Referenced Frame of Reference UID must EQUAL the outer
    // (0020,0052): C.8.8.5.1 requires every Frame of Reference an ROI's
    // coordinates use to be listed in (3006,0010), and that sequence has
    // exactly one item. A model naming a different frame does not know
    // which frame it is in, so the caller refuses it rather than writing a
    // dangling reference.
    if let Some(frame_of_reference) = &identity.frame_of_reference_uid {
        write_ui(dataset, 0x3006, 0x0024, frame_of_reference);
    }
    write_lo(dataset, 0x3006, 0x0026, &entry.roi.name);
    if let Some(description) = &entry.roi.description {
        write_st(dataset, 0x3006, 0x0028, description);
    }
    write_cs(dataset, 0x3006, 0x0036, &entry.roi.generation_algorithm);
    write_item_end(dataset);
}

/// (3006,0039) ROI Contour Sequence: one item per ROI.
///
/// # Errors
///
/// A contour whose ContourData exceeds the 16-bit length of a DS, which has
/// no correct encoding and is reported rather than truncated. See
/// [`ensure_contour_fits`].
fn write_roi_contour_item(dataset: &mut Vec<u8>, entry: &RtRoiEntry) -> Result<(), String> {
    write_item_start(dataset);
    // (3006,002A) ROI Display Color: THREE IS VALUES, each 0..=255 per
    // C.8.8.6 ("RGB triplet color representation for ROI, specified using
    // the range 0-255"). The VR is IS, so each channel is an integer
    // string: a fractional value would have a reader see a truncated
    // integer, which is why the schema refuses one rather than rounding.
    //
    // (3006,002A) precedes (3006,0084) because PS3.5 7.1 orders the elements
    // inside an item too, not only at the top level.
    let mut color = [0u8; 32];
    let text = color_into(&mut color, entry.roi.color);
    write_is(dataset, 0x3006, 0x002A, text);
    // (3006,0084) Referenced ROI Number: the (3006,0022) written above.
    let mut number_buffer = [0u8; 20];
    write_is(
        dataset,
        0x3006,
        0x0084,
        is_into(&mut number_buffer, usize::from(entry.number)),
    );
    // (3006,0040) Contour Sequence: one item per contour.
    write_sequence_start(dataset, 0x3006, 0x0040);
    for (position, contour) in entry.contours.iter().enumerate() {
        write_item_start(dataset);
        // (3006,0016) Contour Image Sequence: the one element that places
        // the contour on its slice. Written for every contour because on
        // this path the contour is always on a real slice - the
        // Source Pixel Planes Characteristics path (C.8.8.6.4), which is
        // the only way to omit it, describes a synthetic pixel grid this
        // encoder never emits.
        write_sequence_start(dataset, 0x3006, 0x0016);
        write_item_start(dataset);
        write_ui(
            dataset,
            0x0008,
            0x1150,
            if contour.source.sop_class_uid.is_empty() {
                CT_IMAGE_STORAGE_UID
            } else {
                &contour.source.sop_class_uid
            },
        );
        write_ui(dataset, 0x0008, 0x1155, &contour.source.sop_instance_uid);
        write_item_end(dataset);
        write_sequence_end(dataset);
        // (3006,0042) Contour Geometric Type: CLOSED_PLANAR only, so an
        // ROI that mixes geometric types cannot be expressed.
        write_cs(dataset, 0x3006, 0x0042, CONTOUR_GEOMETRIC_TYPE);
        // (3006,0046) Number of Contour Points: one per patient-space
        // point, which is three DS values in (3006,0050).
        let mut count_buffer = [0u8; 20];
        write_is(
            dataset,
            0x3006,
            0x0046,
            is_into(&mut count_buffer, contour.points.len()),
        );
        // (3006,0048) Contour Number. Type 3 in 2026d, but the same table
        // row carries the value constraint "shall be unique within the
        // Contour Sequence", and a viewer without the element shows
        // "Operation failed". Always written, always unique, assigned as
        // position + 1 - this is pin P1.
        //
        // PS3.5 7.1 orders an item's elements too, so it is written after
        // (3006,0042) and (3006,0046) and before (3006,0050).
        let mut number_buffer = [0u8; 20];
        write_is(
            dataset,
            0x3006,
            0x0048,
            is_into(&mut number_buffer, position + 1),
        );
        // (3006,0050) Contour Data: patient millimetres, three values per
        // point. Written through the ONE genuinely new formatter, into a
        // buffer sized by the point count.
        //
        // The buffer is STACK-allocated for a polygon of ordinary size and
        // only falls back to the heap beyond that: a 512-point contour is
        // already 48 KiB of text, which is too much to spend per contour on
        // a worker stack, but a 3-point contour must not allocate at all.
        // The threshold is deliberately generous, because a polygon that
        // big is rare and the heap path is still bounded by the schema's
        // own point cap.
        //
        // The two paths write the element INSIDE their own scope: the text
        // borrows the buffer, so the element has to be emitted before the
        // buffer goes out of scope. Hoisting the text out of the branches is
        // what the borrow checker refuses, and rightly so.
        const STACK_POINTS: usize = 128;
        let mut stack = [0u8; contour_buffer_bytes(STACK_POINTS)];
        let needed = contour_buffer_bytes(contour.points.len());
        if needed <= stack.len() {
            // `unwrap_or` cannot fire: the buffer is sized by
            // `contour_buffer_bytes`, which is exactly the requirement
            // `contour_points_into` checks. The fallback is the single
            // origin, not an empty value, because an empty ContourData is
            // not a legal element and writing one would put an unreadable
            // structure set in the study.
            let text = contour_points_into(&mut stack[..needed], &contour.points)
                .unwrap_or(EMPTY_CONTOUR_TEXT);
            ensure_contour_fits(text.len(), contour.points.len())?;
            write_ds(dataset, 0x3006, 0x0050, text);
        } else {
            let mut heap = vec![0u8; needed];
            let text =
                contour_points_into(&mut heap, &contour.points).unwrap_or(EMPTY_CONTOUR_TEXT);
            ensure_contour_fits(text.len(), contour.points.len())?;
            write_ds(dataset, 0x3006, 0x0050, text);
        }
        write_item_end(dataset);
    }
    write_sequence_end(dataset);
    write_item_end(dataset);
    Ok(())
}

/// Refuse a `ContourData` value that its own VR cannot carry.
///
/// `ContourData` is a DS, a SHORT VR, so its length field is 16 bits and the
/// value is capped at 65534 bytes. A longer value has no correct encoding:
/// the length wraps, and a receiver reads the first value bytes as the next
/// element's VR. The message names the point count and the byte count so an
/// operator can tell a genuinely huge polygon from a formatting problem, and
/// it says what to do, because the remedy is on the producing side.
fn ensure_contour_fits(bytes: usize, points: usize) -> Result<(), String> {
    if bytes <= crate::pr::SHORT_VR_MAX {
        return Ok(());
    }
    Err(format!(
        "contour with {points} point(s) needs {bytes} bytes of ContourData, \
         more than the {max} a DS element can carry; the producing model must \
         reduce the polygon (fewer points, or coordinates quantised to fewer \
         decimals)",
        max = crate::pr::SHORT_VR_MAX
    ))
}

/// (3006,0080) RT ROI Observations Sequence: one item per ROI.
fn write_rt_roi_observation_item(dataset: &mut Vec<u8>, entry: &RtRoiEntry) {
    write_item_start(dataset);
    // (3006,0082) Observation Number: 1..N, unique, assigned as position
    // + 1 - the same rule the ROI number follows.
    let mut number_buffer = [0u8; 20];
    write_is(
        dataset,
        0x3006,
        0x0082,
        is_into(&mut number_buffer, usize::from(entry.number)),
    );
    let mut reference_buffer = [0u8; 20];
    write_is(
        dataset,
        0x3006,
        0x0084,
        is_into(&mut reference_buffer, usize::from(entry.number)),
    );
    // (3006,00A4) RT ROI Interpreted Type: Type 2 with a closed
    // enumeration, so the element is always present and the schema refuses
    // a value outside the list.
    write_cs(dataset, 0x3006, 0x00A4, &entry.roi.interpreted_type);
    // (3006,00A6) ROI Interpreter is Type 2: present, and empty because
    // this path has no human interpreter to name.
    write_pn(dataset, 0x3006, 0x00A6, "");
    // (3006,00B0) ROI Physical Properties Sequence is Type 3 and nothing on
    // this path observes physical properties, so it is omitted rather than
    // filled with a placeholder.
    write_item_end(dataset);
}

/// (3006,002A) as "r\\g\\b" into a caller-owned buffer.
///
/// Three IS values, so the three channels are joined with backslashes. A
/// `String` would be one allocation per ROI; the buffer keeps the ROI loop
/// allocation-free.
fn color_into(buffer: &mut [u8; 32], color: [u8; 3]) -> &str {
    let mut offset = 0;
    for (index, channel) in color.iter().enumerate() {
        if index > 0 {
            buffer[offset] = b'\\';
            offset += 1;
        }
        // A byte channel is at most three digits, so the text never
        // exceeds the remaining room in the 32-byte buffer.
        let mut digits = [0u8; 3];
        let text = u8_into(&mut digits, *channel);
        buffer[offset..offset + text.len()].copy_from_slice(text.as_bytes());
        offset += text.len();
    }
    std::str::from_utf8(&buffer[..offset]).unwrap_or("0\\0\\0")
}

/// An unsigned byte as decimal text into a caller-owned buffer.
fn u8_into(buffer: &mut [u8; 3], value: u8) -> &str {
    let mut cursor = buffer.len();
    let mut remaining = value;
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + (remaining % 10);
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    std::str::from_utf8(&buffer[cursor..]).unwrap_or("0")
}

/// An unsigned integer as decimal text into a caller-owned buffer.
///
/// The same formatter as [`u8_into`], for the IS values that name a number:
/// the ROI number, the observation number, the referenced ROI number and the
/// contour number. Every one of those sits inside a per-ROI or per-contour
/// loop, so `to_string` there costs one heap round-trip per emitted number.
/// The buffer is written from the BACK, so no reversal is needed and the
/// slices returned are the exact digits.
///
/// A `usize` is at most 20 digits on a 64-bit target, which is the buffer
/// size every caller uses.
fn is_into(buffer: &mut [u8; 20], value: usize) -> &str {
    let mut cursor = buffer.len();
    let mut remaining = value;
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + ((remaining % 10) as u8);
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    std::str::from_utf8(&buffer[cursor..]).unwrap_or("0")
}

/// CT Image Storage, the class every contour's source slice carries.
///
/// Used only as a fallback when a source declares no class. The worker
/// always fills it from the frame's own instance, so this exists so the
/// writer cannot emit an EMPTY (0008,1150) - a reference with no class is
/// not resolvable, and a missing class is a frame-metadata gap rather than
/// a reason to write an unreadable object.
pub const CT_IMAGE_STORAGE_UID: &str = "1.2.840.10008.5.1.4.1.1.2";

#[cfg(test)]
mod tests {
    use super::*;

    /// `serialize`, with the fixtures here assumed to be encodable.
    ///
    /// Every fixture in this module is a handful of points, so the refusal
    /// this file pins separately (`a_contour_too_large_for_its_vr_is_refused`)
    /// can never fire here. Rebinding the name keeps the ~40 assertions
    /// below reading exactly as they did, which is what makes them still
    /// worth trusting: they test byte layout, not the error path.
    fn serialize(identity: &RtIdentity) -> Vec<u8> {
        super::serialize(identity).expect("the fixture structure set encodes")
    }

    // -- A fixture that exercises every branch -------------------------
    //
    // Two ROIs, two slices, one non-identity orientation and an
    // anisotropic pixel spacing. The values are chosen so a swap of any
    // axis produces a DIFFERENT number, which is what makes the geometry
    // pin real rather than decorative.

    /// Slice 1: axial, 2.0 mm x 0.5 mm, origin (10.0, -20.0, 30.0).
    fn source_one() -> RtSource {
        RtSource {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
            sop_instance_uid: "1.2.826.0.1.3680043.10.1234.9.1".to_string(),
            series_uid: "1.2.826.0.1.3680043.10.1234.8".to_string(),
            position: Some([10.0, -20.0, 30.0]),
            // NON-IDENTITY and not a rotation of the identity: the row
            // cosines are not axis-aligned, so an implementation that
            // ignored the orientation cannot pass by accident.
            orientation: Some([0.0, 1.0, 0.0, -1.0, 0.0, 0.0]),
            // ANISOTROPIC: row resolution 2.0, column resolution 0.5.
            pixel_spacing: Some([2.0, 0.5]),
        }
    }

    /// Slice 2: one more series, so the reference chain has to group.
    fn source_two() -> RtSource {
        RtSource {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
            sop_instance_uid: "1.2.826.0.1.3680043.10.1234.9.2".to_string(),
            series_uid: "1.2.826.0.1.3680043.10.1234.7".to_string(),
            position: Some([10.0, -20.0, 32.5]),
            orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
            pixel_spacing: Some([1.0, 1.0]),
        }
    }

    fn roi(name: &str, description: Option<&str>, color: [u8; 3]) -> RtRoi {
        RtRoi {
            name: name.to_string(),
            description: description.map(str::to_string),
            color,
            generation_algorithm: "AUTOMATIC".to_string(),
            interpreted_type: "ORGAN".to_string(),
            // Placeholder contours: the entry builders below replace them
            // with the real ones, and an empty list here is what makes a
            // test notice if the replacement is ever skipped.
            contours: Vec::new(),
        }
    }

    fn identity() -> RtIdentity {
        RtIdentity {
            sop_instance_uid: "1.2.826.0.1.3680043.10.1234.20".to_string(),
            study_uid: "1.2.826.0.1.3680043.10.1234.1".to_string(),
            series_uid: "1.2.826.0.1.3680043.10.1234.2".to_string(),
            frame_of_reference_uid: Some("1.2.826.0.1.3680043.10.1234.3".to_string()),
            label: "AI LUNG".to_string(),
            name: Some("Artificial Intelligence Structure Set".to_string()),
            rois: Vec::new(),
            derivation_description: "Generated by clinfer lung profile".to_string(),
            patient_name: Some("IVANOV^IVAN".to_string()),
            patient_id: Some("P-1".to_string()),
            patient_id_issuer: None,
            patient_birth_date: Some("19700101".to_string()),
            patient_sex: Some("M".to_string()),
            study_id: Some("S-1".to_string()),
            accession_number: Some("A-1".to_string()),
            referring_physician: None,
            station_name: Some("STATION".to_string()),
            series_description: None,
            source_series: Vec::new(),
            contour_images: Vec::new(),
        }
    }

    /// One entry with `count` contours on the FIRST source slice.
    ///
    /// The points go through `to_patient` because `RtContour.points` is
    /// defined as PATIENT millimetres: the worker converts before calling the
    /// writer, and a fixture that passed raw pixel indices would be testing a
    /// shape the writer never receives.
    fn entry_on(source: &RtSource, number: u16, count: usize) -> RtRoiEntry {
        let contours = (0..count)
            .map(|index| RtContour {
                points: vec![
                    to_patient(source, index as f64, 0.0),
                    to_patient(source, 1.0 + index as f64, 0.0),
                    to_patient(source, 1.0 + index as f64, 1.0),
                ],
                source: source.clone(),
            })
            .collect();
        RtRoiEntry {
            number,
            roi: roi("LUNG", None, [255, 0, 0]),
            contours,
        }
    }

    // -- Reading the output -------------------------------------------

    /// Every occurrence of an 8-digit hex tag in the bytes.
    /// The VRs whose explicit header carries a reserved word and a 32-bit
    /// length (PS3.5 Table 7.1-1).
    const LONG_VR: &[&[u8; 2]] = &[
        b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT", b"UV",
    ];
    fn tag_at(bytes: &[u8], group: u16, element: u16) -> bool {
        let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
        bytes.windows(4).any(|window| window == needle.as_slice())
    }

    /// One value read back out of the dataset.
    #[derive(Debug, PartialEq, Eq)]
    enum Node {
        Item {
            length: u32,
        },
        ItemEnd,
        Element {
            tag: (u16, u16),
            vr: [u8; 2],
            value: Vec<u8>,
        },
        SequenceEnd,
    }

    /// Walk an explicit-VR-LE dataset into a flat list of nodes.
    ///
    /// Independent of the crate's own writer: it descends into every item
    /// and sequence, so it can disagree with a writer that nested wrongly.
    fn walk(data: &[u8], from: usize) -> Vec<Node> {
        let mut nodes = Vec::new();
        let mut at = from;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
            if group == 0xFFFE {
                let length =
                    u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]);
                at += 8;
                match element {
                    0xE000 => nodes.push(Node::Item { length }),
                    0xE00D => nodes.push(Node::ItemEnd),
                    0xE0DD => nodes.push(Node::SequenceEnd),
                    _ => {}
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
            let start = at + head;
            if length == 0xFFFF_FFFF {
                nodes.push(Node::Element {
                    tag: (group, element),
                    vr,
                    value: Vec::new(),
                });
                at = start;
                continue;
            }
            let end = start + length;
            if end > data.len() {
                break;
            }
            nodes.push(Node::Element {
                tag: (group, element),
                vr,
                value: data[start..end].to_vec(),
            });
            at = end;
        }
        nodes
    }

    /// The dataset, after the 128-byte preamble and the meta group.
    fn dataset(bytes: &[u8]) -> &[u8] {
        let length = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]) as usize;
        &bytes[132 + 12 + length..]
    }

    /// All values of one tag, in document order.
    ///
    /// The writer pads an odd-length UI with a NUL and a string with a
    /// space, so the reader trims both: without this a comparison against a
    /// literal UID fails on a trailing NUL that is not part of the value.
    /// The (group, element) of every TOP-LEVEL element of the dataset.
    ///
    /// "Top-level" is what PS3.5 7.1 orders: the elements of the data set
    /// itself, not the ones inside items it contains. The walk stops at a
    /// sequence's value and skips it whole, so an item's tags never enter
    /// the list.
    /// The offset just past an undefined-length value that starts at `at`.
    ///
    /// Delimiters nest: every `FFFE,E000` opens a level and every
    /// `FFFE,E00D` closes one, with `FFFE,E0DD` closing the sequence. The
    /// walk therefore counts depth rather than stopping at the first
    /// terminator, which is what a sequence holding a sequence requires.
    fn skip_undefined_value(data: &[u8], mut at: usize) -> usize {
        let mut depth = 1usize;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
            if group != 0xFFFE {
                // A real element: step over it by its own length so a value
                // that happens to contain FFFE,E0DD is not read as one.
                let vr = [data[at + 4], data[at + 5]];
                let (length, head) = if LONG_VR.contains(&&vr) {
                    (
                        u32::from_le_bytes([
                            data[at + 8],
                            data[at + 9],
                            data[at + 10],
                            data[at + 11],
                        ]) as usize,
                        12,
                    )
                } else {
                    (u16::from_le_bytes([data[at + 6], data[at + 7]]) as usize, 8)
                };
                if length == 0xFFFF_FFFF {
                    at = skip_undefined_value(data, at + head);
                } else {
                    at += head + length;
                }
                continue;
            }
            let length =
                u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]])
                    as usize;
            at += 8;
            match element {
                0xE000 => {
                    // A defined-length item's body is skipped whole; an
                    // undefined one is walked until its own E00D.
                    if length != 0xFFFF_FFFF {
                        at += length;
                    } else {
                        depth += 1;
                    }
                }
                0xE00D => depth -= 1,
                0xE0DD => {
                    depth -= 1;
                    if depth == 0 {
                        return at;
                    }
                }
                _ => {}
            }
        }
        data.len()
    }
    fn top_level_tags(bytes: &[u8]) -> Vec<(u16, u16)> {
        let data = dataset(bytes);
        let mut tags = Vec::new();
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
            if group == 0xFFFE {
                // A delimiter has no place here: the top level holds no
                // unterminated item, so reaching one means the walk is off.
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
                // Undefined length: the value is a stream of items, items
                // that may nest, and delimiters. Walked rather than
                // measured, because a sequence's extent is only known from
                // its own terminator.
                at = skip_undefined_value(data, at + head);
            } else {
                at += head + length;
            }
        }
        tags
    }
    fn values(nodes: &[Node], group: u16, element: u16) -> Vec<String> {
        nodes
            .iter()
            .filter_map(|node| match node {
                Node::Element { tag, value, .. } if *tag == (group, element) => Some(
                    String::from_utf8_lossy(value)
                        .trim_end_matches(['\0', ' '])
                        .to_string(),
                ),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_top_level_elements_are_in_ascending_tag_order() {
        // PS3.5 7.1: a data set's elements are in ascending tag order. The
        // writer used to emit the General Study rows, then the Patient
        // rows, then (0020,000D) - out of order three times over - and a
        // strict reader is entitled to stop at the first descent.
        let bytes = serialize(&identity());
        let tags = top_level_tags(&bytes);
        assert!(tags.len() > 15, "the object has a body: {tags:?}");
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(tags, sorted, "top-level tags ascend");
        // The list is the whole dataset rather than a prefix: the last
        // element the writer emits is the ROI observations sequence.
        assert_eq!(
            tags.last().copied(),
            Some((0x3006, 0x0080)),
            "the walk reached the end of the dataset: {tags:?}"
        );
    }

    /// (0008,1070) is Operators' Name and does NOT belong to the General
    /// Equipment Module: PS3.3 Table C.7-8 runs (0008,1041) straight to
    /// (0008,1090). Writing it stated that a human operated on this object,
    /// and named them after the vendor.
    #[test]
    fn the_structure_set_does_not_claim_an_operator() {
        let bytes = serialize(&identity());
        let nodes = walk(dataset(&bytes), 0);
        assert!(
            values(&nodes, 0x0008, 0x1070).is_empty(),
            "Operators' Name is not an attribute of this IOD: {:?}",
            values(&nodes, 0x0008, 0x1070)
        );
        // The device is named where the module actually puts it.
        assert_eq!(
            one(&nodes, 0x0008, 0x1090),
            implementation_version_name(),
            "Manufacturer's Model Name carries the writer's identity"
        );
    }

    /// The single value of one tag, ignoring the trailing pad.
    fn one(nodes: &[Node], group: u16, element: u16) -> String {
        let all = values(nodes, group, element);
        assert_eq!(
            all.len(),
            1,
            "expected exactly one ({group:04X},{element:04X}), found {all:?}"
        );
        all[0].clone()
    }

    /// A structure set with two ROIs, three contours, two source series.
    ///
    /// Every point is built through `to_patient`, so the document holds real
    /// patient millimetres whose z ties each contour to its slice.
    fn populated() -> RtIdentity {
        let first = source_one();
        let second = source_two();
        let mut identity = identity();
        identity.rois = vec![
            RtRoiEntry {
                number: 1,
                roi: roi("LUNG_L", Some("left lung"), [255, 0, 0]),
                contours: vec![
                    RtContour {
                        points: vec![
                            to_patient(&first, 0.0, 0.0),
                            to_patient(&first, 2.0, 0.0),
                            to_patient(&first, 2.0, 2.0),
                        ],
                        source: first.clone(),
                    },
                    RtContour {
                        points: vec![
                            to_patient(&second, 0.0, 0.0),
                            to_patient(&second, 4.0, 0.0),
                            to_patient(&second, 4.0, 4.0),
                        ],
                        source: second.clone(),
                    },
                ],
            },
            RtRoiEntry {
                number: 2,
                roi: roi("LUNG_R", None, [0, 0, 255]),
                contours: vec![RtContour {
                    points: vec![
                        to_patient(&first, 6.0, 6.0),
                        to_patient(&first, 8.0, 6.0),
                        to_patient(&first, 8.0, 8.0),
                    ],
                    source: first.clone(),
                }],
            },
        ];
        identity.source_series = vec![first.series_uid.clone(), second.series_uid.clone()];
        identity.contour_images = vec![first, second];
        identity
    }

    // -- P1: ContourNumber ---------------------------------------------

    #[test]
    fn p1_every_contour_carries_a_number_unique_within_its_sequence() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let numbers = values(&nodes, 0x3006, 0x0048);
        // Three contours across two ROIs, numbered 1..n inside each ROIContour
        // item - so two sequences of 1,2 and one of 1.
        assert_eq!(
            numbers,
            vec!["1", "2", "1"],
            "contour numbers restart per ROI and never repeat inside one"
        );
        // The pin is "present AND unique within its sequence", so the count
        // must equal the count of ContourData elements: one number per
        // contour, never a contour without one.
        assert_eq!(
            numbers.len(),
            values(&nodes, 0x3006, 0x0050).len(),
            "every contour with data carries a number"
        );
    }

    #[test]
    fn p1_a_contour_number_is_absorbed_when_it_would_be_empty() {
        // A single contour still carries the element: the pin is about
        // presence, and a writer that only numbered "the second one onwards"
        // would satisfy uniqueness while failing the viewer.
        let mut identity = identity();
        identity.rois = vec![entry_on(&source_one(), 1, 1)];
        identity.source_series = vec![source_one().series_uid];
        identity.contour_images = vec![source_one()];
        let nodes = walk(dataset(&serialize(&identity)), 0);
        assert_eq!(values(&nodes, 0x3006, 0x0048), vec!["1"]);
    }

    #[test]
    fn p1_contour_number_is_type_is_not_ds() {
        // (3006,0048) is IS. A DS would let a reader see "1.000000" and a
        // strict parser reject the value; the VR in the header is the fact.
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let vr = nodes
            .iter()
            .find_map(|node| match node {
                Node::Element { tag, vr, .. } if *tag == (0x3006, 0x0048) => Some(*vr),
                _ => None,
            })
            .expect("ContourNumber is present");
        assert_eq!(&vr, b"IS", "ContourNumber VR");
    }

    // -- P2: ROIDisplayColor -------------------------------------------

    #[test]
    fn p2_display_color_is_three_integer_channels_in_range() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let colors = values(&nodes, 0x3006, 0x002A);
        assert_eq!(colors, vec!["255\\0\\0", "0\\0\\255"]);
        for text in &colors {
            let channels: Vec<&str> = text.split('\\').collect();
            assert_eq!(channels.len(), 3, "three channels in {text:?}");
            for channel in channels {
                assert!(
                    !channel.contains('.'),
                    "a channel is an integer, not a decimal: {text:?}"
                );
                let value: u32 = channel.parse().expect("a channel parses as an integer");
                assert!(value <= 255, "a channel is 0..=255: {text:?}");
            }
        }
    }

    #[test]
    fn p2_display_color_vr_is_is() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let vr = nodes
            .iter()
            .find_map(|node| match node {
                Node::Element { tag, vr, .. } if *tag == (0x3006, 0x002A) => Some(*vr),
                _ => None,
            })
            .expect("ROIDisplayColor is present");
        assert_eq!(&vr, b"IS", "ROIDisplayColor VR");
    }

    #[test]
    fn p2_color_channel_formatting_is_byte_exact() {
        // The three growth paths of the integer formatter, and the two ends
        // of the range, so a buffer that overflowed or a formatter that
        // emitted a leading zero would be caught here rather than on the
        // wire.
        let mut buffer = [0u8; 32];
        assert_eq!(color_into(&mut buffer, [0, 0, 0]), "0\\0\\0");
        assert_eq!(color_into(&mut buffer, [255, 255, 255]), "255\\255\\255");
        assert_eq!(color_into(&mut buffer, [7, 70, 255]), "7\\70\\255");
        assert_eq!(color_into(&mut buffer, [1, 10, 100]), "1\\10\\100");
    }

    // -- P3: ContourData and the pixel-to-patient equation --------------

    #[test]
    fn p3_to_patient_follows_equation_c_7_6_2_1_1() {
        // Equation C.7.6.2.1-1:
        //   P = S + i * Dr * X + j * Dc * Y
        // where i is the COLUMN index, Dr the COLUMN resolution (which the
        // standard stores SECOND in PixelSpacing), X the ROW direction
        // cosines, and j the ROW index with Dc the ROW resolution.
        //
        // The names are the trap: "column resolution" is paired with the
        // COLUMN index and the ROW cosines. With Spacing = [2.0, 0.5],
        // column = 4, row = 3, position (10,-20,30), row cosines
        // (0,1,0) and column cosines (-1,0,0):
        //
        //   dc = 0.5 (the COLUMN resolution, Spacing[1])
        //   dr = 2.0 (the ROW resolution, Spacing[0])
        //   x = 10 + 4*0.5*0      + 3*2.0*(-1) = 10 - 6  = 4
        //   y = -20 + 4*0.5*1     + 3*2.0*0    = -20 + 2 = -18
        //   z = 30
        let point = to_patient(&source_one(), 4.0, 3.0);
        assert!(
            (point[0] - 4.0).abs() < 1e-9,
            "x follows the column index, the column resolution and the ROW cosines: {point:?}"
        );
        assert!(
            (point[1] - -18.0).abs() < 1e-9,
            "y follows the row index, the row resolution and the COLUMN cosines: {point:?}"
        );
        assert!(
            (point[2] - 30.0).abs() < 1e-9,
            "z is the slice origin: {point:?}"
        );

        // And the two pairings are genuinely swapped: the SAME indices on a
        // slice whose spacing is transposed must land somewhere else, which
        // is what an implementation that ignored the anisotropy cannot do.
        let mut transposed = source_one();
        transposed.pixel_spacing = Some([0.5, 2.0]);
        let other = to_patient(&transposed, 4.0, 3.0);
        assert_ne!(point, other, "the spacing pair is load-bearing");
    }

    #[test]
    fn p3_an_isotropic_identity_slice_still_moves_by_the_spacing() {
        // The plain case, so the equation is anchored at the trivial end
        // too: with a 1 mm isotropic identity slice the patient millimetres
        // equal the pixel index plus the origin.
        let point = to_patient(&source_two(), 4.0, 3.0);
        assert!((point[0] - 14.0).abs() < 1e-9, "{point:?}");
        assert!((point[1] - -17.0).abs() < 1e-9, "{point:?}");
        assert!((point[2] - 32.5).abs() < 1e-9, "{point:?}");
    }

    #[test]
    fn p3_the_point_count_matches_the_number_of_triples() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let counts = values(&nodes, 0x3006, 0x0046);
        let data = values(&nodes, 0x3006, 0x0050);
        assert_eq!(counts.len(), data.len(), "one count per contour");
        for (count, text) in counts.iter().zip(&data) {
            let declared: usize = count.parse().expect("a point count is an integer");
            let values: Vec<&str> = text.split('\\').collect();
            assert_eq!(
                values.len(),
                declared * 3,
                "three DS values per point: {declared} points, {text:?}"
            );
            assert_eq!(values.len() % 3, 0, "the data is a whole number of points");
        }
    }

    #[test]
    fn p3_every_point_z_is_the_referenced_slices_z() {
        // THE pin that makes ContourImageSequence load-bearing: the z of a
        // contour's points must BE the referenced slice's (0020,0032) z, or
        // a viewer draws the contour on the wrong slice.
        let identity = populated();
        let nodes = walk(dataset(&serialize(&identity)), 0);
        let data = values(&nodes, 0x3006, 0x0050);
        // Contour order is ROI 1 contour 1, ROI 1 contour 2, ROI 2 contour 1.
        let expected = [30.0, 32.5, 30.0];
        assert_eq!(data.len(), expected.len(), "three contours in the fixture");
        for (text, z) in data.iter().zip(&expected) {
            let values: Vec<&str> = text.split('\\').collect();
            assert_eq!(values.len() % 3, 0, "a whole number of points: {text:?}");
            for triple in values.chunks(3) {
                let z_value: f64 = triple[2].parse().expect("z parses");
                assert!(
                    (z_value - z).abs() < 1e-6,
                    "z {z_value} is the referenced slice's z {z}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn p3_the_geometry_conversion_actually_ran() {
        // The fixture builds every point through `to_patient`, so this pins
        // that the DOCUMENT holds the converted values rather than the pixel
        // indices the conversion started from.
        //
        // The discriminator cannot be "the first point is a slice origin":
        // only a contour traced from pixel index (0,0) starts at the origin,
        // and the fixture's second ROI starts at (6,6). What every contour
        // does have is a first point whose z is its slice's z AND whose x/y
        // are offset from the small pixel indices, which raw indices could
        // never produce.
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let data = values(&nodes, 0x3006, 0x0050);
        let first = source_one();
        let second = source_two();
        let slice_z = [first.position.unwrap()[2], second.position.unwrap()[2]];

        assert_eq!(data.len(), 3, "three contours in the fixture");
        for text in &data {
            let components: Vec<&str> = text.split('\\').collect();
            assert_eq!(components.len() % 3, 0, "whole points: {text:?}");
            let first_point = &components[..3];
            let z: f64 = first_point[2].parse().expect("z parses");
            assert!(
                slice_z.iter().any(|value| (value - z).abs() < 1e-6),
                "the first point's z is one of the slice z values: {text:?}"
            );
            // The pixel-space discriminant: the fixture's pixels are single
            // digits, so a converted point can never be three integers below
            // ten with a z equal to a slice origin.
            let x: f64 = first_point[0].parse().expect("x parses");
            let y: f64 = first_point[1].parse().expect("y parses");
            assert!(
                x.abs() >= 2.0 || y.abs() >= 2.0 || z >= 30.0,
                "the point is in patient space, not pixel space: {text:?}"
            );
        }
    }

    #[test]
    fn p3_an_absent_geometry_pair_falls_back_to_the_identity() {
        // A source that declares no orientation and no spacing cannot be
        // placed; the caller refuses such a frame, and the writer's own
        // fallback is the identity so a partially-filled fixture still
        // produces a document rather than a panic. This pins the fallback
        // so it cannot silently become "skip the contour".
        let mut source = source_two();
        source.orientation = None;
        source.pixel_spacing = None;
        let point = to_patient(&source, 4.0, 3.0);
        assert!((point[0] - 14.0).abs() < 1e-9, "{point:?}");
        assert!((point[1] - -17.0).abs() < 1e-9, "{point:?}");
        assert!((point[2] - 32.5).abs() < 1e-9, "{point:?}");
    }

    // -- P4: the reference chain ---------------------------------------

    #[test]
    fn p4_the_chain_is_3006_0010_0012_0014_0016_and_in_that_nesting() {
        // The four sequences must NEST, not sit beside each other. The walk
        // reads item and sequence delimiters, so a writer that flattened the
        // chain is caught here and not by a tag search.
        //
        // (3006,0016) is written a SECOND time per contour, because a
        // contour names the slice it lies on through the same sequence. So
        // the assertion is about the FIRST occurrence of each header: the
        // chain heads in ascending order, each opening its item before the
        // next head appears.
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let mut heads = Vec::new();
        for (index, node) in nodes.iter().enumerate() {
            if let Node::Element { tag, .. } = node
                && [
                    (0x3006u16, 0x0010u16),
                    (0x3006, 0x0012),
                    (0x3006, 0x0014),
                    (0x3006, 0x0016),
                ]
                .contains(tag)
                && !heads.iter().any(|(_, seen)| seen == tag)
            {
                heads.push((index, *tag));
            }
        }
        let tags: Vec<(u16, u16)> = heads.iter().map(|(_, tag)| *tag).collect();
        assert_eq!(
            tags,
            vec![
                (0x3006, 0x0010),
                (0x3006, 0x0012),
                (0x3006, 0x0014),
                (0x3006, 0x0016)
            ],
            "the first occurrence of each header, in ascending order"
        );
        for pair in heads.windows(2) {
            let between = &nodes[pair[0].0 + 1..pair[1].0];
            assert!(
                matches!(between.first(), Some(Node::Item { .. })),
                "the sequence at {:?} opens an item: {between:?}",
                pair[0].1
            );
        }
    }

    #[test]
    fn p4_the_contour_level_image_sequence_names_one_slice_per_contour() {
        // (3006,0016) appears in TWO roles, and the counts differ:
        //
        //  * inside the reference chain, ONE per distinct source slice
        //    (C.8.8.5.1 wants the referenced slices listed once, not once per
        //    contour that touches them);
        //  * inside each contour item, ONE per contour, naming the slice that
        //    contour lies on.
        //
        // A writer that reused one role's count for the other produces a
        // document that parses but misplaces contours, so both counts are
        // asserted against the fixture's own numbers.
        let identity = populated();
        let nodes = walk(dataset(&serialize(&identity)), 0);
        let headers: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| match node {
                Node::Element { tag, .. } if *tag == (0x3006, 0x0016) => Some(index),
                _ => None,
            })
            .collect();

        let contours = identity
            .rois
            .iter()
            .map(|entry| entry.contours.len())
            .sum::<usize>();
        let slices = identity.contour_images.len();
        assert_eq!(
            headers.len(),
            slices + contours,
            "{slices} chain occurrences plus one per contour ({contours})"
        );

        // The ROI sequences begin at the first (3006,0020), so every
        // (3006,0016) after it belongs to a contour, and every one before it
        // belongs to the chain.
        let roi_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x3006, 0x0020)))
            .expect("the ROI sequence is present");
        let contour_level = headers.iter().filter(|index| **index > roi_at).count();
        let chain_level = headers.iter().filter(|index| **index < roi_at).count();
        assert_eq!(
            contour_level, contours,
            "one ContourImageSequence per contour"
        );
        assert_eq!(chain_level, slices, "one per distinct source slice, once");
    }

    #[test]
    fn p4_the_outer_study_item_names_the_study_class_not_ct_image_storage() {
        // C.8.8.5.4: the (3006,0012) item's (0008,1150) is the SOP Class of
        // the STUDY "not of the Instances within the Study ... even if all
        // of the Instances within the Study are of the same Instance SOP
        // Class". Writing CT Image Storage here is the trap this pins.
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let study_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x3006, 0x0012)))
            .expect("the study sequence is present");
        let class_at = nodes[study_at..]
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0008, 0x1150)))
            .map(|offset| study_at + offset)
            .expect("the study item names a class");
        let value = match &nodes[class_at] {
            Node::Element { value, .. } => String::from_utf8_lossy(value)
                .trim_end_matches(['\0', ' '])
                .to_string(),
            _ => unreachable!("selected an element"),
        };
        assert_eq!(
            value, STUDY_SOP_CLASS_UID,
            "the outer reference names the Study SOP Class"
        );
        assert_eq!(
            value, "1.2.840.10008.3.1.2.3.1",
            "the Study SOP Class is Detached Study Management, pinned by \
             literal rather than by the constant: a constant that drifted \
             with its test would pass this test and still be wrong on the wire"
        );
        assert_ne!(
            value, "1.2.840.10008.3.1.1.1",
            "Modality Worklist FIND is a query model, not the Study class"
        );
        assert_ne!(
            value, CT_IMAGE_STORAGE_UID,
            "CT Image Storage in the OUTER position is the C.8.8.5.4 error"
        );
        // And the study UID beside it is the study's own.
        assert_eq!(
            nodes[class_at..]
                .iter()
                .find_map(|node| match node {
                    Node::Element { tag, value, .. } if *tag == (0x0008, 0x1155) => {
                        Some(
                            String::from_utf8_lossy(value)
                                .trim_end_matches(['\0', ' '])
                                .to_string(),
                        )
                    }
                    _ => None,
                })
                .as_deref(),
            Some("1.2.826.0.1.3680043.10.1234.1"),
        );
    }

    #[test]
    fn p4_the_inner_contour_image_items_name_ct_image_storage() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let image_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x3006, 0x0016)))
            .expect("the contour image sequence is present");
        // Everything from the first (3006,0016) onwards is either a contour
        // image item or the chain closing: every class named there is the
        // instance class.
        let classes = values(&nodes[image_at..], 0x0008, 0x1150);
        assert!(
            !classes.is_empty(),
            "the contour image sequence holds at least one item"
        );
        for class in &classes {
            assert_eq!(
                class, CT_IMAGE_STORAGE_UID,
                "every item past the study item names the instance class"
            );
        }
        // The two are DIFFERENT values, which is the whole point of the
        // pin: a writer that used one constant for both positions fails one
        // of these two tests.
        assert_ne!(STUDY_SOP_CLASS_UID, CT_IMAGE_STORAGE_UID);
    }

    #[test]
    fn the_series_number_separates_this_series_from_the_source_and_the_seg() {
        // A viewer lists a study's series BY NUMBER, so two series in one
        // study that share a number are two rows a reader cannot tell
        // apart. The structure set used to write 1, the same number the
        // source CT carries, which is what made it look like a copy of the
        // study rather than a result derived from it.
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let number = one(&nodes, 0x0020, 0x0011);
        assert_eq!(
            number, SERIES_NUMBER,
            "(0020,0011) is this writer's own series number"
        );
        assert_eq!(
            number, "3",
            "the RTSTRUCT series number is 3, pinned by literal: CT is 1, \
             the SEG is 2, and this is the third series in the study"
        );
        // The three numbers in one study are pairwise distinct, which is
        // the property the scheme exists to provide.
        assert_ne!("3", crate::seg::SERIES_NUMBER, "RTSTRUCT is not the SEG");
        assert_ne!("3", "1", "RTSTRUCT is not the source acquisition");
        assert_ne!(
            crate::seg::SERIES_NUMBER,
            "1",
            "the SEG is not the source acquisition either"
        );
    }

    #[test]
    fn p4_the_slices_are_grouped_by_series() {
        // (3006,0014) groups its slices BY SERIES. The fixture's three
        // contours sit on two slices in two different series, so a writer
        // that listed every slice under the first series would produce one
        // series item holding two mismatched slices.
        let node = walk(dataset(&serialize(&populated())), 0);
        let series = values(&node, 0x0020, 0x000E);
        // Two series UIDs inside the frame-of-reference chain, and two more
        // in the Common Instance Reference module: the same two, twice.
        assert!(
            series.contains(&"1.2.826.0.1.3680043.10.1234.7".to_string()),
            "the second source series appears as a group head: {series:?}"
        );
        assert!(series.contains(&"1.2.826.0.1.3680043.10.1234.8".to_string()));
    }

    #[test]
    fn p4_referenced_roi_number_points_back_at_a_written_roi() {
        // (3006,0084) appears in ROI Contour and RT ROI Observations. Every
        // value must be an ROI number that (3006,0022) established, or the
        // sequences reference an ROI that does not exist.
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let numbers: Vec<String> = values(&nodes, 0x3006, 0x0022);
        assert_eq!(numbers, vec!["1", "2"], "one ROI item per ROI");
        let references = values(&nodes, 0x3006, 0x0084);
        assert!(!references.is_empty(), "the references are written");
        for reference in &references {
            assert!(
                numbers.contains(reference),
                "({reference}) is not a written ROI number: {numbers:?}"
            );
        }
    }

    #[test]
    fn p4_the_roi_number_is_position_plus_one_with_no_gaps() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        assert_eq!(values(&nodes, 0x3006, 0x0022), vec!["1", "2"]);
        assert_eq!(
            values(&nodes, 0x3006, 0x0082),
            vec!["1", "2"],
            "observation numbers follow the same rule"
        );
    }

    // -- The file envelope and identity rows ---------------------------

    #[test]
    fn the_file_is_a_part_10_file_with_the_right_meta_group() {
        let bytes = serialize(&populated());
        assert_eq!(&bytes[128..132], b"DICM", "the DICM magic");
        assert_eq!(&bytes[0..128], &[0u8; 128], "a 128-byte preamble");
        let nodes = walk(&bytes, 132);
        let meta = &nodes[..];
        assert_eq!(one(meta, 0x0002, 0x0002), RT_STRUCT_SOP_CLASS_UID);
        assert_eq!(one(meta, 0x0002, 0x0003), "1.2.826.0.1.3680043.10.1234.20");
        assert_eq!(one(meta, 0x0002, 0x0010), EXPLICIT_VR_LE);
        // (0002,0001) File Meta Information Version is OB with the two
        // bytes 00 01, which the writer writes as the hex text "01".
        assert_eq!(one(meta, 0x0002, 0x0001), "01");
    }

    #[test]
    fn the_written_class_and_modality_match_the_iod() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        assert_eq!(one(&nodes, 0x0008, 0x0016), RT_STRUCT_SOP_CLASS_UID);
        // C.8.8.1.1: "Enumerated Values if RT Structure Set IOD: RTSTRUCT".
        assert_eq!(one(&nodes, 0x0008, 0x0060), "RTSTRUCT");
    }

    #[test]
    fn the_character_set_declaration_precedes_the_bytes_it_governs() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let charset = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0008, 0x0005)))
            .expect("(0008,0005) is written");
        let name = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0010, 0x0010)))
            .expect("PatientName is written");
        assert!(
            charset < name,
            "the character set declaration precedes the patient identity"
        );
        assert_eq!(one(&nodes, 0x0008, 0x0005), SPECIFIC_CHARACTER_SET);
    }

    #[test]
    fn a_cyrillic_patient_name_survives_the_round_trip() {
        // Written as escapes so this source file itself stays ASCII: the
        // bytes in the document are UTF-8 and the character set declaration
        // says so.
        let mut identity = populated();
        identity.patient_name = Some(
            "\u{041f}\u{0415}\u{0422}\u{0420}\u{041e}\u{0412}^\u{0418}\u{0412}\u{0410}\u{041d}"
                .to_string(),
        );
        identity.rois[0].roi.name =
            "\u{041b}\u{0435}\u{0432}\u{043e}\u{0435} \u{043b}\u{0451}\u{0433}\u{043a}\u{043e}\u{0435}".to_string();
        let nodes = walk(dataset(&serialize(&identity)), 0);
        assert_eq!(
            one(&nodes, 0x0010, 0x0010),
            "\u{041f}\u{0415}\u{0422}\u{0420}\u{041e}\u{0412}^\u{0418}\u{0412}\u{0410}\u{041d}"
        );
        assert_eq!(
            values(&nodes, 0x3006, 0x0026)[0],
            "\u{041b}\u{0435}\u{0432}\u{043e}\u{0435} \u{043b}\u{0451}\u{0433}\u{043a}\u{043e}\u{0435}"
        );
    }

    #[test]
    fn the_type_2_identity_rows_are_present_even_when_empty() {
        // Type 2 means present with a possibly empty value. Omitting the
        // element claims the writer never considered it, which is a
        // different fact from "the source had none".
        let mut identity = populated();
        identity.patient_name = None;
        identity.patient_id = None;
        identity.patient_birth_date = None;
        identity.patient_sex = None;
        identity.referring_physician = None;
        identity.study_id = None;
        identity.accession_number = None;
        let nodes = walk(dataset(&serialize(&identity)), 0);
        for tag in [
            (0x0010u16, 0x0010u16),
            (0x0010, 0x0020),
            (0x0010, 0x0030),
            (0x0010, 0x0040),
            (0x0008, 0x0090),
            (0x0020, 0x0010),
            (0x0008, 0x0050),
            (0x0020, 0x1040),
        ] {
            let found = values(&nodes, tag.0, tag.1);
            assert_eq!(
                found.len(),
                1,
                "({:04X},{:04X}) is a Type 2 row and must be present: {found:?}",
                tag.0,
                tag.1
            );
            assert_eq!(found[0], "", "and legitimately empty");
        }
    }

    #[test]
    fn an_absent_issuer_omits_the_element_but_an_explicit_empty_writes_it() {
        // (0010,0021) is Type 1C inside a macro: `None` means no source
        // carried one and the element is absent; `Some("")` means the
        // workitem said so at this site and the zero-length element is the
        // faithful answer.
        let mut identity = populated();
        identity.patient_id_issuer = None;
        let nodes = walk(dataset(&serialize(&identity)), 0);
        assert!(
            values(&nodes, 0x0010, 0x0021).is_empty(),
            "no issuer means no element"
        );

        let mut identity = populated();
        identity.patient_id_issuer = Some(String::new());
        let nodes = walk(dataset(&serialize(&identity)), 0);
        assert_eq!(
            values(&nodes, 0x0010, 0x0021),
            vec![""],
            "an explicit empty issuer is written as the empty element"
        );
    }

    #[test]
    fn the_frame_of_reference_is_written_only_when_the_source_declares_one() {
        // Usage U (Table A.19.3-1 Note 2). An invented value would claim
        // the contours live in a frame nothing else knows about.
        let mut identity = populated();
        identity.frame_of_reference_uid = None;
        let nodes = walk(dataset(&serialize(&identity)), 0);
        assert!(
            values(&nodes, 0x0020, 0x0052).is_empty(),
            "no source frame means no frame element"
        );
        assert!(
            values(&nodes, 0x3006, 0x0024).is_empty(),
            "and no ROI claims one either"
        );

        let nodes = walk(dataset(&serialize(&populated())), 0);
        // (0020,0052) is written TWICE and both are correct: once as the
        // dataset-level Frame of Reference UID, once inside the single
        // Referenced Frame of Reference item, where C.8.8.5.1 requires the
        // same value. Both must be the source's, unchanged.
        for reference in values(&nodes, 0x0020, 0x0052) {
            assert_eq!(
                reference, "1.2.826.0.1.3680043.10.1234.3",
                "the source's value, unchanged, in both positions"
            );
        }
        assert_eq!(
            values(&nodes, 0x0020, 0x0052).len(),
            2,
            "the dataset level and the referenced frame-of-reference item"
        );
        // The same value on every ROI: C.8.8.5.1 requires each frame an
        // ROI's coordinates use to be listed in (3006,0010), and that
        // sequence has exactly one item.
        for reference in values(&nodes, 0x3006, 0x0024) {
            assert_eq!(reference, "1.2.826.0.1.3680043.10.1234.3");
        }
    }

    #[test]
    fn the_roi_identity_rows_carry_the_script_values() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        assert_eq!(values(&nodes, 0x3006, 0x0026), vec!["LUNG_L", "LUNG_R"]);
        assert_eq!(values(&nodes, 0x3006, 0x0028), vec!["left lung"]);
        assert_eq!(
            values(&nodes, 0x3006, 0x0036),
            vec!["AUTOMATIC", "AUTOMATIC"],
            "the generation algorithm"
        );
        assert_eq!(
            values(&nodes, 0x3006, 0x00A4),
            vec!["ORGAN", "ORGAN"],
            "the interpreted type"
        );
        assert_eq!(one(&nodes, 0x3006, 0x0002), "AI LUNG");
        assert_eq!(
            one(&nodes, 0x3006, 0x0004),
            "Artificial Intelligence Structure Set"
        );
        assert_eq!(
            one(&nodes, 0x0008, 0x2111),
            "Generated by clinfer lung profile",
            "Derivation Description is the only provenance channel A.19 provides"
        );
    }

    #[test]
    fn the_geometric_type_is_closed_planar_for_every_contour() {
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let types = values(&nodes, 0x3006, 0x0042);
        assert_eq!(types.len(), 3, "one per contour");
        for kind in &types {
            assert_eq!(kind, CONTOUR_GEOMETRIC_TYPE);
        }
    }

    #[test]
    fn a_structure_set_without_rois_still_serializes_its_identity() {
        // The SCHEMA refuses an empty ROI array, so this shape reaches the
        // writer only from an internal caller. The writer must still emit
        // a well-formed document rather than a truncated one, because a
        // reader that opened it would otherwise see a file with no ROI
        // sequences at all and no way to tell that from a parse failure.
        let mut identity = identity();
        identity.rois = Vec::new();
        identity.source_series = Vec::new();
        identity.contour_images = Vec::new();
        let bytes = serialize(&identity);
        let nodes = walk(dataset(&bytes), 0);
        assert_eq!(one(&nodes, 0x0008, 0x0016), RT_STRUCT_SOP_CLASS_UID);
        assert!(values(&nodes, 0x3006, 0x0022).is_empty(), "no ROI items");
        assert!(tag_at(&bytes, 0x3006, 0x0010), "the frame chain is present");
    }

    #[test]
    fn every_item_and_sequence_this_writer_opens_is_closed() {
        // This writer uses UNDEFINED lengths with explicit delimiters, so the
        // byte stream carries an item delimiter for every item and a
        // sequence delimiter for every sequence.
        //
        // The two families balance INDEPENDENTLY: (FFFE,E000) is paired with
        // (FFFE,E00D), and an SQ header is paired with (FFFE,E0DD). A single
        // combined depth counter reports a false imbalance, because a
        // sequence whose item sits inside it closes two levels with one
        // delimiter while a sequence with no item closes one.
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let mut items_open = 0i64;
        let mut sequences_open = 0i64;
        for node in &nodes {
            match node {
                Node::Item { length } => {
                    assert_eq!(*length, 0xFFFF_FFFF, "undefined-length items");
                    items_open += 1;
                }
                Node::ItemEnd => items_open -= 1,
                Node::SequenceEnd => sequences_open -= 1,
                Node::Element { vr, .. } => {
                    if vr == b"SQ" {
                        sequences_open += 1;
                    }
                }
            }
            assert!(items_open >= 0, "an item delimiter without its opener");
            assert!(
                sequences_open >= 0,
                "a sequence delimiter without its header"
            );
        }
        assert_eq!(items_open, 0, "every item is closed");
        assert_eq!(sequences_open, 0, "every sequence is closed");
        // The document holds at least one of each, so the loop was not vacuous.
        assert!(nodes.iter().any(|node| matches!(node, Node::SequenceEnd)));
        assert!(nodes.iter().any(|node| matches!(node, Node::ItemEnd)));
    }

    #[test]
    fn the_contour_data_buffer_is_sized_for_the_worst_case() {
        // The formatter writes 32 bytes per value and the assembly adds one
        // separator per value: a buffer one byte short would either
        // allocate or truncate, and this asserts the arithmetic the caller
        // relies on.
        assert_eq!(contour_buffer_bytes(0), 0);
        assert_eq!(contour_buffer_bytes(1), 3 * 32 + 3);
        assert_eq!(contour_buffer_bytes(3), 9 * 32 + 9);
    }

    #[test]
    fn a_single_point_contour_reports_a_count_of_one() {
        // The schema's MIN_CONTOUR_POINTS is enforced there, not here, so
        // the writer's job is only to report the true count. A writer that
        // hard-coded three would pass the populated fixture and fail a
        // one-point contour.
        let mut identity = identity();
        identity.rois = vec![RtRoiEntry {
            number: 1,
            roi: roi("POINT", None, [1, 2, 3]),
            contours: vec![RtContour {
                points: vec![[0.0, 0.0, 0.0]],
                source: source_one(),
            }],
        }];
        identity.source_series = vec![source_one().series_uid];
        identity.contour_images = vec![source_one()];
        let nodes = walk(dataset(&serialize(&identity)), 0);
        assert_eq!(values(&nodes, 0x3006, 0x0046), vec!["1"]);
        assert_eq!(values(&nodes, 0x3006, 0x0050).len(), 1);
    }
    #[test]
    fn a_negative_coordinate_keeps_its_sign() {
        // Both source slices put the origin at y = -20, and the column
        // cosines only ever add a non-negative quantity to it, so every
        // point's y is negative. A formatter that dropped the sign would
        // put the whole structure set on the wrong side of the origin.
        let nodes = walk(dataset(&serialize(&populated())), 0);
        let data = values(&nodes, 0x3006, 0x0050);
        for text in &data {
            let components: Vec<&str> = text.split('\\').collect();
            for triple in components.chunks(3) {
                assert!(
                    triple[1].starts_with('-'),
                    "y is negative in this fixture: {text:?}"
                );
            }
        }
    }

    // -- The ContourData byte ceiling ---------------------------------
    //
    // A live CT structure set was refused by the origin with "Invalid VR in
    // undefined-length element": one contour of 3874 points encoded to 69342
    // bytes of ContourData, which does not fit a DS element's 16-bit length
    // field, and the old writer cast the length to u16 - 69342 became 3806,
    // so a parser advanced 62730 bytes too few and read the digits of a
    // coordinate as the next element's VR. These tests pin the behaviour
    // that replaced the cast: a refusal, with the numbers in the message.

    /// One contour with `count` points spread over a realistic extent.
    fn bitmap_contour(count: usize) -> RtRoiEntry {
        use crate::rtstruct::{RtContour, to_patient};

        let source = RtSource {
            sop_class_uid: CT_IMAGE_STORAGE_UID.to_string(),
            sop_instance_uid: "1.2.3.4.5.6".to_string(),
            series_uid: "1.2.3.4.5".to_string(),
            position: Some([-388.0, -388.0, 16.0]),
            orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
            pixel_spacing: Some([0.2, 0.2]),
        };
        // A circle, so the encoded width per point resembles a real trace
        // rather than a single repeated value.
        let points = (0..count)
            .map(|index| {
                let angle = std::f64::consts::TAU * index as f64 / count as f64;
                to_patient(
                    &source,
                    388.0 + 180.0 * angle.cos(),
                    388.0 + 180.0 * angle.sin(),
                )
            })
            .collect::<Vec<[f64; 3]>>();
        RtRoiEntry {
            number: 1,
            roi: crate::abi::schema::RtRoi {
                name: "BONE".to_string(),
                description: None,
                color: [1, 2, 3],
                generation_algorithm: "AUTOMATIC".to_string(),
                interpreted_type: "ORGAN".to_string(),
                contours: Vec::new(),
            },
            contours: vec![RtContour { points, source }],
        }
    }

    /// The size of the ContourData text one entry produces, without encoding.
    fn contour_data_bytes(entry: &RtRoiEntry) -> usize {
        let needed = contour_buffer_bytes(entry.contours[0].points.len());
        let mut buffer = vec![0u8; needed];
        contour_points_into(&mut buffer, &entry.contours[0].points)
            .expect("the buffer is sized by contour_buffer_bytes")
            .len()
    }

    #[test]
    fn a_contour_too_large_for_its_vr_is_refused() {
        // THE regression pin for the live defect. The fixture is sized so its
        // ContourData crosses 65534 bytes, which is where the old writer's
        // u16 cast wrapped.
        let mut identity = populated();
        identity.rois = vec![bitmap_contour(4000)];
        let bytes = contour_data_bytes(&identity.rois[0]);
        assert!(
            bytes > crate::pr::SHORT_VR_MAX,
            "the fixture must exceed the ceiling to prove the refusal; it is \
             {bytes} bytes"
        );

        let error = super::serialize(&identity).expect_err("an over-long contour is refused");
        assert!(
            error.contains("ContourData"),
            "the refusal names the element: {error}"
        );
        assert!(
            error.contains(&bytes.to_string()),
            "the refusal names the actual byte count ({bytes}): {error}"
        );
        assert!(
            error.contains("4000"),
            "the refusal names the point count: {error}"
        );
    }

    #[test]
    fn a_contour_just_under_the_ceiling_is_written() {
        // The other side of the boundary: the refusal must not be so eager
        // that it rejects a legal polygon. The point count is FOUND by
        // growing until the encoding would not fit, then stepping back one,
        // so the test cannot drift out of range if the formatter changes.
        let mut count = 100;
        while contour_data_bytes(&bitmap_contour(count + 10)) <= crate::pr::SHORT_VR_MAX {
            count += 10;
            assert!(count < 20_000, "the ceiling must be reachable");
        }
        let entry = bitmap_contour(count);
        let bytes = contour_data_bytes(&entry);
        assert!(
            bytes <= crate::pr::SHORT_VR_MAX,
            "the largest accepted fixture must fit: {bytes} bytes"
        );

        let mut identity = populated();
        identity.rois = vec![entry];
        let raw = super::serialize(&identity).expect("a contour that fits is written");

        // The declared length must equal the real value length, which is the
        // fact the old writer got wrong: find the ContourData element in the
        // bytes and compare its length field with the value the encoder
        // produced. A wrapped length is exactly what this catches.
        let needle = [0x3006u16.to_le_bytes(), 0x0050u16.to_le_bytes()].concat();
        let at = raw
            .windows(4)
            .position(|window| window == needle.as_slice())
            .expect("the dataset carries (3006,0050)");
        // `DS` is a SHORT VR, so the length is the two bytes after the tag.
        let declared = u16::from_le_bytes([raw[at + 6], raw[at + 7]]) as usize;
        // String values are padded to EVEN length (PS3.5 6.2), so the
        // declared length is the formatted text rounded up, not the text
        // itself - an odd text length is legal and carries one pad byte.
        let padded = bytes + (bytes & 1);
        assert_eq!(
            declared, padded,
            "the declared length must equal the encoded value (text padded to \
             even); a shorter field is the 16-bit wrap this test exists for"
        );
    }
}
