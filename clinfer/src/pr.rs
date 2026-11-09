// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The result object: a Grayscale Softcopy Presentation State carrying the
//! detections as boxes (PS3.3 Annex N, fr-03 section 6).
//!
//! This is a plain DICOM Part-10 object the worker can STOW like any other
//! instance. It is written by hand, in the same zero-dependency style as the
//! rest of the sidecar: the crate already carries its own DICOM geometry
//! reader, and a writer for one fixed template is smaller than a codec
//! dependency would be.
//!
//! # Why a POLYLINE and not a RECTANGLE
//!
//! GSPS has no RECTANGLE graphic type. A box is drawn as a closed POLYLINE of
//! five points whose last equals the first (PS3.3 C.10.5), which is what
//! every viewer renders as a rectangle. Emitting anything else would be a
//! private convention no viewer would honour.
//!
//! # Coordinates
//!
//! `GraphicData` (0070,0022) is in the PIXEL coordinates of the referenced
//! instance, which is exactly the space the engine reports detections in, so
//! the numbers are carried across unchanged. A viewer that renders them
//! against the source pixels needs the reference sequence below to find that
//! source: the boxes are meaningless without it.
//!
//! # Encoding
//!
//! Explicit VR Little Endian. The origin stores the object as received, so
//! no transcode is involved anywhere on this path.

use crate::conf::{LabelAnchor, LabelProfile, RuleResult};

/// One detected object, in source-image pixels.
///
/// The geometry an overlay draws: carried from the processor's schema box
/// (or from an inference stage a site's script ran), and read verbatim by
/// the serializer.
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// The class label, or the class index when there are no labels.
    pub label: String,
    pub class_index: usize,
    pub confidence: f32,
    /// Source-image coordinates, in pixels, with the origin top left.
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

/// SOP Class UID of the presentation state object.
///
/// The retired Grayscale Softcopy Presentation State Storage class with the
/// `.1` suffix, which is what the frozen Python reference carries. The
/// current unified class (no suffix) is rejected by dcm4che
/// (`PrDicomObject.validateAndGetPresentationStateType`: "does not match
/// any supported DICOM Presentation State type"), so Weasis never lists
/// the object in its presentation-state menu when the current class is
/// used as the primary one.
pub const GSPS_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.11.1";

/// The CURRENT unified Grayscale Softcopy Presentation State class.
///
/// Carried as (0008,001B) Original Specialized SOP Class UID so that a
/// reader that only knows the current class still recognises the object:
/// the primary class must stay the retired one for Weasis, and (0008,001B)
/// is the standard's mechanism for naming a second class one object
/// conforms to (PS3.3 C.12.1.1.2).
pub const CURRENT_GSPS_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.11";

/// Explicit VR Little Endian, the encoding this writer produces.
pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";

/// (0008,0005) SpecificCharacterSet.
///
/// The PR copies the patient identity from the source study, so it can
/// carry non-ASCII bytes; without this declaration they are illegal under
/// the default repertoire (PS3.5 6.1.2.3). ISO_IR 192 is UTF-8, so one
/// fixed value covers every script. The same constant `sr` writes.
pub const SPECIFIC_CHARACTER_SET: &str = crate::sr::SPECIFIC_CHARACTER_SET;

/// One box to draw, in the source instance's pixel coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Box2D {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl Box2D {
    /// The detection's box, which the engine already reports in source pixels.
    #[must_use]
    pub fn of(detection: &Detection) -> Self {
        Self {
            x1: detection.x1,
            y1: detection.y1,
            x2: detection.x2,
            y2: detection.y2,
        }
    }

    /// The closed POLYLINE a GSPS graphic uses for a rectangle: the four
    /// corners and the first corner again.
    #[must_use]
    pub fn as_polyline(&self) -> [f32; 10] {
        [
            self.x1, self.y1, self.x2, self.y1, self.x2, self.y2, self.x1, self.y2, self.x1,
            self.y1,
        ]
    }
}

/// One traced outline to draw, in the source instance's pixel coordinates.
///
/// A contour is the same CLOSED POLYLINE a box is, with the points the
/// processor traced instead of four corners: GSPS has no other outline
/// form, so a mask trace and a rectangle travel the same graphic path.
#[derive(Debug, Clone, PartialEq)]
pub struct Contour2D {
    /// Free text naming what the outline traces, e.g. `Left Lung`.
    pub label: String,
    /// The ring, `[column, row]` per point, first point repeated last.
    pub points: Vec<[f64; 2]>,
}

/// The identity a generated object needs, supplied by the caller so a test
/// can pin it and the worker can mint it fresh.
#[derive(Debug, Clone)]
pub struct PrIdentity {
    /// (0008,0018) of the presentation state itself.
    pub sop_instance_uid: String,
    /// (0020,000D) of the study the overlay belongs to.
    pub study_uid: String,
    /// (0020,000E) of the series it is stored in.
    pub series_uid: String,
    /// The source images the boxes were measured on, grouped by the
    /// series they belong to. The series-level reference names every
    /// analyzed image (the own viewer's scope); every annotation
    /// additionally names its own image (Weasis's scope).
    pub referenced: Vec<ReferencedSeries>,
    /// The largest rows/columns over every referenced image: the displayed
    /// area must cover the whole study (PS3.3 C.10.4), or a viewer crops
    /// while the PR is applied - the overlay must never change the
    /// displayed area.
    pub source_rows: u32,
    pub source_cols: u32,
    /// True when every referenced image is MONOCHROME1. The PR pipeline
    /// replaces the images' photometric interpretation (PS3.4 N.2), so
    /// the Presentation LUT shape must reproduce it.
    pub source_monochrome1: bool,
    /// (0010,0010) copied from the source study. The frozen Python
    /// reference carries patient identity, the origin's SOP-owner check
    /// refuses a derived object whose (coerced) patient disagrees with the
    /// study's, and Weasis groups the PR under the study's patient only
    /// when the name matches. The name is already present in the study's
    /// own images, so the PR copies rather than discloses anything new.
    pub patient_name: Option<String>,
    /// (0010,0020) copied from the source study - same reason as the name.
    pub patient_id: Option<String>,
    /// (0010,0021) Issuer of Patient ID, the "Issuer of Patient ID Macro"
    /// row of AIW-I Appendix V Table V-1 ("Source Copy").
    ///
    /// `Some("")` writes a ZERO-LENGTH element - the workitem explicitly
    /// said "no issuer at this site", and Appendix V copies the value
    /// unaltered. `None` omits the element. No fallback exists: no source
    /// on this path carries an issuer.
    pub patient_id_issuer: Option<String>,
    /// (0010,0030) copied from the source study when it carries one. Type 2
    /// in the Patient module: written empty when unknown, never invented.
    pub patient_birth_date: Option<String>,
    /// (0010,0040) copied from the source study - same Type-2 rule.
    pub patient_sex: Option<String>,
    /// (0020,0010) copied from the source study. Type 2 in the General
    /// Study module.
    pub study_id: Option<String>,
    /// (0008,0050) copied from the source study. Type 2 in the General
    /// Study module.
    pub accession_number: Option<String>,
    /// (0008,0090) copied from the source study. Type 2 in the General
    /// Study module.
    pub referring_physician: Option<String>,
    /// (0008,1010) Station Name, from the workitem's (0040,4028) Performed
    /// Station Name Code Sequence Code Meaning (AIW-I Appendix V Table V-1
    /// Note 4).
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
}

/// One series the boxes reference, and its analyzed images.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferencedSeries {
    /// (0008,1115) item -> (0020,000E).
    pub series_uid: String,
    /// The images the engine actually analyzed, in QIDO order.
    pub images: Vec<ReferencedImage>,
}

/// One analyzed image and its boxes.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferencedImage {
    /// (0008,1150) of the image, Type 1 in the reference item (C.7.6.1).
    pub sop_class_uid: String,
    /// (0008,1155) of the image.
    pub sop_instance_uid: String,
    /// (0008,1160) Referenced Frame Number, written ONLY when the source
    /// is a multi-frame instance and the boxes belong to one of its
    /// frames: without it a presentation state applies to EVERY frame of
    /// the referenced multi-frame SOP (C.11.11, Table 10-3 Image SOP
    /// Instance Reference Macro).
    pub frame_number: Option<u32>,
    /// The boxes measured on this image, in its pixel coordinates.
    pub detections: Vec<Detection>,
    /// The outlines traced on this image, in its pixel coordinates.
    ///
    /// A contour carries no confidence: a segmenter's mask edge has no
    /// score, and writing one would invent it. Kept apart from
    /// `detections` because the two are different kinds of finding - a box
    /// is a detection with a score, a contour is a traced boundary.
    pub contours: Vec<Contour2D>,
}

/// What the rule contributes to the object.
#[derive(Debug, Clone)]
pub struct PrProfile {
    /// `result.color` from the rule, `#RRGGBB` or `#RRGGBBAA`.
    pub color: Option<String>,
    /// `result.series_description`, when the rule names one.
    pub series_description: Option<String>,
    /// `result.label`: where the label box sits relative to the detection
    /// box, and where the leader line starts.
    pub label: LabelProfile,
}

impl PrProfile {
    #[must_use]
    pub fn of(result: &RuleResult) -> Self {
        Self {
            color: result.color.clone(),
            series_description: result.series_description.clone(),
            label: result.label.clone(),
        }
    }
}

/// Serialize one presentation state object.
///
/// `identity.referenced` must carry at least one detection overall: an
/// overlay with no graphics is not a result, and the caller decides that
/// case (fr-03 section 6) rather than this writer producing an empty
/// object.
#[must_use]
pub fn serialize(identity: &PrIdentity, profile: &PrProfile) -> Vec<u8> {
    let mut dataset = Vec::with_capacity(4096);

    // TOP-LEVEL ELEMENTS ARE WRITTEN IN ASCENDING TAG ORDER: PS3.5 7.1 makes
    // that the order of a data set, and a reader is entitled to rely on it.
    // The comments below group the writes by MODULE; the order here is the
    // tag order, which is why one module's rows sometimes appear in two
    // places.
    //
    // (0008,0005) SpecificCharacterSet, Type 1C (PS3.3 C.12.1.1.1): the
    // patient identity copied below may be Cyrillic or CJK, and without
    // this declaration those bytes are illegal under the default
    // repertoire (PS3.5 6.1.2.3). ISO_IR 192 is UTF-8, so one fixed value
    // covers every script, exactly as `sr` does. It is also the lowest
    // top-level tag, so the two rules agree.
    write_cs(&mut dataset, 0x0008, 0x0005, SPECIFIC_CHARACTER_SET);
    // (0008,0016) and (0008,0018) repeat in the dataset what the file meta
    // says, because a viewer that receives the object without its meta (a
    // STOW part, a database row) still has to identify it.
    write_ui(&mut dataset, 0x0008, 0x0016, GSPS_SOP_CLASS_UID);
    write_ui(&mut dataset, 0x0008, 0x0018, &identity.sop_instance_uid);
    // (0008,001B) Original Specialized SOP Class UID: the current unified
    // class, so modern readers recognise what the retired primary class
    // already says.
    write_ui(&mut dataset, 0x0008, 0x001B, CURRENT_GSPS_SOP_CLASS_UID);
    // (0008,0020)/(0008,0030): the content date and time.
    let (date, time) = now_stamp();
    write_da(&mut dataset, 0x0008, 0x0020, &date);
    write_tm(&mut dataset, 0x0008, 0x0030, &time);
    // (0008,0050) AccessionNumber - Type 2 in the General Study module
    // (PS3.3 C.7.2.1).
    write_sh(
        &mut dataset,
        0x0008,
        0x0050,
        identity.accession_number.as_deref().unwrap_or(""),
    );
    // (0008,0060) Modality: a presentation state is "PR". This is the
    // object's own modality, not a workitem attribute.
    write_cs(&mut dataset, 0x0008, 0x0060, "PR");
    // (0008,0070) Manufacturer - Type 2 in the General Equipment module
    // (PS3.3 C.7.5.1): always present, and the device that produced the
    // object is this crate. The report writes the same value.
    write_lo(&mut dataset, 0x0008, 0x0070, crate::sr::MANUFACTURER);
    // (0008,0090) ReferringPhysicianName - Type 2 in the same module.
    write_pn(
        &mut dataset,
        0x0008,
        0x0090,
        identity.referring_physician.as_deref().unwrap_or(""),
    );
    // (0008,1010) Station Name, from the workitem's (0040,4028) Performed
    // Station Name Code Sequence Code Meaning (AIW-I Appendix V Table V-1
    // Note 4). Type 3, and omitted when the workitem carries no such
    // sequence: the mapping runs FROM (0040,4028), so there is no fallback.
    if let Some(station) = &identity.station_name {
        write_sh(&mut dataset, 0x0008, 0x1010, station);
    }
    // (0008,103E) Series Description.
    if let Some(description) = &profile.series_description {
        write_lo(&mut dataset, 0x0008, 0x103E, description);
    }
    // (0008,1115) Referenced Series Sequence -> one item per source
    // series, each holding (0008,1140) Referenced Image Sequence -> every
    // analyzed image with its class+instance pair (C.7.6.1 Type 1).
    write_sequence_start(&mut dataset, 0x0008, 0x1115);
    for series in &identity.referenced {
        write_item_start(&mut dataset);
        // PS3.5 7.1: elements ascend by (group, element) inside an item as
        // well as at the top level. (0008,1140) sorts before (0020,000E), so
        // the referenced-image sequence comes first and the series UID after
        // it - the item used to name the series before the sequence that
        // belongs to it.
        write_sequence_start(&mut dataset, 0x0008, 0x1140);
        for image in &series.images {
            write_item_start(&mut dataset);
            write_ui(&mut dataset, 0x0008, 0x1150, &image.sop_class_uid);
            write_ui(&mut dataset, 0x0008, 0x1155, &image.sop_instance_uid);
            // (0008,1160) binds the reference to ONE frame of a
            // multi-frame instance; absent, the presentation state
            // applies to all of its frames.
            if let Some(frame) = image.frame_number {
                write_is(&mut dataset, 0x0008, 0x1160, &frame.to_string());
            }
            write_item_end(&mut dataset);
        }
        write_sequence_end(&mut dataset);
        write_ui(&mut dataset, 0x0020, 0x000E, &series.series_uid);
        write_item_end(&mut dataset);
    }
    write_sequence_end(&mut dataset);

    // -- Patient identity ---------------------------------------------
    // Copied from the source study (frozen Python reference shape): a
    // derived object without it gets coerced placeholders, and the origin's
    // SOP-owner check then sees a patient disagreement and answers 409 on
    // re-runs.
    //
    // Type 2 means PRESENT, value may be empty (PS3.3 C.7.1.1): when the
    // source study carries no value the attribute is written with a zero
    // length rather than dropped, because an absent attribute says the
    // writer forgot while an empty one says the origin did not know.
    // PatientName/PatientID are Type 2 in the Patient module as well.
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
    // PatientID and PatientBirthDate.
    //
    // `Some("")` writes the zero-length element (the workitem explicitly
    // said "no issuer at this site", and the value is copied unaltered);
    // `None` omits it. No fallback: no source on this path carries an
    // issuer.
    if let Some(issuer) = &identity.patient_id_issuer {
        write_lo(&mut dataset, 0x0010, 0x0021, issuer);
    }
    // (0010,0030) PatientBirthDate and (0010,0040) PatientSex - Type 2 in
    // the Patient module. Copied when the study carries them, empty when
    // it does not; never fabricated.
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

    // -- Study and series identity ------------------------------------
    // (0020,000D)/(0020,000E): the PR belongs to the study it annotates and
    // is a series of that study - the frozen Python reference carries both,
    // and the origin's STOW refuses an object that does not name its study.
    write_ui(&mut dataset, 0x0020, 0x000D, &identity.study_uid);
    write_ui(&mut dataset, 0x0020, 0x000E, &identity.series_uid);
    // (0020,0010) StudyID - Type 2 in the General Study module.
    write_sh(
        &mut dataset,
        0x0020,
        0x0010,
        identity.study_id.as_deref().unwrap_or(""),
    );
    // (0020,0011) Series Number - Type 2 in the Series module, carried by
    // the frozen Python reference: one object per series, always 1.
    write_is(&mut dataset, 0x0020, 0x0011, "1");
    // (0020,0013) Instance Number, Type 2 in the Presentation State
    // Identification Module (C.11.10): one object per series, always 1.
    write_is(&mut dataset, 0x0020, 0x0013, "1");
    // (0032,1060) Requested Procedure Description and (0032,1064) Requested
    // Procedure Code Sequence - the requested-procedure "Source Copy" rows
    // of AIW-I Appendix V Table V-1. Written only when the workitem states
    // them; there is no fallback (see the report).
    if let Some(description) = &identity.requested_procedure_description {
        write_lo(&mut dataset, 0x0032, 0x1060, description);
    }
    if let Some(code) = &identity.requested_procedure_code {
        write_code_sequence(&mut dataset, 0x0032, 0x1064, code);
    }
    // (0040,0008) Scheduled Protocol Code Sequence, mapped from the
    // workitem's (0040,4018) (Appendix V Table V-1: "Copy from: Scheduled
    // Workitem Code Sequence (0040,4018)"), then the remaining
    // requested-procedure "Source Copy" rows.
    if let Some(code) = &identity.scheduled_protocol_code {
        write_code_sequence(&mut dataset, 0x0040, 0x0008, code);
    }
    if let Some(id) = &identity.requested_procedure_id {
        write_sh(&mut dataset, 0x0040, 0x1001, id);
    }
    if let Some(reason) = &identity.reason_for_requested_procedure {
        write_lo(&mut dataset, 0x0040, 0x1002, reason);
    }
    if let Some(code) = &identity.reason_for_requested_procedure_code {
        write_code_sequence(&mut dataset, 0x0040, 0x100A, code);
    }

    // -- The boxes -----------------------------------------------------
    // (0070,0001) Graphic Annotation Sequence -> one item per annotation.
    // Each item holds the graphic itself (0070,0009) and, separately, the
    // text (0070,0008): PS3.3 Table C.10-5 puts Unformatted Text Value
    // (0070,0006) in the TEXT object, not in the graphic object, so a label
    // written beside the polyline would be a stray attribute no viewer
    // reads.
    write_sequence_start(&mut dataset, 0x0070, 0x0001);
    for series in &identity.referenced {
        for image in &series.images {
            for detection in &image.detections {
                write_item_start(&mut dataset);
                // The annotation's own scope: Weasis checks ONLY the
                // annotation-level ReferencedImageSequence against the
                // displayed SOP/frame (architect report 1609, section
                // 2.3), so each annotation carries the image it was
                // measured on. The series-level reference above stays
                // for the own viewer, which reads that one.
                write_sequence_start(&mut dataset, 0x0008, 0x1140);
                write_item_start(&mut dataset);
                write_ui(&mut dataset, 0x0008, 0x1150, &image.sop_class_uid);
                write_ui(&mut dataset, 0x0008, 0x1155, &image.sop_instance_uid);
                if let Some(frame) = image.frame_number {
                    write_is(&mut dataset, 0x0008, 0x1160, &frame.to_string());
                }
                write_item_end(&mut dataset);
                write_sequence_end(&mut dataset);
                // Which layer this annotation is drawn in (Table C.10-5, type 1).
                write_cs(&mut dataset, 0x0070, 0x0002, "AI");

                // -- The graphic: a closed polyline around the box ---------------
                write_sequence_start(&mut dataset, 0x0070, 0x0008);
                write_item_start(&mut dataset);
                // Placement (C.10.5.1, 1C): without a placement mechanism Weasis
                // silently drops the text object. The leader starts at the
                // anchor, on the box's top-left corner or its center as the
                // rule's label profile says.
                let (anchor_x, anchor_y) = match profile.label.anchor {
                    LabelAnchor::TopLeft => (detection.x1, detection.y1),
                    LabelAnchor::Center => (
                        (detection.x1 + detection.x2) / 2.0,
                        (detection.y1 + detection.y2) / 2.0,
                    ),
                };
                // The label box above and to the LEFT of the anchor,
                // explicit: Weasis draws the leader from the box's CENTER to
                // the anchor (PRManager's TextObjectProcessor), so the corner
                // pair is what positions the text. The corners are the RETIRED
                // tags (0070,0010)/(0070,0011): that is the placement the
                // frozen Python reference carries, and dcm4che's dictionary
                // (what Weasis reads as Tag.BoundingBoxTopLeftHandCorner)
                // names the retired pair. The current pair (0070,0007)/(0070,0008)
                // is invisible to it, and anchor-only text falls back to
                // anchor+(20,50) glued to the box's corner. The X deltas come
                // from the rule's label profile: the label sits left of the
                // corner and the leader runs down and to the right.
                let label_center_y = anchor_y + profile.label.offset_y_px as f32;
                // (0070,0006) Unformatted Text Value, VR ST. Built straight into
                // the dataset buffer rather than through a `format!` String: one
                // heap allocation per detection would make the serializer's
                // allocation count grow with the box count, which the ap101 gate
                // forbids.
                write_cs(&mut dataset, 0x0070, 0x0003, "PIXEL");
                write_cs(&mut dataset, 0x0070, 0x0004, "PIXEL");
                write_cs(&mut dataset, 0x0070, 0x0005, "PIXEL");
                write_label(&mut dataset, &detection.label, detection.confidence);
                write_fl(
                    &mut dataset,
                    0x0070,
                    0x0010,
                    &[
                        anchor_x + profile.label.offset_x_left_px as f32,
                        label_center_y - 60.0,
                    ],
                );
                write_fl(
                    &mut dataset,
                    0x0070,
                    0x0011,
                    &[
                        anchor_x + profile.label.offset_x_right_px as f32,
                        label_center_y + 60.0,
                    ],
                );
                // (0070,0012) Bounding Box Text Horizontal Justification,
                // part of the bounding-box placement mechanism: the text
                // starts at the box's left edge.
                write_cs(&mut dataset, 0x0070, 0x0012, "LEFT");
                write_fl(&mut dataset, 0x0070, 0x0014, &[anchor_x, anchor_y]);
                write_cs(&mut dataset, 0x0070, 0x0015, "Y");
                write_item_end(&mut dataset);
                write_sequence_end(&mut dataset);
                write_sequence_start(&mut dataset, 0x0070, 0x0009);
                write_item_start(&mut dataset);
                // (0070,0005) Graphic Annotation Units, type 1. PIXEL: the points
                // are image-relative with sub-pixel resolution, which is what the
                // detector's coordinates already are.
                write_cs(&mut dataset, 0x0070, 0x0005, "PIXEL");
                // (0070,0020) Graphic Dimensions and (0070,0021) Number of Graphic
                // Points are Type 1 (PS3.3 C.10.5.1): two dimensions, five points
                // for the closed polyline. (0070,0021) is US in the dictionary
                // (PS3.6), so it is written as a binary US value, not as IS text.
                write_us(&mut dataset, 0x0070, 0x0020, 2);
                write_us(&mut dataset, 0x0070, 0x0021, 5);
                let line = Box2D::of(detection).as_polyline();
                write_fl(&mut dataset, 0x0070, 0x0022, &line);
                // (0070,0023) Graphic Type - required, and the value a viewer uses
                // to decide how to join the points.
                write_cs(&mut dataset, 0x0070, 0x0023, "POLYLINE");
                // (0070,0024) Graphic Filled, type 1C: required because the first
                // point equals the last, so the polyline is closed (Table C.10-5).
                // N: an outline, not a translucent slab over the finding.
                write_cs(&mut dataset, 0x0070, 0x0024, "N");
                write_item_end(&mut dataset);
                write_sequence_end(&mut dataset);
                write_item_end(&mut dataset);
            }
            for contour in &image.contours {
                // A contour is the same annotation object as a box: its own
                // scope, then a graphic and a text object. Only the graphic
                // differs - the traced ring instead of four corners.
                write_item_start(&mut dataset);
                write_sequence_start(&mut dataset, 0x0008, 0x1140);
                write_item_start(&mut dataset);
                write_ui(&mut dataset, 0x0008, 0x1150, &image.sop_class_uid);
                write_ui(&mut dataset, 0x0008, 0x1155, &image.sop_instance_uid);
                if let Some(frame) = image.frame_number {
                    write_is(&mut dataset, 0x0008, 0x1160, &frame.to_string());
                }
                write_item_end(&mut dataset);
                write_sequence_end(&mut dataset);
                write_cs(&mut dataset, 0x0070, 0x0002, "AI");

                // -- The graphic: the traced ring --------------------------------
                write_sequence_start(&mut dataset, 0x0070, 0x0008);
                write_item_start(&mut dataset);
                // The anchor is the ring's first point, which is also where
                // the ring closes: a corner of the outline, so the label
                // never lands on top of the traced structure.
                let (anchor_x, anchor_y) =
                    (contour.points[0][0] as f32, contour.points[0][1] as f32);
                write_cs(&mut dataset, 0x0070, 0x0003, "PIXEL");
                write_cs(&mut dataset, 0x0070, 0x0004, "PIXEL");
                write_cs(&mut dataset, 0x0070, 0x0005, "PIXEL");
                // No confidence: see `Contour2D`. A contour's label is the
                // structure name alone, with no " NN%" suffix, because the
                // mask edge has no score to report.
                write_label_without_confidence(&mut dataset, &contour.label);
                write_fl(
                    &mut dataset,
                    0x0070,
                    0x0010,
                    &[
                        anchor_x + profile.label.offset_x_left_px as f32,
                        anchor_y + profile.label.offset_y_px as f32 - 60.0,
                    ],
                );
                write_fl(
                    &mut dataset,
                    0x0070,
                    0x0011,
                    &[
                        anchor_x + profile.label.offset_x_right_px as f32,
                        anchor_y + profile.label.offset_y_px as f32 + 60.0,
                    ],
                );
                write_cs(&mut dataset, 0x0070, 0x0012, "LEFT");
                write_fl(&mut dataset, 0x0070, 0x0014, &[anchor_x, anchor_y]);
                write_cs(&mut dataset, 0x0070, 0x0015, "Y");
                write_item_end(&mut dataset);
                write_sequence_end(&mut dataset);
                write_sequence_start(&mut dataset, 0x0070, 0x0009);
                write_item_start(&mut dataset);
                write_cs(&mut dataset, 0x0070, 0x0005, "PIXEL");
                write_us(&mut dataset, 0x0070, 0x0020, 2);
                // (0070,0021) is the point COUNT, so it comes from the ring
                // rather than a constant. The parser floors it at three and
                // caps it, so it fits a US by construction.
                let count = u16::try_from(contour.points.len()).unwrap_or(u16::MAX);
                write_us(&mut dataset, 0x0070, 0x0021, count);
                // Graphic Data is FL, so the ring is widened once into a
                // scratch vector here. This is the one allocation per
                // contour the serializer makes, and it is proportional to
                // what the contour already occupies.
                let mut line = Vec::with_capacity(contour.points.len() * 2);
                for point in &contour.points {
                    line.push(point[0] as f32);
                    line.push(point[1] as f32);
                }
                write_fl(&mut dataset, 0x0070, 0x0022, &line);
                write_cs(&mut dataset, 0x0070, 0x0023, "POLYLINE");
                // The producer closed the ring, so the polyline is closed
                // (Table C.10-5). N: an outline, not a filled slab over the
                // structure - the image underneath stays readable.
                write_cs(&mut dataset, 0x0070, 0x0024, "N");
                write_item_end(&mut dataset);
                write_sequence_end(&mut dataset);
                write_item_end(&mut dataset);
            }
        }
    }
    write_sequence_end(&mut dataset);

    // Displayed Area Module (C.10.4, mandatory in the GSPS IOD): NEUTRAL
    // by design - top-left 0\0, bottom-right beyond the source image,
    // SCALE TO FIT with square pixels. A crop here would change what a
    // viewer shows only while the overlay is on.
    //
    // The item lives under (0070,005A) Displayed Area Selection Sequence,
    // which is the attribute the module defines; the pixel spacing pair is
    // (0070,0101) Vertical\Horizontal, VR DS.
    write_sequence_start(&mut dataset, 0x0070, 0x005A);
    write_item_start(&mut dataset);
    // (0070,0052) Displayed Area Top Left Hand Corner, VR SL: the pixel
    // coordinates of the corner, as a row\column pair.
    write_sl_pair(&mut dataset, 0x0070, 0x0052, 0, 0);
    // (0070,0053) Displayed Area Bottom Right Hand Corner, VR SL. Extends
    // PAST the source image (max dims + 200) so SCALE TO FIT has slack and
    // the displayed area never crops what the rule annotated.
    write_sl_pair(
        &mut dataset,
        0x0070,
        0x0053,
        identity.source_cols + 200,
        identity.source_rows + 200,
    );
    write_cs(&mut dataset, 0x0070, 0x0100, "SCALE TO FIT");
    // (0070,0101) Presentation Pixel Spacing, VR DS.
    write_ds(&mut dataset, 0x0070, 0x0101, "1\\1");
    // (0070,0102) Presentation Pixel Aspect Ratio, VR IS.
    write_is(&mut dataset, 0x0070, 0x0102, "1\\1");
    write_item_end(&mut dataset);
    write_sequence_end(&mut dataset);

    // -- Presentation State relationship ------------------------------
    // (0070,0060) Graphic Layer Sequence: one layer, named and coloured.
    write_sequence_start(&mut dataset, 0x0070, 0x0060);
    write_item_start(&mut dataset);
    write_cs(&mut dataset, 0x0070, 0x0002, "AI");
    // (0070,0062) Graphic Layer Order. Not (0070,0003): that tag is
    // Bounding Box Annotation Units, and an IS there is unreadable.
    write_is(&mut dataset, 0x0070, 0x0062, "1");
    // (0070,0067) Graphic Layer Recommended Display RGB Value - the one
    // colour representation every viewer honours. Not (0070,006C), which is
    // not in the dictionary at all. The rule's colour is `#RRGGBB`; a rule
    // with no colour gets a neutral red.
    let rgb = parse_rgb(profile.color.as_deref()).unwrap_or([255, 0, 0]);
    write_us3(&mut dataset, 0x0070, 0x0067, rgb);
    write_item_end(&mut dataset);
    write_sequence_end(&mut dataset);

    // -- Presentation State identification ----------------------------
    // (0070,0080) Content Label - short, human-readable, and required.
    write_cs(&mut dataset, 0x0070, 0x0080, "AI DETECTIONS");
    // (0070,0081) Content Description - Type 2 (C.11.10): written empty
    // when the rule names no series description, never dropped.
    write_lo(
        &mut dataset,
        0x0070,
        0x0081,
        profile.series_description.as_deref().unwrap_or(""),
    );
    // (0070,0082)/(0070,0083) Presentation Creation Date/Time - Type 1
    // (C.11.10). These are the PRESENTATION's stamp, not the study's:
    // (0008,0020)/(0008,0030) above are the Study module's Type-2 pair.
    write_da(&mut dataset, 0x0070, 0x0082, &date);
    write_tm(&mut dataset, 0x0070, 0x0083, &time);
    // (0070,0084) Content Creator's Name.
    //
    // No AI disclaimer is written: the sidecar is transport, the model is
    // a third party's, and stating what the model means is the vendor's
    // obligation, not this crate's. The viewer's own NOT FOR DIAGNOSTIC USE
    // banner is the deployment's answer to that question.
    write_pn(&mut dataset, 0x0070, 0x0084, "clinfer");

    // Softcopy Presentation LUT Module (C.11.6, mandatory): when a PR is
    // applied, the image's photometric interpretation is ignored (PS3.4
    // N.2) and polarity comes from this shape alone. MONOCHROME1 needs
    // INVERSE to keep the original appearance.
    write_cs(
        &mut dataset,
        0x2050,
        0x0020,
        if identity.source_monochrome1 {
            "INVERSE"
        } else {
            "IDENTITY"
        },
    );

    // -- Assemble the Part-10 file ------------------------------------
    let mut out = Vec::with_capacity(dataset.len() + 256);
    out.extend_from_slice(&[0u8; 128]); // preamble
    out.extend_from_slice(b"DICM");
    write_file_meta(&mut out, identity);
    out.extend_from_slice(&dataset);
    out
}

/// (0002,xxxx) group: the canonical trio plus the two information UIDs a
/// reader uses to identify the object before parsing the dataset.
fn write_file_meta(out: &mut Vec<u8>, identity: &PrIdentity) {
    let meta = file_meta_bytes(identity);
    // (0002,0000) File Meta Information Group Length - the length of the
    // elements that follow it, which is what every parser walks first.
    write_explicit_header(out, 0x0002, 0x0000, b"UL", 4);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
}

fn file_meta_bytes(identity: &PrIdentity) -> Vec<u8> {
    let mut meta = Vec::with_capacity(256);
    // Every value here goes through `write_string`, the same helper the
    // dataset uses, so one padding rule covers the whole object. Writing
    // the UIDs by hand with their raw length put an ODD length on
    // (0002,0003) whenever a UID had an odd character count - illegal for
    // every VR (PS3.5 6.2) and enough to make a parser read past the value.
    write_string(&mut meta, 0x0002, 0x0001, b"OB", "01");
    write_string(&mut meta, 0x0002, 0x0002, b"UI", GSPS_SOP_CLASS_UID);
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

/// The largest value a SHORT-VR element's 16-bit length field can carry.
///
/// PS3.5 Table 7.1-2 lists the VRs whose length stays 16-bit, and the field
/// is a plain unsigned 16-bit integer, so 65535 is the arithmetic ceiling.
/// A value is padded to an even length, so in practice a short-VR element
/// holds at most 65534 bytes of value.
pub(crate) const SHORT_VR_MAX: usize = 65_534;

/// Whether a VR takes the long header form: VR, two reserved bytes, then a
/// four-byte length (PS3.5 Table 7.1-1).
///
/// This is the complement of Table 7.1-2, which lists the VRs whose length
/// field stays 16-bit: AE, AS, AT, CS, DA, DS, DT, FL, FD, IS, LO, LT, PN,
/// SH, SL, SS, ST, TM, UI, UL and US. `UL` is in that list, so it is a
/// short VR and must NOT be added here.
pub(crate) fn is_long_vr(vr: &[u8; 2]) -> bool {
    matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    )
}

/// Whether an element of `len` bytes can be written with `vr`'s header.
///
/// A LONG VR has a four-byte length, so its ceiling is far above anything a
/// serializer here produces and is not checked. A SHORT VR has a two-byte
/// length: a value beyond [`SHORT_VR_MAX`] has NO correct encoding as that
/// VR, and writing one anyway truncates the length to its low 16 bits. The
/// result is a file whose length field disagrees with its content, which a
/// receiver resolves by reading value bytes as the next element's VR - the
/// exact failure a live structure set produced (69,342 bytes declared as
/// 3,806, and the two digits of a contour coordinate read as a VR).
#[must_use]
pub(crate) fn fits_explicit_header(vr: &[u8; 2], len: usize) -> bool {
    is_long_vr(vr) || len <= SHORT_VR_MAX
}

pub(crate) fn write_explicit_header(
    out: &mut Vec<u8>,
    group: u16,
    element: u16,
    vr: &[u8; 2],
    len: usize,
) {
    // A caller that reaches this with too much data has already been given
    // the chance to refuse (see `fits_explicit_header`); writing the header
    // would put a wrapped length on the wire, so the wrap is a bug and not a
    // degradation to accept quietly. Debug builds stop here; release builds
    // write a length that at least cannot be mistaken for a small value,
    // because the alternative - a plausible-looking truncated length - is
    // what makes the resulting file parse as garbage rather than fail.
    debug_assert!(
        fits_explicit_header(vr, len),
        "element ({group:04X},{element:04X}) {vr:?} of {len} bytes exceeds the \
         16-bit length of its VR"
    );
    let len = if is_long_vr(vr) {
        len
    } else {
        len.min(SHORT_VR_MAX)
    };
    out.extend_from_slice(&group.to_le_bytes());
    out.extend_from_slice(&element.to_le_bytes());
    out.extend_from_slice(vr);
    if is_long_vr(vr) {
        out.extend_from_slice(&[0, 0]); // reserved
        out.extend_from_slice(&(len as u32).to_le_bytes());
    } else {
        out.extend_from_slice(&(len as u16).to_le_bytes());
    }
}

/// A string element, NUL- or space-padded to even length as its VR requires.
pub(crate) fn write_string(out: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &str) {
    let bytes = value.as_bytes();
    let pad = if bytes.len().is_multiple_of(2) { 0 } else { 1 };
    // UI pads with NUL, every other string VR pads with a space (PS3.5 6.2).
    let pad_byte = if vr == b"UI" { 0u8 } else { b' ' };
    write_explicit_header(out, group, element, vr, bytes.len() + pad);
    out.extend_from_slice(bytes);
    if pad == 1 {
        out.push(pad_byte);
    }
}

/// The longest a DS value may be, in characters (PS3.5 Table 6.2-1).
///
/// The limit is sixteen INCLUDING a sign, a decimal point and an exponent,
/// and it applies to each value of a multi-valued element rather than to
/// the element: `1.5\1.5` is two legal values, not one eighteen-character
/// one.
pub(crate) const DS_MAX_CHARS: usize = 16;

/// The shortest of the two DS forms that fits: plain digits when they fit,
/// exponent form when they do not.
///
/// `digits` and `decimals` are the precision of the fixed-point form,
/// chosen by the caller because it knows what the value MEANS (a slice
/// position deserves a millimetre, a normalized coordinate does not). The
/// fixed form is preferred whenever it is legal AND still carries the
/// value: `-0.001` reads as a position, while `-1e-3` reads as a
/// computation, but at three decimals a 5e-8 coordinate rounds to `0.000`,
/// which is a different number and would move a contour point to the
/// origin - so that value goes to the exponent form instead.
///
/// A value that fits neither form is written as `0`. Every magnitude this
/// crate writes is bounded by the image geometry, so the branch exists to
/// keep the promise "always a legal DS" for any input rather than to
/// describe a case that occurs.
pub(crate) fn ds_text(value: f64, digits: usize, decimals: usize) -> String {
    let mut out = String::with_capacity(DS_MAX_CHARS);
    write_ds_into(&mut StringSink(&mut out), value, digits, decimals);
    out
}

/// A `core::fmt::Write` over a `String`, so [`ds_text`] and
/// [`ds_text_into`] share one implementation.
struct StringSink<'a>(&'a mut String);

impl core::fmt::Write for StringSink<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        self.0.push_str(text);
        Ok(())
    }
}

/// [`ds_text`] into a caller-owned buffer, allocating nothing.
///
/// The per-frame paths (a segmentation's functional groups write two DS
/// values per frame) cannot afford a `String` per value, so the rendering
/// lives here and [`ds_text`] is the owned wrapper.
pub(crate) fn ds_text_into(
    buffer: &mut [u8; DS_MAX_CHARS * 2],
    value: f64,
    digits: usize,
    decimals: usize,
) -> &str {
    let len = {
        let mut stack = crate::seg::StackBuf {
            buf: buffer,
            len: 0,
        };
        write_ds_into(&mut stack, value, digits, decimals);
        stack.len
    };
    std::str::from_utf8(&buffer[..len]).unwrap_or("0")
}

/// The one rendering both DS writers share.
fn write_ds_into<W: core::fmt::Write + ?Sized>(
    sink: &mut W,
    value: f64,
    digits: usize,
    decimals: usize,
) {
    if !value.is_finite() {
        // NaN and the infinities have no DS spelling; a zero is the only
        // legal stand-in, and a caller that can tell the difference
        // validates before it gets here.
        let _ = sink.write_str("0");
        return;
    }
    // The exact spelling first: the shortest decimal that round-trips the
    // value. A value copied from a source dataset arrived as a legal DS of
    // at most sixteen characters, and no round-tripping string is shorter
    // than the shortest one, so a copied Pixel Spacing of 0.48828125 keeps
    // all of its digits instead of collapsing to the caller's three
    // decimals. Values whose exact spelling does not fit - a computed
    // contour point, whose shortest form can run to seventeen characters
    // plus an exponent - fall through to the caller's precision below.
    if emit_within(sink, format_args!("{value}")) {
        return;
    }
    let digits = digits.clamp(1, 15);
    let decimals = decimals.min(digits);
    // The fixed-point form is preferred only when it still CARRIES the
    // value: at three decimals a 5e-8 coordinate rounds to "0.000", which
    // is a different number and would move a contour point to the origin.
    // The fixed form is therefore used only when it is non-zero for a
    // non-zero input, and the exponent form takes over otherwise.
    let fixed_carries = value == 0.0 || value.abs() >= 0.5 * 10f64.powi(-(decimals as i32));
    if fixed_carries && emit_within(sink, format_args!("{value:.decimals$}")) {
        return;
    }
    if emit_within(sink, format_args!("{value:.digits$e}")) {
        return;
    }
    if emit_within(sink, format_args!("{value:e}")) {
        return;
    }
    // The last resort. Every f64 this crate writes is bounded by the image
    // geometry, so this branch keeps the contract ("always a legal DS")
    // rather than describing a case that occurs.
    let _ = sink.write_str("0");
}
/// Render `text` into a stack buffer and write it to `sink` only if it is a
/// legal DS value.
///
/// The probe is what makes "an exponent form only when the plain form does
/// not fit" honest: without it the writer would have to guess at the width
/// of a formatted float, and every guess is wrong for some exponent.
///
/// A failed format and an empty rendering are both rejections. The buffer
/// truncates instead of erroring, so a truncated value arrives here as a
/// failed write rather than as a short string; and an empty string is not a
/// legal DS value under PS3.5 Table 6.2-1, so accepting it would put a
/// zero-length element in the dataset.
fn emit_within<W: core::fmt::Write + ?Sized>(sink: &mut W, text: core::fmt::Arguments<'_>) -> bool {
    let mut scratch = [0u8; DS_MAX_CHARS * 2];
    let len = {
        let mut probe = crate::seg::StackBuf {
            buf: &mut scratch,
            len: 0,
        };
        if core::fmt::write(&mut probe, text).is_err() {
            return false;
        }
        probe.len
    };
    let Ok(candidate) = std::str::from_utf8(&scratch[..len]) else {
        return false;
    };
    if candidate.is_empty() || candidate.len() > DS_MAX_CHARS {
        return false;
    }
    let _ = sink.write_str(candidate);
    true
}

pub(crate) fn write_ui(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"UI", value);
}
pub(crate) fn write_cs(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"CS", value);
}
pub(crate) fn write_lo(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"LO", value);
}
/// (0008,0119) Long Code Value: UC, the alternate to (0008,0100) for a code
/// value longer than SH can carry (PS3.3 Table 8.8-1).
pub(crate) fn write_uc(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"UC", value);
}
pub(crate) fn write_da(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"DA", value);
}
pub(crate) fn write_tm(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"TM", value);
}
pub(crate) fn write_pn(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"PN", value);
}
pub(crate) fn write_sh(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"SH", value);
}
/// A Short Text (ST) element, any group and element.
///
/// ST is the VR of free text such as Derivation Description (0008,2111)
/// and ROI Description (3006,0028). There is no ST row in the presentation
/// state tables, so this helper was added for the structure set writer.
pub(crate) fn write_st(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"ST", value);
}
/// (0070,0101) Presentation Pixel Spacing: DS, a decimal-string pair.
pub(crate) fn write_ds(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"DS", value);
}

/// (0070,0006) Unformatted Text Value, built in place.
///
/// This is the text of a Text Object Sequence item (0070,0008), which is
/// where PS3.3 Table C.10-5 puts the label a reader sees beside the box.
/// The VR is ST (the dictionary is explicit). `format!` would allocate a
/// String per detection, which the serializer's allocation gate forbids;
/// this writes the same bytes into the caller's buffer with no
/// intermediate owner.
fn write_label(out: &mut Vec<u8>, label: &str, confidence: f32) {
    // "label 87%" - the confidence rounded to whole percent.
    let percent = (confidence * 100.0).round().clamp(0.0, 100.0) as u32;
    let mut digits = [0u8; 3];
    let mut count = 0usize;
    let mut value = percent;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }

    let body_len = label.len() + 1 + count + 1;
    let pad = if body_len.is_multiple_of(2) { 0 } else { 1 };
    write_explicit_header(out, 0x0070, 0x0006, b"ST", body_len + pad);
    out.extend_from_slice(label.as_bytes());
    out.push(b' ');
    for index in (0..count).rev() {
        out.push(digits[index]);
    }
    out.push(b'%');
    if pad == 1 {
        out.push(b' ');
    }
}

/// (0070,0006) Unformatted Text Value for a finding that carries no score.
///
/// A contour has no confidence - a mask edge has no operating point - so
/// the text is the label alone. Writing the shared `write_label` with a
/// zero confidence would put "0%" beside every trace, which reads as a
/// measurement nobody made.
fn write_label_without_confidence(out: &mut Vec<u8>, label: &str) {
    let pad = if label.len().is_multiple_of(2) { 0 } else { 1 };
    write_explicit_header(out, 0x0070, 0x0006, b"ST", label.len() + pad);
    out.extend_from_slice(label.as_bytes());
    if pad == 1 {
        out.push(b' ');
    }
}

/// (0070,0062) Graphic Layer Order is IS; it is stored as text.
pub(crate) fn write_is(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    write_string(out, group, element, b"IS", value);
}

/// A single US value: (0070,0020) Graphic Dimensions.
fn write_us(out: &mut Vec<u8>, group: u16, element: u16, value: u16) {
    write_explicit_header(out, group, element, b"US", 2);
    out.extend_from_slice(&value.to_le_bytes());
}

/// (0070,0008) Graphic Type - an even-length CS.
pub(crate) fn write_sequence_start(out: &mut Vec<u8>, group: u16, element: u16) {
    // A sequence whose length is not known while streaming uses the
    // undefined length sentinel with an explicit delimiter, which is legal
    // and is what a hand-written serializer emits naturally.
    //
    // The sentinel IS the length: the header has to carry 0xFFFFFFFF, not a
    // separate length of 0 followed by four more bytes. Writing both puts
    // eight bytes where a four-byte length belongs, so a receiver reads the
    // sequence as empty and then finds the first item's delimiter where it
    // expected a tag - the file stops being parseable at the first sequence.
    write_explicit_header(out, group, element, b"SQ", 0xFFFF_FFFF);
}

pub(crate) fn write_sequence_end(out: &mut Vec<u8>) {
    out.extend_from_slice(&0xFFFEu16.to_le_bytes());
    out.extend_from_slice(&0xE0DDu16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

pub(crate) fn write_item_start(out: &mut Vec<u8>) {
    out.extend_from_slice(&0xFFFEu16.to_le_bytes());
    out.extend_from_slice(&0xE000u16.to_le_bytes());
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
}

pub(crate) fn write_item_end(out: &mut Vec<u8>) {
    out.extend_from_slice(&0xFFFEu16.to_le_bytes());
    out.extend_from_slice(&0xE00Du16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

/// Write a one-item code sequence from a workitem code.
///
/// The `CodeSequenceMacro` shape of PS3.3 Table 8.8-1: `(group,element)` SQ
/// holding one item with (0008,0100) Code Value, (0008,0102) Coding Scheme
/// Designator and (0008,0104) Code Meaning, in ascending tag order. It is
/// the shape AIW-I Appendix V Table V-1 asks for in its three code-sequence
/// mappings: (0040,4018) -> (0040,0008) Scheduled Protocol Code, (0032,1064)
/// Requested Procedure Code and (0040,100A) Reason for Requested Procedure
/// Code.
///
/// Coding Scheme Version (0008,0103) is type 1C - required only for a
/// versioned scheme - and a workitem code from the deployment's private
/// scheme carries no version, so it is not written.
///
/// The item carries an undefined length with its delimiter, matching the
/// sequences this writer already emits.
fn write_code_sequence(
    out: &mut Vec<u8>,
    group: u16,
    element: u16,
    code: &crate::dicomweb::WorkitemCode,
) {
    write_sequence_start(out, group, element);
    write_item_start(out);
    write_sh(out, 0x0008, 0x0100, &code.value);
    write_sh(out, 0x0008, 0x0102, &code.scheme);
    write_lo(out, 0x0008, 0x0104, &code.meaning);
    write_item_end(out);
    write_sequence_end(out);
}

/// (0070,0022) Graphic Data - FL (float single), 2 values per point.
fn write_fl(out: &mut Vec<u8>, group: u16, element: u16, points: &[f32]) {
    write_explicit_header(out, group, element, b"FL", points.len() * 4);
    for value in points {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

/// A pair of SL values, written as two little-endian i32 with no heap
/// allocation: the Displayed Area corners (0070,0052)/(0070,0053) are the
/// only multi-valued binary attributes the serializer needs.
fn write_sl_pair(out: &mut Vec<u8>, group: u16, element: u16, first: u32, second: u32) {
    write_explicit_header(out, group, element, b"SL", 8);
    out.extend_from_slice(&(first as i32).to_le_bytes());
    out.extend_from_slice(&(second as i32).to_le_bytes());
}

/// (0070,006C) Graphic Layer Recommended Display RGB Value - three US.
fn write_us3(out: &mut Vec<u8>, group: u16, element: u16, rgb: [u8; 3]) {
    write_explicit_header(out, group, element, b"US", 6);
    for value in rgb {
        out.extend_from_slice(&u16::from(value).to_le_bytes());
    }
}

/// `#RRGGBB`, with any alpha component ignored: the recommended-display
/// attribute has no alpha channel.
fn parse_rgb(color: Option<&str>) -> Option<[u8; 3]> {
    let text = color?.strip_prefix('#')?;
    if text.len() < 6 {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&text[at..at + 2], 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// Today's UTC date and time, in the DA and TM formats the object carries.
pub(crate) fn now_stamp() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    (
        format!("{y:04}{m:02}{d:02}"),
        format!("{:02}{:02}{:02}", rem / 3600, (rem % 3600) / 60, rem % 60),
    )
}

/// Days-since-epoch to a civil date (Howard Hinnant's algorithm, the same
/// one the server's stamp writer uses).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detection(label: &str, x1: f32, y1: f32, x2: f32, y2: f32) -> Detection {
        Detection {
            label: label.to_string(),
            class_index: 0,
            confidence: 0.5,
            x1,
            y1,
            x2,
            y2,
        }
    }

    fn identity() -> PrIdentity {
        identity_with(vec![detection("Nodule", 1.0, 2.0, 3.0, 4.0)])
    }

    fn identity_with(detections: Vec<Detection>) -> PrIdentity {
        PrIdentity {
            sop_instance_uid: "1.2.3.4.5".to_string(),
            study_uid: "1.2.3.4.6".to_string(),
            series_uid: "1.2.3.4.7".to_string(),
            referenced: vec![ReferencedSeries {
                series_uid: "1.2.3.4.8".to_string(),
                images: vec![ReferencedImage {
                    sop_class_uid: "1.2.3.4.10".to_string(),
                    sop_instance_uid: "1.2.3.4.9".to_string(),
                    frame_number: None,
                    detections,
                    contours: Vec::new(),
                }],
            }],
            source_rows: 400,
            source_cols: 300,
            source_monochrome1: false,
            patient_name: Some("Fixture^PR".to_string()),
            patient_id: Some("PR-FIXTURE".to_string()),
            patient_id_issuer: Some("SITE-A".to_string()),
            patient_birth_date: Some("19700101".to_string()),
            patient_sex: Some("O".to_string()),
            study_id: Some("STUDY-1".to_string()),
            accession_number: Some("ACC-1".to_string()),
            referring_physician: Some("REFERRER^A".to_string()),
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
        }
    }

    /// The same identity with a source study that carried none of the
    /// Type-2 attributes: the writer must still emit them, empty.
    fn identity_without_study_values() -> PrIdentity {
        PrIdentity {
            patient_name: None,
            patient_id: None,
            patient_birth_date: None,
            patient_sex: None,
            study_id: None,
            accession_number: None,
            referring_physician: None,
            ..identity()
        }
    }

    /// Every occurrence of an 8-digit hex tag in the dataset.
    #[test]
    fn a_ds_value_is_always_within_the_sixteen_characters_the_vr_allows() {
        // PS3.5 Table 6.2-1: a DS value is at most sixteen characters, sign,
        // decimal point and exponent included. Every value a contour or a
        // spacing can carry has to fit, and a value that cannot be written in
        // sixteen characters has no legal encoding at all.
        let cases = [
            5e-8_f64,
            -5e-8,
            1.0,
            -1.0,
            123_456_789.123_456_8,
            -123_456_789.123_456_8,
            0.000123456789012,
            1234567.891,
            1e-12,
            1e12,
            0.7,
            -13.75,
            f64::MAX.min(1e30),
        ];
        for value in cases {
            let text = super::ds_text(value, 6, 3);
            assert!(
                text.chars().count() <= super::DS_MAX_CHARS,
                "DS text for {value} is {} chars: {text:?}",
                text.chars().count()
            );
            assert!(
                text.parse::<f64>().is_ok(),
                "DS text for {value} reads back as a number: {text:?}"
            );
            // And the value it carries is the one that went in, to the
            // precision the writer promises.
            let read: f64 = text.parse().expect("parses");
            let tolerance = (value.abs() * 1e-3).max(1e-9);
            assert!(
                (read - value).abs() <= tolerance,
                "{value} read back as {read} from {text:?}"
            );
        }
        // The precision is not silently degraded: a millimetre-scale value
        // keeps three decimals.
        assert_eq!(super::ds_text(1.5, 6, 3), "1.5");
        assert_eq!(super::ds_text(-13.75, 6, 3), "-13.75");
    }

    #[test]
    fn a_copied_geometry_value_keeps_every_digit_the_source_had() {
        // A source dataset states its Pixel Spacing and its direction
        // cosines as DS values, and a SEG claims to COPY that geometry. A
        // renderer that runs every value through the caller's three
        // decimals turns 0.48828125 into 0.488 - a sub-millimetre shift of
        // every mask column - and a cosine the source stated to eight
        // decimals into three, after which the orientation is no longer the
        // unit vector the source declared.
        assert_eq!(super::ds_text(0.48828125, 6, 3), "0.48828125");
        assert_eq!(super::ds_text(0.12345678, 6, 3), "0.12345678");
        // A direction cosine carried at full f64 precision needs seventeen
        // characters, one more than the VR allows, so the caller's three
        // decimals are the answer. Geometry arrives as DS-text-bounded
        // values in practice; this case exists to show the fallback is real
        // and not a formality.
        let cosine = 0.707_106_781_186_547_5_f64;
        assert_eq!(super::ds_text(cosine, 6, 3), "0.707");
        assert_eq!(super::ds_text(-cosine, 6, 3), "-0.707");
        // 1/3 has no exact decimal spelling; its shortest round-trip form
        // runs past sixteen characters, so the caller's three decimals
        // remain the answer - the fallback is the whole reason the exact
        // probe is a probe rather than a replacement.
        assert_eq!(super::ds_text(1.0 / 3.0, 6, 3), "0.333");
    }

    #[test]
    fn an_empty_rendering_is_not_a_ds_value() {
        // PS3.5 Table 6.2-1 measures a DS value in characters, and a
        // zero-length one is not a number: a reader that parses it gets an
        // error where it expected a value. The probe has to say no rather
        // than copy the empty candidate into the dataset.
        let mut sink = String::new();
        assert!(!super::emit_within(&mut sink, format_args!("")));
        assert!(sink.is_empty(), "nothing may be written: {sink:?}");
    }
    fn tag_at(bytes: &[u8], group: u16, element: u16) -> bool {
        let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
        bytes.windows(4).any(|window| window == needle.as_slice())
    }

    /// The VRs whose header carries a four-byte length (PS3.5 Table 7.1-1).
    const LONG_VR: &[&[u8; 2]] = &[
        b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT", b"UV",
    ];

    /// The dataset of a Part-10 file: past the preamble, the magic and the
    /// meta group, whose own length says where the dataset begins.
    fn dataset_bytes(bytes: &[u8]) -> &[u8] {
        let meta_length =
            u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]) as usize;
        &bytes[144 + meta_length..]
    }

    /// The top-level tags of a dataset, in the order they are written.
    ///
    /// Each element is measured by its own header rather than found by a
    /// byte scan: a scan cannot tell a sequence's contents from the dataset
    /// around it, so it would report item bodies and delimiters as elements.
    fn top_level_tags(bytes: &[u8]) -> Vec<(u16, u16)> {
        let data = dataset_bytes(bytes);
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

    /// The tags directly inside the FIRST item of the sequence at `seq_tag`.
    ///
    /// `top_level_tags` cannot answer this: it steps over a sequence by its
    /// terminator, so an order defect inside an item is invisible to it. This
    /// walk descends into the sequence, then into its first item, and returns
    /// the item's own elements in the order they were written.
    fn first_item_tags(bytes: &[u8], seq_tag: (u16, u16)) -> Vec<(u16, u16)> {
        let data = dataset_bytes(bytes);
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
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
            if (group, element) != seq_tag {
                at = if length == 0xFFFF_FFFF {
                    skip_undefined_value(data, at + head)
                } else {
                    at + head + length
                };
                continue;
            }
            // The sequence's value is the item stream. Step past the
            // (FFFE,E000) item header, then read the item's own elements.
            let mut item_at = at + head;
            let item_group = u16::from_le_bytes([data[item_at], data[item_at + 1]]);
            let item_element = u16::from_le_bytes([data[item_at + 2], data[item_at + 3]]);
            assert_eq!(
                (item_group, item_element),
                (0xFFFE, 0xE000),
                "the sequence must open with an item"
            );
            item_at += 8; // item tag + its undefined length
            let mut tags = Vec::new();
            while item_at + 8 <= data.len() {
                let g = u16::from_le_bytes([data[item_at], data[item_at + 1]]);
                let e = u16::from_le_bytes([data[item_at + 2], data[item_at + 3]]);
                if g == 0xFFFE {
                    break; // item delimiter: the item's elements end here
                }
                let v = [data[item_at + 4], data[item_at + 5]];
                let (len, h) = if LONG_VR.contains(&&v) {
                    (
                        u32::from_le_bytes([
                            data[item_at + 8],
                            data[item_at + 9],
                            data[item_at + 10],
                            data[item_at + 11],
                        ]) as usize,
                        12,
                    )
                } else {
                    (
                        u16::from_le_bytes([data[item_at + 6], data[item_at + 7]]) as usize,
                        8,
                    )
                };
                tags.push((g, e));
                item_at = if len == 0xFFFF_FFFF {
                    skip_undefined_value(data, item_at + h)
                } else {
                    item_at + h + len
                };
            }
            return tags;
        }
        panic!("sequence {seq_tag:?} not found in the dataset");
    }

    /// PS3.5 7.1 orders elements inside an item as well as at the top level.
    /// (0008,1140) sorts before (0020,000E), so the referenced-image sequence
    /// must be written before the series UID that carries it.
    #[test]
    fn the_referenced_series_item_lists_its_sequence_before_the_series_uid() {
        let bytes = serialize(&identity(), &PrProfile::of(&pr_rule_result(None, None)));
        let tags = first_item_tags(&bytes, (0x0008, 0x1115));
        let image_seq = tags
            .iter()
            .position(|t| *t == (0x0008, 0x1140))
            .expect("the item holds the referenced-image sequence");
        let series_uid = tags
            .iter()
            .position(|t| *t == (0x0020, 0x000E))
            .expect("the item names its series");
        assert!(
            image_seq < series_uid,
            "(0008,1140) must precede (0020,000E) inside the item: {tags:?}"
        );
        assert_eq!(
            tags,
            {
                let mut sorted = tags.clone();
                sorted.sort_unstable();
                sorted
            },
            "every element of the item ascends: {tags:?}"
        );
    }

    /// PS3.5 7.1 also orders the ANNOTATION item: (0070,0001) starts the
    /// annotation sequence, and the Text Object Sequence (0070,0008) precedes
    /// the Graphic Object Sequence (0070,0009).
    #[test]
    fn the_annotation_item_lists_its_text_object_before_its_graphic() {
        let bytes = serialize(&identity(), &PrProfile::of(&pr_rule_result(None, None)));
        let tags = first_item_tags(&bytes, (0x0070, 0x0001));
        assert!(tags.len() > 2, "the annotation item is read: {tags:?}");
        let text = tags
            .iter()
            .position(|t| *t == (0x0070, 0x0008))
            .expect("the item holds the text object sequence");
        let graphic = tags
            .iter()
            .position(|t| *t == (0x0070, 0x0009))
            .expect("the item holds the graphic object sequence");
        assert!(
            text < graphic,
            "(0070,0008) must precede (0070,0009) inside the item: {tags:?}"
        );
        assert_eq!(
            tags,
            {
                let mut sorted = tags.clone();
                sorted.sort_unstable();
                sorted
            },
            "every element of the item ascends: {tags:?}"
        );
    }

    /// The offset just past an undefined-length value that starts at `at`.
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
                _ => return at,
            }
        }
        at
    }

    #[test]
    fn the_top_level_elements_are_in_ascending_tag_order() {
        // PS3.5 7.1: a data set is an ordered list in ascending tag order,
        // and a reader is entitled to stop at the first descent. The write
        // order groups attributes by MODULE, so a group can land late by
        // accident - the presentation's own 0070 group is the one that did.
        let bytes = serialize(&identity(), &PrProfile::of(&pr_rule_result(None, None)));
        let tags = top_level_tags(&bytes);
        assert!(
            tags.len() > 20,
            "the walk reached the end of the dataset: {} tags",
            tags.len()
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
        // (2050,0020) Presentation LUT Sequence is the last top-level row,
        // which proves the walk went past the nested sequences rather than
        // stopping at the first undefined-length value it met.
        assert_eq!(tags.last(), Some(&(0x2050, 0x0020)));
    }

    /// The VR of the first occurrence of a tag.
    fn vr_at(bytes: &[u8], group: u16, element: u16) -> Option<[u8; 2]> {
        let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
        let at = bytes
            .windows(4)
            .position(|window| window == needle.as_slice())?;
        Some([bytes[at + 4], bytes[at + 5]])
    }

    fn profile(series_description: Option<&str>) -> PrProfile {
        PrProfile {
            color: None,
            series_description: series_description.map(str::to_string),
            label: LabelProfile::default(),
        }
    }

    /// A pr result, for the profile constructor the tests share.
    fn pr_rule_result(color: Option<&str>, series_description: Option<&str>) -> RuleResult {
        RuleResult {
            kind: crate::conf::ResultKind::Pr,
            color: color.map(str::to_string),
            profile_version: 1,
            series_description: series_description.map(str::to_string),
            label: LabelProfile::default(),
            sr: None,
            measurements: None,
        }
    }

    /// A box becomes the closed five-point POLYLINE GSPS requires.
    #[test]
    fn a_box_is_a_closed_five_point_polyline() {
        let line = Box2D {
            x1: 10.0,
            y1: 20.0,
            x2: 30.0,
            y2: 40.0,
        }
        .as_polyline();
        assert_eq!(line.len(), 10, "five points, two coordinates each");
        assert_eq!(&line[..2], &line[8..], "first point equals the last");
        assert_eq!(line[0], 10.0);
        assert_eq!(line[1], 20.0);
        assert_eq!(line[2], 30.0);
        assert_eq!(line[3], 20.0);
    }

    #[test]
    fn the_output_is_a_part10_file() {
        let bytes = serialize(
            &identity(),
            &PrProfile {
                color: Some("#FF3B30".to_string()),
                series_description: None,
                label: LabelProfile::default(),
            },
        );
        assert!(bytes.len() > 132);
        assert_eq!(&bytes[128..132], b"DICM", "128-byte preamble + DICM");
        assert_eq!(&bytes[132..136], &[0x02, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn the_file_meta_names_the_gsps_sop_class_and_explicit_vr() {
        let bytes = serialize(&identity(), &PrProfile::of(&pr_rule_result(None, None)));
        let text = String::from_utf8_lossy(&bytes[..400]);
        assert!(
            text.contains(GSPS_SOP_CLASS_UID),
            "meta carries the GSPS SOP class"
        );
        assert!(
            text.contains(EXPLICIT_VR_LE),
            "meta declares explicit VR little endian"
        );
        assert!(text.contains("1.2.3.4.5"), "meta carries the instance UID");
    }

    #[test]
    fn the_object_carries_both_sop_classes() {
        // The retired class is the primary one (Weasis's supported set,
        // the frozen reference); the current unified class rides along as
        // (0008,001B) Original Specialized SOP Class UID for modern
        // readers.
        let bytes = serialize(&identity(), &profile(None));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains(GSPS_SOP_CLASS_UID), "retired primary class");
        assert!(
            text.contains(CURRENT_GSPS_SOP_CLASS_UID),
            "current class as the original specialized class"
        );
        assert!(
            bytes.windows(4).any(|w| w == [0x08, 0x00, 0x1B, 0x00]),
            "(0008,001B) Original Specialized SOP Class UID is present"
        );
    }

    #[test]
    fn the_polyline_and_its_data_are_present() {
        let bytes = serialize(
            &identity_with(vec![detection("Nodule", 11.0, 22.0, 33.0, 44.0)]),
            &profile(None),
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("POLYLINE"), "graphic type is POLYLINE");
        assert!(
            !text.contains("RECTANGLE"),
            "GSPS has no RECTANGLE graphic type"
        );
        // The coordinates are the detection's, unchanged.
        for value in [11.0f32, 22.0, 33.0, 44.0] {
            let needle = value.to_le_bytes();
            assert!(
                bytes.windows(4).any(|w| w == needle),
                "coordinate {value} must appear in GraphicData"
            );
        }
    }

    #[test]
    fn the_reference_sequence_names_the_source() {
        let bytes = serialize(&identity(), &profile(None));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("1.2.3.4.8"), "referenced source series");
        assert!(text.contains("1.2.3.4.9"), "referenced source instance");
    }

    /// A box becomes the label's explicit placement: the anchor and the
    /// label box corner pair are all written, or Weasis glues the text to
    /// the box's corner (its anchor-only fallback is anchor+(20,50)).
    fn floats_present(bytes: &[u8], values: &[f32]) {
        for value in values {
            let needle = value.to_le_bytes();
            assert!(
                bytes.windows(4).any(|w| w == needle),
                "float {value} must appear in the text object"
            );
        }
    }

    #[test]
    fn the_label_sits_in_an_explicit_box_above_the_anchor() {
        let bytes = serialize(
            &identity_with(vec![detection("Nodule", 100.0, 200.0, 300.0, 400.0)]),
            &profile(None),
        );
        // Defaults: anchor on the top-left corner; the label box's center is
        // 110 px above it, its X span is the frozen reference's
        // anchor-200 .. anchor+80, and its half height is 60.
        floats_present(
            &bytes,
            &[
                100.0, 200.0, // the anchor
                -100.0, 30.0, // the top-left corner
                180.0, 150.0, // the bottom-right corner
            ],
        );
        // The corners are the RETIRED tags (0070,0010)/(0070,0011): dcm4che's
        // dictionary, which Weasis reads, names the retired pair, so the
        // current pair (0070,0007)/(0070,0008) would leave the text with no
        // box and glued to the corner.
        for tag in [
            [0x70, 0x00, 0x10, 0x00],
            [0x70, 0x00, 0x11, 0x00],
            [0x70, 0x00, 0x12, 0x00],
        ] {
            assert!(
                bytes.windows(4).any(|w| w == tag),
                "the retired placement tags must be present: {tag:02x?}"
            );
        }
    }

    #[test]
    fn the_anchor_moves_to_the_center_when_the_profile_says_so() {
        let mut prof = profile(None);
        prof.label = LabelProfile {
            anchor: LabelAnchor::Center,
            ..LabelProfile::default()
        };
        let bytes = serialize(
            &identity_with(vec![detection("Nodule", 100.0, 200.0, 300.0, 400.0)]),
            &prof,
        );
        // Anchor at the box center (200,300); label center 110 px above,
        // corners (200-200, 300-110-60) and (200+80, 300-110+60).
        floats_present(
            &bytes,
            &[
                200.0, 300.0, // the center anchor
                0.0, 130.0, // the top-left corner
                280.0, 250.0, // the bottom-right corner
            ],
        );
    }

    #[test]
    fn the_offset_moves_the_label_box_vertically() {
        let mut prof = profile(None);
        prof.label = LabelProfile {
            anchor: LabelAnchor::TopLeft,
            offset_y_px: -50,
            ..LabelProfile::default()
        };
        let bytes = serialize(
            &identity_with(vec![detection("Nodule", 100.0, 200.0, 300.0, 400.0)]),
            &prof,
        );
        // Label center at 200-50=150, so the corners sit at y=90 and y=210.
        floats_present(&bytes, &[90.0, 210.0]);
    }

    #[test]
    fn the_x_deltas_shift_the_label_box_horizontally() {
        let mut prof = profile(None);
        prof.label = LabelProfile {
            anchor: LabelAnchor::TopLeft,
            offset_y_px: -130,
            offset_x_left_px: -120,
            offset_x_right_px: 60,
        };
        let bytes = serialize(
            &identity_with(vec![detection("Nodule", 100.0, 200.0, 300.0, 400.0)]),
            &prof,
        );
        // Corners at (100-120, 70-60) and (100+60, 70+60).
        floats_present(&bytes, &[-20.0, 10.0, 160.0, 130.0]);
    }

    #[test]
    fn no_ai_disclaimer_is_written_into_the_object() {
        // Clinfer is transport and the model is a third party's, so the
        // disclaimer obligation is the vendor's (fr-03 section 7). The
        // object must carry none, and the viewer's own NOT FOR DIAGNOSTIC
        // USE banner is the deployment's answer. This test exists so the
        // text cannot creep back in.
        let bytes = serialize(
            &identity(),
            &PrProfile {
                color: None,
                series_description: Some("AI Boxes".to_string()),
                label: LabelProfile::default(),
            },
        );
        let text = String::from_utf8_lossy(&bytes).to_lowercase();
        for phrase in [
            "not a diagnosis",
            "диагноз",
            "disclaimer",
            "for research",
            "not for clinical",
        ] {
            assert!(
                !text.contains(phrase),
                "the object must carry no disclaimer, found {phrase:?}"
            );
        }
    }

    #[test]
    fn a_hex_colour_becomes_the_recommended_display_triple() {
        assert_eq!(parse_rgb(Some("#FF3B30")), Some([255, 59, 48]));
        assert_eq!(parse_rgb(Some("#FF3B30FF")), Some([255, 59, 48]));
        assert_eq!(parse_rgb(Some("FF3B30")), None, "a colour needs its #");
        assert_eq!(parse_rgb(Some("#FFF")), None, "short form is not DICOM");
        assert_eq!(parse_rgb(None), None);
    }

    #[test]
    fn every_detection_gets_its_own_annotation_item() {
        let one = serialize(
            &identity_with(vec![detection("A", 1.0, 1.0, 2.0, 2.0)]),
            &profile(None),
        );
        let two = serialize(
            &identity_with(vec![
                detection("A", 1.0, 1.0, 2.0, 2.0),
                detection("B", 3.0, 3.0, 4.0, 4.0),
            ]),
            &profile(None),
        );

        let text = String::from_utf8_lossy(&two);
        assert!(text.contains("A 50%"), "the first label is present");
        assert!(text.contains("B 50%"), "the second label is present");

        // The item-start marker is (FFFE,E000) little endian. A second
        // detection adds exactly one annotation item, its reference item,
        // one graphic object item, and one text object item (PS3.3 Table
        // C.10-5 plus the annotation-level C.7.6.1 reference): four
        // starts, so the structure scales with the boxes and nothing else
        // does. Pinning the number keeps a future edit from quietly
        // dropping one of the four.
        let starts = |bytes: &[u8]| {
            bytes
                .windows(4)
                .filter(|w| *w == [0xFE, 0xFF, 0x00, 0xE0])
                .count()
        };
        assert_eq!(
            starts(&two) - starts(&one),
            4,
            "one more detection adds an annotation, its reference, a graphic object and a text object"
        );
        // And the graphic half really is present, not just the item count.
        // The FIXED items are the series-level pair, the graphic layer, the
        // displayed area, and one item per code sequence the identity
        // carries (AIW-I Appendix V maps three into this object). Every
        // annotation contributes the same four on top of those, so the
        // remainder is what proves no annotation is short an item.
        let fixed = 4 + code_sequence_item_count(&identity());
        assert_eq!(
            (two.windows(4)
                .filter(|w| *w == [0xFE, 0xFF, 0x00, 0xE0])
                .count()
                - fixed)
                % 4,
            0,
            "every annotation contributes the same four items"
        );
    }

    /// How many item-start markers the identity's code sequences add.
    ///
    /// Derived from the identity rather than hard-coded, so the fixed-item
    /// count above cannot drift silently when a mapping is added or a
    /// fixture gains a code.
    fn code_sequence_item_count(identity: &PrIdentity) -> usize {
        [
            identity.scheduled_protocol_code.is_some(),
            identity.requested_procedure_code.is_some(),
            identity.reason_for_requested_procedure_code.is_some(),
        ]
        .iter()
        .filter(|present| **present)
        .count()
    }

    #[test]
    fn each_annotation_carries_its_own_reference_with_the_sop_class() {
        let bytes = serialize(&identity(), &profile(None));
        // (0008,1140) appears twice: once at series level (the own
        // viewer's scope) and once per annotation (Weasis reads only the
        // annotation-level one).
        let refs = bytes
            .windows(4)
            .filter(|w| *w == [0x08, 0x00, 0x40, 0x11])
            .count();
        assert_eq!(refs, 2, "series-level and annotation-level references");
        // Each reference item carries the class UID (0008,1150), Type 1.
        let classes = bytes
            .windows(4)
            .filter(|w| *w == [0x08, 0x00, 0x50, 0x11])
            .count();
        assert_eq!(classes, 2, "one class UID per reference item");
        assert!(
            bytes.windows(10).any(|w| w == b"1.2.3.4.10"),
            "the source SOP class UID is written"
        );
    }

    #[test]
    fn a_multi_frame_reference_names_its_frame() {
        let mut identity = identity_with(vec![detection("A", 1.0, 2.0, 3.0, 4.0)]);
        identity.referenced[0].images[0].frame_number = Some(7);
        let bytes = serialize(&identity, &profile(None));
        // (0008,1160) appears at BOTH reference sites (series-level and
        // annotation-level), value 7: without it the presentation state
        // would apply to every frame of the multi-frame SOP.
        let mut values: Vec<String> = Vec::new();
        let mut from = 0;
        while let Some(at) = bytes[from..]
            .windows(4)
            .position(|window| window == [0x08, 0x00, 0x60, 0x11])
        {
            let at = from + at;
            from = at + 4;
            let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
            values.push(
                String::from_utf8_lossy(&bytes[at + 8..at + 8 + length])
                    .trim()
                    .to_string(),
            );
        }
        assert_eq!(
            values,
            ["7", "7"],
            "series-level and annotation-level references both name the frame"
        );
        // A single-frame source needs no frame number: the element stays
        // absent rather than claiming a frame the instance does not have.
        let plain = serialize(
            &identity_with(vec![detection("A", 1.0, 2.0, 3.0, 4.0)]),
            &profile(None),
        );
        assert!(
            !plain.windows(4).any(|w| w == [0x08, 0x00, 0x60, 0x11]),
            "no frame number for a single-frame image"
        );
    }

    #[test]
    fn every_image_carries_its_own_annotation_scope() {
        let mut identity = identity_with(vec![detection("A", 1.0, 2.0, 3.0, 4.0)]);
        identity.referenced[0].images.push(ReferencedImage {
            sop_class_uid: "1.2.3.4.10".to_string(),
            sop_instance_uid: "1.2.3.4.11".to_string(),
            frame_number: None,
            detections: vec![detection("B", 5.0, 6.0, 7.0, 8.0)],
            contours: Vec::new(),
        });
        let bytes = serialize(&identity, &profile(None));
        // One series-level (0008,1140) plus one annotation-level per image.
        let refs = bytes
            .windows(4)
            .filter(|w| *w == [0x08, 0x00, 0x40, 0x11])
            .count();
        assert_eq!(refs, 3, "series-level plus one annotation-level per image");
        // Each image's UID appears in its own series-level and
        // annotation-level reference items - twice each.
        for (uid, why) in [
            (&b"1.2.3.4.9"[..], "first image"),
            (&b"1.2.3.4.11"[..], "second image"),
        ] {
            assert_eq!(
                bytes.windows(uid.len()).filter(|w| *w == uid).count(),
                2,
                "{why} is referenced at both levels"
            );
        }
        // Both labels are present: the boxes of both images live in the
        // one PR.
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("A 50%"), "first image's label");
        assert!(text.contains("B 50%"), "second image's label");
    }

    #[test]
    fn the_mandatory_gsps_modules_are_present() {
        let bytes = serialize(&identity(), &profile(None));
        let text = String::from_utf8_lossy(&bytes);
        // Displayed Area (C.10.4) - SCALE TO FIT with a corner beyond the
        // 300x400 source image.
        assert!(text.contains("SCALE TO FIT"), "displayed area size mode");
        assert!(
            bytes.windows(4).any(|w| w == [0x70, 0x00, 0x02, 0x01]),
            "(0070,0102) DisplayedAreaSelectionSequence is present"
        );
        // Presentation LUT (C.11.6) - MONOCHROME2 stays IDENTITY.
        assert!(
            bytes.windows(4).any(|w| w == [0x50, 0x20, 0x20, 0x00]),
            "(2050,0020) PresentationLUTShape is present"
        );
        assert!(text.contains("IDENTITY"), "the LUT shape value");
        // Presentation State Identification (C.11.10) Type 1/2 stamp and
        // instance number.
        assert!(
            bytes.windows(4).any(|w| w == [0x70, 0x00, 0x82, 0x00]),
            "(0070,0082) PresentationCreationDate"
        );
        assert!(
            bytes.windows(4).any(|w| w == [0x70, 0x00, 0x83, 0x00]),
            "(0070,0083) PresentationCreationTime"
        );
        assert!(
            bytes.windows(4).any(|w| w == [0x20, 0x00, 0x13, 0x00]),
            "(0020,0013) InstanceNumber"
        );
        // Series module Type 2, carried by the frozen Python reference.
        assert!(
            bytes.windows(4).any(|w| w == [0x20, 0x00, 0x11, 0x00]),
            "(0020,0011) SeriesNumber"
        );
        // (0070,0081) Content Description (Type 2, C.11.10) comes from the
        // profile.
        let described = serialize(
            &identity(),
            &PrProfile {
                color: None,
                series_description: Some("AI Boxes".to_string()),
                label: LabelProfile::default(),
            },
        );
        assert!(
            described.windows(4).any(|w| w == [0x70, 0x00, 0x81, 0x00]),
            "(0070,0081) ContentDescription is present"
        );
        // The text object carries a placement (C.10.5.1, 1C), or Weasis
        // drops it silently.
        assert!(
            bytes.windows(4).any(|w| w == [0x70, 0x00, 0x04, 0x00]),
            "(0070,0004) AnchorPointAnnotationUnits"
        );
        assert!(
            bytes.windows(4).any(|w| w == [0x70, 0x00, 0x14, 0x00]),
            "(0070,0014) AnchorPoint"
        );
    }

    /// B-1: the Displayed Area item lives under (0070,005A) with the
    /// module's own attributes, and the tags it used to be written under
    /// are gone.
    #[test]
    fn the_displayed_area_module_is_written_under_its_own_tags() {
        let bytes = serialize(&identity(), &profile(None));

        assert!(
            tag_at(&bytes, 0x0070, 0x005A),
            "(0070,005A) DisplayedAreaSelectionSequence is present"
        );
        // (0070,0102) is the aspect ratio INSIDE the item, and the sequence
        // the writer used to put it on is not the item's home.
        assert!(
            tag_at(&bytes, 0x0070, 0x0052),
            "(0070,0052) DisplayedAreaTopLeftHandCorner"
        );
        assert!(
            tag_at(&bytes, 0x0070, 0x0053),
            "(0070,0053) DisplayedAreaBottomRightHandCorner"
        );
        assert!(tag_at(&bytes, 0x0070, 0x0100), "(0070,0100) PixelOrigin");
        assert!(
            tag_at(&bytes, 0x0070, 0x0101),
            "(0070,0101) PresentationPixelSpacing"
        );
        assert!(
            tag_at(&bytes, 0x0070, 0x0102),
            "(0070,0102) PresentationPixelAspectRatio"
        );

        // The tags the module used to be written under: (0070,0012) is
        // BoundingBoxTextHorizontalJustification and appears in the TEXT
        // object legitimately, so it cannot be asserted absent globally;
        // (0070,0013) is not in the 2026c registry at all and must be gone.
        assert!(
            !tag_at(&bytes, 0x0070, 0x0013),
            "(0070,0013) is not a registered attribute and must not be written"
        );

        // The corners are SL, a SHORT VR: the 16-bit length follows the VR
        // directly, with no reserved pair (PS3.5 Table 7.1-2).
        let needle = [0x70, 0x00, 0x52, 0x00];
        let at = bytes
            .windows(4)
            .position(|window| window == needle)
            .expect("(0070,0052)");
        assert_eq!(&bytes[at + 4..at + 6], b"SL", "the corner is SL");
        assert_eq!(
            u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]),
            8,
            "two SL values are eight bytes"
        );
        assert_eq!(
            i32::from_le_bytes([bytes[at + 8], bytes[at + 9], bytes[at + 10], bytes[at + 11]]),
            0,
            "top-left row"
        );
        assert_eq!(
            i32::from_le_bytes([
                bytes[at + 12],
                bytes[at + 13],
                bytes[at + 14],
                bytes[at + 15]
            ]),
            0,
            "top-left column"
        );

        // And the bottom-right corner extends past the 300x400 source.
        let needle = [0x70, 0x00, 0x53, 0x00];
        let at = bytes
            .windows(4)
            .position(|window| window == needle)
            .expect("(0070,0053)");
        assert_eq!(
            i32::from_le_bytes([bytes[at + 8], bytes[at + 9], bytes[at + 10], bytes[at + 11]]),
            (identity().source_cols + 200) as i32,
            "bottom-right row"
        );
        assert_eq!(
            i32::from_le_bytes([
                bytes[at + 12],
                bytes[at + 13],
                bytes[at + 14],
                bytes[at + 15]
            ]),
            (identity().source_rows + 200) as i32,
            "bottom-right column"
        );

        assert!(
            String::from_utf8_lossy(&bytes).contains("SCALE TO FIT"),
            "the size mode"
        );
    }

    /// B-1: the two attributes inside the item carry the VRs the dictionary
    /// gives them, which is what a dictionary-strict parser checks.
    #[test]
    fn the_displayed_area_pixel_attributes_use_the_dictionary_vrs() {
        let bytes = serialize(&identity(), &profile(None));
        assert_eq!(
            vr_at(&bytes, 0x0070, 0x0100),
            Some(*b"CS"),
            "(0070,0100) is CS"
        );
        assert_eq!(
            vr_at(&bytes, 0x0070, 0x0101),
            Some(*b"DS"),
            "(0070,0101) is DS, not IS"
        );
        assert_eq!(
            vr_at(&bytes, 0x0070, 0x0102),
            Some(*b"IS"),
            "(0070,0102) is IS"
        );
    }

    /// B-2: the PR declares its character set, so a copied Cyrillic name is
    /// legal bytes rather than an undeclared repertoire.
    #[test]
    fn the_dataset_declares_utf8_as_its_character_set() {
        let bytes = serialize(&identity(), &profile(None));
        assert_eq!(
            vr_at(&bytes, 0x0008, 0x0005),
            Some(*b"CS"),
            "(0008,0005) SpecificCharacterSet"
        );
        let needle = [0x08, 0x00, 0x05, 0x00];
        let at = bytes
            .windows(4)
            .position(|window| window == needle)
            .expect("(0008,0005)");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        assert_eq!(
            String::from_utf8_lossy(&bytes[at + 8..at + 8 + length]).trim_end(),
            SPECIFIC_CHARACTER_SET,
            "ISO_IR 192"
        );
    }

    #[test]
    fn a_cyrillic_patient_name_is_declared_utf8_and_survives() {
        let name = "Петров^Иван";
        let mut identity = identity();
        identity.patient_name = Some(name.to_string());
        let bytes = serialize(&identity, &profile(None));
        assert!(
            bytes.windows(name.len()).any(|w| w == name.as_bytes()),
            "the Cyrillic name is stored byte for byte"
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains(SPECIFIC_CHARACTER_SET),
            "and the object says which repertoire those bytes are in"
        );
    }

    /// D-4: (0070,0021) NumberOfGraphicPoints is US in the dictionary.
    #[test]
    fn the_number_of_graphic_points_is_a_us_value() {
        let bytes = serialize(&identity(), &profile(None));
        let needle = [0x70, 0x00, 0x21, 0x00];
        let at = bytes
            .windows(4)
            .position(|window| window == needle)
            .expect("(0070,0021)");
        assert_eq!(&bytes[at + 4..at + 6], b"US", "the dictionary VR is US");
        assert_eq!(
            u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]),
            2,
            "one US value is two bytes"
        );
        assert_eq!(
            u16::from_le_bytes([bytes[at + 8], bytes[at + 9]]),
            5,
            "five graphical points, as a binary value"
        );
        // The IS spelling of the same value must be gone: "5" is 0x35.
        assert_ne!(bytes[at + 8], b'5', "the value is not IS text");
    }

    /// D-6: Type-2 attributes are PRESENT with an empty value when the
    /// source study does not carry them. The two attributes whose value
    /// never comes from the study - the character set and the
    /// manufacturer - are excluded and pinned separately.
    #[test]
    fn the_type_2_attributes_are_present_but_empty_without_a_source() {
        let bytes = serialize(&identity_without_study_values(), &profile(None));
        // Every Type-2 attribute the module requires, with the VR to walk.
        for (group, element, vr, why) in [
            (0x0010u16, 0x0010u16, b"PN" as &[u8; 2], "PatientName"),
            (0x0010, 0x0020, b"LO", "PatientID"),
            (0x0010, 0x0030, b"DA", "PatientBirthDate"),
            (0x0010, 0x0040, b"CS", "PatientSex"),
            (0x0008, 0x0050, b"SH", "AccessionNumber"),
            (0x0020, 0x0010, b"SH", "StudyID"),
            (0x0008, 0x0090, b"PN", "ReferringPhysicianName"),
            (0x0070, 0x0081, b"LO", "ContentDescription"),
        ] {
            let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
            let at = bytes
                .windows(4)
                .position(|window| window == needle.as_slice())
                .unwrap_or_else(|| panic!("({group:04X},{element:04X}) {why} is absent"));
            assert_eq!(
                &bytes[at + 4..at + 6],
                vr,
                "({group:04X},{element:04X}) {why} keeps its dictionary VR"
            );
            assert_eq!(
                u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]),
                0,
                "({group:04X},{element:04X}) {why} is present with a zero length"
            );
        }
        // The two Type-2 attributes that do NOT come from the study carry
        // their own fixed values even with an empty source.
        for (group, element, value, why) in [
            (0x0008u16, 0x0005u16, "ISO_IR 192", "SpecificCharacterSet"),
            (0x0008, 0x0070, "clinfer", "Manufacturer"),
        ] {
            let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
            let at = bytes
                .windows(4)
                .position(|window| window == needle.as_slice())
                .unwrap_or_else(|| panic!("({group:04X},{element:04X}) {why} is absent"));
            let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
            assert_eq!(
                String::from_utf8_lossy(&bytes[at + 8..at + 8 + length]).trim_end(),
                value,
                "({group:04X},{element:04X}) {why} is always populated"
            );
        }
    }

    #[test]
    fn the_type_2_attributes_carry_the_source_values_when_there_are_any() {
        let bytes = serialize(&identity(), &profile(Some("AI Boxes")));
        let text = String::from_utf8_lossy(&bytes);
        for value in [
            "Fixture^PR",
            "PR-FIXTURE",
            "19700101",
            "STUDY-1",
            "ACC-1",
            "REFERRER^A",
            "clinfer",
            "AI Boxes",
            "Fixture Station",
        ] {
            assert!(
                text.contains(value),
                "the copied value {value:?} is present"
            );
        }
    }

    #[test]
    fn the_station_name_comes_from_the_workitem_and_is_omitted_without_one() {
        // AIW-I Appendix V Table V-1 Note 4: station name is mapped FROM
        // the workitem's (0040,4028). Type 3, so there is no fallback -
        // a workitem that carries no such sequence leaves it out rather
        // than naming the station that wrote the overlay.
        let bytes = serialize(&identity(), &profile(None));
        assert!(tag_at(&bytes, 0x0008, 0x1010), "(0008,1010) StationName");

        let mut without = identity();
        without.station_name = None;
        let bytes = serialize(&without, &profile(None));
        assert!(
            !tag_at(&bytes, 0x0008, 0x1010),
            "no (0040,4028) means no (0008,1010)"
        );
    }

    /// T1/T2/T3: the tail sprint's mappings, per AIW-I Appendix V Table V-1.
    #[test]
    fn the_appendix_v_requestor_identity_is_copied_when_the_workitem_states_it() {
        let bytes = serialize(&identity(), &profile(None));
        // (0010,0021) Issuer of Patient ID ("Issuer of Patient ID Macro").
        assert!(tag_at(&bytes, 0x0010, 0x0021), "(0010,0021) issuer");
        // (0040,0008) Scheduled Protocol Code, (0040,1001) Requested
        // Procedure ID, (0040,1002) Reason, (0040,100A) Reason Code,
        // (0032,1060) Description and (0032,1064) Requested Procedure Code.
        for (group, element) in [
            (0x0040, 0x0008),
            (0x0040, 0x1001),
            (0x0040, 0x1002),
            (0x0040, 0x100A),
            (0x0032, 0x1060),
            (0x0032, 0x1064),
        ] {
            assert!(
                tag_at(&bytes, group, element),
                "({group:04X},{element:04X}) is present"
            );
        }
        let text = String::from_utf8_lossy(&bytes);
        for value in [
            "SITE-A",
            "CAD",
            "RP-1",
            "Screening",
            "REASON-1",
            "Chest CT",
            "RP-CHEST",
        ] {
            assert!(text.contains(value), "{value} is carried");
        }
    }

    #[test]
    fn a_silent_workitem_writes_no_issuer_and_no_requested_procedure_rows() {
        // The zero-length form is a STATEMENT ("no issuer at this site"),
        // while an absent one is silence; the writer must distinguish them
        // and must not invent the requested-procedure rows.
        let mut absent = identity();
        absent.patient_id_issuer = None;
        absent.scheduled_protocol_code = None;
        absent.requested_procedure_id = None;
        absent.requested_procedure_description = None;
        absent.requested_procedure_code = None;
        absent.reason_for_requested_procedure = None;
        absent.reason_for_requested_procedure_code = None;
        let bytes = serialize(&absent, &profile(None));
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
                !tag_at(&bytes, group, element),
                "({group:04X},{element:04X}) is omitted when the workitem is silent"
            );
        }

        // An explicit empty issuer, by contrast, IS written.
        let mut empty = identity();
        empty.patient_id_issuer = Some(String::new());
        let bytes = serialize(&empty, &profile(None));
        assert!(
            tag_at(&bytes, 0x0010, 0x0021),
            "an explicit empty issuer is still an element"
        );
    }

    /// D-11: (0002,0013) names the crate AND its version, truncated on a
    /// character boundary to the 16 characters SH allows.
    #[test]
    fn the_file_meta_stamps_the_build_like_the_report_does() {
        let bytes = serialize(&identity(), &profile(None));
        let needle = [0x02, 0x00, 0x13, 0x00];
        let at = bytes
            .windows(4)
            .position(|window| window == needle)
            .expect("(0002,0013)");
        assert_eq!(&bytes[at + 4..at + 6], b"SH");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        let name = String::from_utf8_lossy(&bytes[at + 8..at + 8 + length])
            .trim_end()
            .to_string();

        assert_eq!(
            name,
            crate::sr::implementation_version_name(),
            "the PR and the SR stamp the same build"
        );
        assert!(name.starts_with("clinfer "), "the crate is named: {name:?}");
        assert!(name.len() <= 16, "SH holds 16 characters at most: {name:?}");
        assert_ne!(name, "CLINFER", "the bare name carried no version");
    }

    #[test]
    fn the_implementation_class_uid_matches_the_report() {
        let bytes = serialize(&identity(), &profile(None));
        let needle = [0x02, 0x00, 0x12, 0x00];
        let at = bytes
            .windows(4)
            .position(|window| window == needle)
            .expect("(0002,0012)");
        let length = u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]) as usize;
        let uid = String::from_utf8_lossy(&bytes[at + 8..at + 8 + length])
            .trim_end_matches(['\0', ' '])
            .to_string();
        assert_eq!(uid, crate::sr::IMPLEMENTATION_CLASS_UID);
    }

    #[test]
    fn a_date_and_time_are_emitted_in_the_dicom_formats() {
        let (date, time) = now_stamp();
        assert_eq!(date.len(), 8, "DA is YYYYMMDD");
        assert!(date.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(time.len(), 6, "TM is HHMMSS");
        assert!(time.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn long_vrs_use_the_32_bit_length_form() {
        // The complete long set from PS3.5 Table 7.1-1, spelled out rather
        // than sampled, so a VR that belongs here cannot be left out.
        for vr in [
            b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"UC", b"UN", b"UR", b"UT",
        ] {
            assert!(
                is_long_vr(vr),
                "{} is a long VR",
                String::from_utf8_lossy(vr)
            );
        }
    }

    #[test]
    fn ul_is_a_short_vr_not_a_long_one() {
        // PS3.5 Table 7.1-2 lists UL among the VRs whose length field stays
        // 16-bit. It is the VR of (0002,0000), the meta group length, so
        // moving it to the long set desynchronises the first element every
        // parser reads. This is the mistake this test exists to prevent.
        assert!(!is_long_vr(b"UL"), "UL is a short VR (PS3.5 Table 7.1-2)");
        for vr in [
            b"AE", b"AS", b"AT", b"CS", b"DA", b"DS", b"DT", b"FL", b"FD", b"IS", b"LO", b"LT",
            b"PN", b"SH", b"SL", b"SS", b"ST", b"TM", b"UI", b"UL", b"US",
        ] {
            assert!(
                !is_long_vr(vr),
                "{} is a short VR (PS3.5 Table 7.1-2)",
                String::from_utf8_lossy(vr)
            );
        }
    }

    #[test]
    fn the_meta_group_framing_is_what_a_parser_seeks_by() {
        // The first element in the file, and the one a parser uses to find
        // the dataset. An 8-byte header here makes a parser seek four bytes
        // short and read the dataset from the wrong offset, which is silent
        // in the sense that nothing errors - the attributes just come back
        // as garbage. The bytes are pinned rather than only the predicate.
        let identity = PrIdentity {
            sop_instance_uid: "2.25.1234".to_string(),
            study_uid: "9.9.9".to_string(),
            series_uid: "2.25.5678".to_string(),
            referenced: vec![],
            source_rows: 100,
            source_cols: 80,
            patient_name: None,
            patient_id: None,
            ..identity_without_study_values()
        };
        let profile = PrProfile::of(&pr_rule_result(Some("#FF3B30"), None));
        let bytes = serialize(&identity, &profile);

        assert_eq!(&bytes[128..132], b"DICM");
        assert_eq!(&bytes[132..134], &[0x02, 0x00], "group 0002");
        assert_eq!(&bytes[134..136], &[0x00, 0x00], "element 0000");
        assert_eq!(&bytes[136..138], b"UL", "the VR");
        // 16-bit form: the length follows the VR directly.
        assert_eq!(
            u16::from_le_bytes([bytes[138], bytes[139]]),
            4,
            "a UL length is 16-bit and says four value bytes"
        );

        // The declared group length really does span the meta elements, so
        // a parser seeking by it lands exactly on the dataset.
        let declared = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]);
        assert!(declared > 0, "the group length is not zero");
        let meta_end = 144 + declared as usize;
        assert!(
            meta_end + 8 <= bytes.len(),
            "the meta group ends inside the file: {meta_end}"
        );
        assert_eq!(
            &bytes[meta_end..meta_end + 4],
            &[0x08, 0x00, 0x05, 0x00],
            "the dataset starts with (0008,0005) Specific Character Set"
        );
    }

    #[test]
    fn an_undefined_length_sequence_declares_the_sentinel_once() {
        // (0008,1115) is written with undefined length. The sentinel IS the
        // length field: writing a zero length and then four more bytes puts
        // eight where four belong, and a receiver reads the sequence as
        // empty and then finds an item delimiter where it expected a tag.
        let identity = PrIdentity {
            sop_instance_uid: "2.25.1234".to_string(),
            study_uid: "9.9.9".to_string(),
            series_uid: "2.25.5678".to_string(),
            referenced: vec![ReferencedSeries {
                series_uid: "2.25.99".to_string(),
                images: vec![ReferencedImage {
                    sop_class_uid: "2.25.97".to_string(),
                    sop_instance_uid: "2.25.98".to_string(),
                    frame_number: None,
                    detections: vec![detection("Nodule", 1.0, 2.0, 3.0, 4.0)],
                    contours: Vec::new(),
                }],
            }],
            source_rows: 100,
            source_cols: 80,
            ..identity_without_study_values()
        };
        let profile = PrProfile::of(&pr_rule_result(Some("#FF3B30"), None));
        let bytes = serialize(&identity, &profile);

        // Find (0008,1115) in the dataset.
        let mut at = 128;
        let mut found = None;
        while at + 12 <= bytes.len() {
            if bytes[at..at + 2] == [0x08, 0x00] && bytes[at + 2..at + 4] == [0x15, 0x11] {
                found = Some(at);
                break;
            }
            at += 2;
        }
        let at = found.expect("(0008,1115) is present");
        assert_eq!(&bytes[at + 4..at + 6], b"SQ", "it is a sequence");
        assert_eq!(&bytes[at + 6..at + 8], &[0x00, 0x00], "the reserved bytes");
        assert_eq!(
            u32::from_le_bytes([bytes[at + 8], bytes[at + 9], bytes[at + 10], bytes[at + 11]]),
            0xFFFF_FFFF,
            "the length is the undefined sentinel, not a separate value"
        );
        // And the first thing after the header is an item, not more length.
        assert_eq!(
            &bytes[at + 12..at + 16],
            &[0xFE, 0xFF, 0x00, 0xE0],
            "an item follows the header immediately"
        );
    }

    /// An identity whose single image carries one contour and no boxes.
    fn identity_with_contour(contour: Contour2D) -> PrIdentity {
        PrIdentity {
            referenced: vec![ReferencedSeries {
                series_uid: "2.25.99".to_string(),
                images: vec![ReferencedImage {
                    sop_class_uid: "2.25.97".to_string(),
                    sop_instance_uid: "2.25.98".to_string(),
                    frame_number: None,
                    detections: Vec::new(),
                    contours: vec![contour],
                }],
            }],
            ..identity_without_study_values()
        }
    }

    /// A ring, `[column, row]` per point, first point repeated last.
    fn ring() -> Contour2D {
        Contour2D {
            label: "Left Lung".to_string(),
            points: vec![[10.0, 20.0], [30.0, 20.0], [30.0, 40.0], [10.0, 20.0]],
        }
    }

    /// The text of the dataset, for substring checks.
    fn as_text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn a_contour_is_written_as_a_closed_polyline_with_all_its_points() {
        // GSPS has one outline form: the traced ring goes out as a POLYLINE
        // whose Graphic Data is the ring verbatim, with the point COUNT the
        // ring actually has rather than the box constant of five.
        let contour = ring();
        let bytes = serialize(&identity_with_contour(contour.clone()), &profile(None));
        let text = as_text(&bytes);

        assert!(text.contains("POLYLINE"), "the graphic type is POLYLINE");
        assert!(!text.contains("POLYGON"), "POLYGON is never written");

        // (0070,0021) is the ring's own point count, not five.
        let at = find_tag(&bytes, 0x0070, 0x0021).expect("(0070,0021) is present");
        assert_eq!(&bytes[at + 4..at + 6], b"US", "(0070,0021) is US");
        assert_eq!(
            u16::from_le_bytes([bytes[at + 8], bytes[at + 9]]),
            contour.points.len() as u16,
            "the count is the ring's own length"
        );

        // Every coordinate reaches Graphic Data, in [column, row] order.
        // `floats_present` asserts internally, so it is called for effect.
        for point in &contour.points {
            floats_present(&bytes, &[point[0] as f32, point[1] as f32]);
        }
    }

    #[test]
    fn a_contour_label_carries_no_confidence() {
        // A mask edge has no operating point, so the text object is the
        // label alone: a " NN%" suffix would read as a measurement nobody
        // made.
        let bytes = serialize(&identity_with_contour(ring()), &profile(None));
        let text = as_text(&bytes);
        assert!(text.contains("Left Lung"), "the label is written");
        assert!(
            !text.contains("Left Lung 0%"),
            "a contour gets no confidence suffix"
        );
    }

    #[test]
    fn a_contour_gets_its_own_annotation_and_reference() {
        // The same scope rule a box obeys: one annotation item, its own
        // reference to the source instance, a graphic and a text object.
        let identity = identity_with_contour(ring());
        let with_contour = serialize(&identity, &profile(None));
        let with_boxes = serialize(
            &identity_with(vec![detection("Nodule", 1.0, 2.0, 3.0, 4.0)]),
            &profile(None),
        );
        // One series-level reference plus one annotation-level reference,
        // exactly as a single box produces.
        assert_eq!(
            referenced_instance_count(&with_boxes),
            2,
            "a single box names the source instance twice, by design"
        );
        assert_eq!(
            referenced_instance_count(&with_contour),
            2,
            "a single contour names it twice as well"
        );
    }

    /// How many times the source SOP instance is referenced in a PR.
    fn referenced_instance_count(bytes: &[u8]) -> usize {
        let needle = [0x08u8, 0x00, 0x55, 0x11];
        bytes
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    /// The offset of a tag's VR, if the tag is present.
    fn find_tag(bytes: &[u8], group: u16, element: u16) -> Option<usize> {
        let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
        bytes
            .windows(needle.len())
            .position(|window| window == needle.as_slice())
    }
}
