// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! `result.json`: the strict, versioned shape a processor writes back.
//!
//! Strict means what it means everywhere else in this crate: an unknown key
//! is a named error, not a silently ignored line (the same rule the
//! configuration parser applies). A typo in a script - `"detectionz"` for
//! `"pr"` - must not produce a run that reports success while persisting
//! nothing; the author has to hear about it.
//!
//! Versioned means the document names the protocol it implements. A script
//! written against a future version is refused with both versions in the
//! message, because a mismatch that parses is worse than one that does not.

use crate::json::Json;

/// The protocol name both documents carry.
pub const PROTOCOL_NAME: &str = "clinfer-abi";

/// The only protocol version this build speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// The largest `result.json` accepted when the model declares no budget, in
/// bytes (64 MiB).
///
/// A box list or a report is kilobytes. A STRUCTURE SET is not: every
/// contour carries three coordinates per point, so the document grows with
/// the polygon count, and the previous bound of 16 MiB sat inside the
/// legitimate range rather than above it. Measured on live CT, with the
/// producer still emitting every traced pixel:
///
/// ```text
///   36 frames / 528 contours   16628332 bytes   15.9 MiB   (accepted)
///   53 frames / 521 contours   28106522 bytes   26.8 MiB   (REFUSED)
/// ```
///
/// The refusal is what made a bigger study look broken: the 53-frame run
/// exited 0 with a complete structure set and was cancelled as "produced no
/// result", because this bound turned a size overrun into a result the
/// reader could not use. 64 MiB covers a study several times larger while
/// still bounding a runaway script, which is the only thing this limit is
/// for: the script is operator-configured and shares the configuration's
/// trust domain, so the threat is a loop filling the disk before the
/// timeout notices, not a hostile document.
///
/// This is the DEFAULT, not the rule: the budget is per model (see the
/// configuration's `max_result_bytes`), because the same deployment can hold
/// a detector writing kilobytes and an outliner writing megabytes, and one
/// number for both is a number that is wrong for one of them.
///
/// Raising it is not the fix for a large structure set on its own: the
/// ContourData byte ceiling is, because the encoded object has its own
/// limit that no result size can lift. This bound only has to stop being
/// the thing that fails first for a legitimate input.
pub const DEFAULT_MAX_RESULT_BYTES: u64 = 64 * 1024 * 1024;

/// Anything wrong with a `result.json`, named for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    /// Where in the document, as a dotted path: `pr.boxes[2].x1`.
    pub at: String,
    pub message: String,
}

impl SchemaError {
    fn new(at: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            at: at.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.at.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.at, self.message)
        }
    }
}

impl std::error::Error for SchemaError {}

type Result<T> = std::result::Result<T, SchemaError>;

/// Who produced the result, as the SCRIPT declares it.
///
/// clinfer cannot know any of this - it does not know which model the script
/// loaded, which version of the script is on disk, or what seed it used.
/// Asking the script to state it is the only honest provenance chain: each
/// side stamps what it owns, and clinfer stamps the rest (its own version,
/// the profile version, the study UID, in the manifest).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Producer {
    /// The script path, as configured.
    pub script: String,
    /// The script's own version string; free-form.
    pub script_version: Option<String>,
    /// The model the script used, if it used one.
    pub model: Option<String>,
    pub model_version: Option<String>,
    /// The RNG seed, when the script's output is seeded.
    pub seed: Option<String>,
}

/// One detection box, in the source frame's pixel coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct PrBox {
    pub label: String,
    pub confidence: f64,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    /// Which frame the box was measured on: 1-based, matching the
    /// manifest's `frames[].index`.
    ///
    /// One presentation state is bound to ONE SOP instance (at series and
    /// at annotation level), so a box has to say which image it belongs to
    /// or the overlay cannot be attached to anything. Optional, default 1,
    /// because the common processor is a per-frame detector that returns
    /// boxes for the frame it was handed.
    pub frame: u32,
}

/// A traced outline a processor found, for a presentation state.
///
/// A contour is a CLOSED outline: the last point repeats the first, and the
/// producer is the one who closes it (the ABI keeps the repeated point
/// rather than dropping it, for the same reason [`Region`] does). There is
/// no graphic-type key because there is only one legal outline form - see
/// [`Region`] for why `POLYGON` is not it.
#[derive(Debug, Clone, PartialEq)]
pub struct PrContour {
    /// Free text naming what the outline traces, e.g. `Left Lung`.
    ///
    /// Presentation states have no coded-name requirement for a graphic
    /// object, so this is the label a reader shows. No confidence: a
    /// segmenter's mask edge has no score, and writing one would invent it.
    pub label: String,
    /// The points, each `[column, row]` in source pixels, ring closed.
    pub points: Vec<[f64; 2]>,
    /// Which frame the outline was traced on: 1-based, matching the
    /// manifest's `frames[].index`, default 1.
    pub frame: u32,
}

/// A `pr` block: what a processor found that a presentation state can draw.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PrResult {
    pub boxes: Vec<PrBox>,
    pub contours: Vec<PrContour>,
}

/// An `sr` block: free text for a report.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SrResult {
    pub text: String,
}

/// A `measurements` block: numeric findings for an Enhanced SR.
///
/// Separate from `sr` because a number cannot legally enter a Basic Text SR
/// (NUM is absent from its value-type list, PS3.3 A.35.1.3.1.1): the only
/// conformant home is Enhanced SR with TID 1500, a different SOP class.
/// clinfer is the transport - the values are written exactly as the
/// processor emitted them, with no threshold judgement applied here.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurements {
    /// The unit every value in this block carries.
    ///
    /// Required, never defaulted: `1` (UCUM, "no units") for a fraction in
    /// [0,1] and `%` for a 0..100 scale are both correct for a probability,
    /// and only the processor knows which one it computed.
    pub unit: Code,
    /// One group per finding.
    pub groups: Vec<MeasurementGroup>,
}

/// One finding and the measurements made about it.
///
/// The finding is a whole-image score's subject, so it is required: a
/// measurement whose subject is unknown cannot be interpreted.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasurementGroup {
    /// (121071, DCM, "Finding") - the pathology the scores are about.
    pub finding: Code,
    /// The numeric measurements for this finding.
    pub measurements: Vec<Measurement>,
}

/// One numeric measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// The measured concept. Absent means the block default,
    /// (122157, DCM, "Probability").
    pub concept: Option<Code>,
    /// The measured value, written verbatim.
    pub value: f64,
    /// The class's operating threshold, when the processor reports one.
    ///
    /// A fact about the model, not a decision: a classifier's operating
    /// point is not 0.5, so a reader that wants to know whether the value
    /// cleared the class's threshold needs the number itself. Written only
    /// when the producer supplies it.
    pub threshold: Option<f64>,
    /// The region of the image this measurement is about, when the
    /// processor traced one.
    ///
    /// A whole-image score needs none, which is why it is optional. A
    /// region that IS supplied becomes a SCOORD content item in the same
    /// Measurement Group, so a reader can draw where the number came from.
    pub region: Option<Region>,
}

/// A traced region of the source frame, in PIXEL coordinates.
///
/// The coordinates are the source frame's own pixel grid - the same grid the
/// `seg` block's masks are packed on - and NEVER patient millimetres.
/// clinfer stamps the DICOM geometry from the source instances when it
/// writes the document, so a producer that converted to millimetres here
/// would be duplicating a transformation it cannot do correctly: it never
/// sees the Image Plane attributes, and a producer-side conversion is how
/// the axes get transposed.
///
/// The graphic type is always `POLYLINE`, which is the ONLY way a closed
/// outline is expressed in DICOM. PS3.3 C.18.6.1.2 enumerates the legal
/// (0070,0023) values as POINT, MULTIPOINT, POLYLINE, CIRCLE and ELLIPSE -
/// `POLYGON` is NOT among them, so a producer that emitted it was writing a
/// value no reader is obliged to accept. A closed region is a POLYLINE whose
/// last point repeats the first; where that ring is closed is the producer's
/// obligation, and this ABI keeps the repeated point rather than dropping it.
///
/// POINT, CIRCLE and ELLIPSE are legal but are not representable as a point
/// list, so accepting one here would mean inventing its parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// The points, each `[column, row]` in source pixels.
    ///
    /// `[column, row]` is the same order the `seg`/`rtstruct` ABI uses, and
    /// the order (0070,0022) Graphic Data expects in a SCOORD item whose
    /// referenced image has its default orientation.
    ///
    /// At least three points: a closed outline needs a ring, and a two-point
    /// list is a line, not an area.
    pub points: Vec<[f64; 2]>,
}

/// The concept a measurement carries when its own block names none:
/// (122157, DCM, "Probability").
///
/// A default only in the sense of "the block's subject is a probability";
/// clinfer still never invents a VALUE.
#[must_use]
pub fn default_measurement_concept() -> Code {
    Code {
        code: "122157".to_string(),
        scheme: "DCM".to_string(),
        meaning: "Probability".to_string(),
    }
}

/// A coded concept: a `(code, scheme, meaning)` triple.
///
/// The processor supplies the clinical vocabulary and clinfer copies it. A
/// segmentation without a property category and type cannot be interpreted
/// by a reader, and inventing one here would be clinfer making a clinical
/// claim - so the triple is required wherever it appears and clinfer never
/// substitutes a default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    pub code: String,
    pub scheme: String,
    pub meaning: String,
}

/// One declared segment: what it is, and how to draw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub label: String,
    /// Segmented Property Category, (0062,0003).
    pub category: Code,
    /// Segmented Property Type, (0062,000F).
    pub property_type: Code,
    /// Recommended Display CIELab Value, (0062,000D): three integers. A
    /// recommendation the reader may ignore, which is why it is optional.
    pub color: Option<[u16; 3]>,
    /// Segment Algorithm Type, (0062,0008): AUTOMATIC or MANUAL.
    pub algorithm: String,
}

/// One mask file for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegFile {
    /// A name inside the workdir, never a path.
    pub file: String,
    /// Which frame the mask belongs to: 1-based, default 1.
    pub frame: u32,
}

/// A `seg` block: per-frame binary masks, one segment set per frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Seg {
    pub segments: Vec<Segment>,
    pub files: Vec<SegFile>,
    /// (0062,0013) Segments Overlap, when the producer knows whether any
    /// pixel belongs to more than one segment. Optional: absent means the
    /// attribute is omitted, which tells a reader to check every pixel.
    pub overlap: Option<String>,
}

/// One derived output file, with the geometry the processor declares.
///
/// The geometry is DECLARED rather than required to equal the source: DICOM
/// allows any size, and the validation's job is catching garbage, not
/// forbidding upscale, crop or downscale. An omitted field means the source
/// geometry for that axis.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DerivedFile {
    pub file: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bits: Option<u16>,
}

/// A `derived` block: replacement image frames, one per input frame.
///
/// The `files` are names inside the workdir, not paths: the workdir is the
/// unit of work and a result that reaches outside it would be a result
/// clinfer cannot reason about. One entry per input frame, in frame order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Derived {
    pub files: Vec<DerivedFile>,
}

/// An `stl` block: one binary surface mesh for the whole study.
///
/// ONE file, not a list. A.85.1 stores a single encapsulated document, so a
/// study has at most one surface object per processor run; a list here
/// would be a schema that promises something the encoder cannot express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stl {
    /// A name inside the workdir, never a path.
    pub file: String,
}

/// One closed polygon contour, in the source frame's pixel coordinates.
///
/// The points are `(column, row)` pairs in the SAME pixel space the PR
/// writer's boxes use, keyed by the same 1-based frame index. That keying
/// is deliberate: `PrBox::frame` and `SegFile::frame` already speak it, and
/// a third spelling would be a third thing to keep in agreement.
///
/// Patient millimetres are NOT accepted here. Converting pixel to patient
/// needs `(0020,0032)`, `(0020,0037)` and `(0028,0030)` per frame, none of
/// which the workdir manifest carries - a processor asked for millimetres
/// would have to fetch every instance a second time and re-implement the
/// DICOM geometry rule in a language clinfer does not control. clinfer
/// already holds all three per frame, so the conversion happens here, once,
/// against Equation C.7.6.2.1-1.
#[derive(Debug, Clone, PartialEq)]
pub struct RtContour {
    /// Which frame the contour was traced on: 1-based, matching the
    /// manifest's `frames[].index`. Default 1.
    pub frame: u32,
    /// The closed polygon, `(column, row)` per point. The first point is
    /// not repeated as the last: C.8.8.6.1 says the last point is connected
    /// to the first. At least three points, so the polygon has an area.
    pub points: Vec<[f64; 2]>,
}

/// One region of interest: what it is, how to draw it, and its contours.
#[derive(Debug, Clone, PartialEq)]
pub struct RtRoi {
    /// (3006,0026) ROI Name.
    pub name: String,
    /// (3006,0028) ROI Description. Omitted when absent.
    pub description: Option<String>,
    /// (3006,002A) ROI Display Color: three integers, each 0..=255.
    pub color: [u8; 3],
    /// (3006,0036) ROI Generation Algorithm, one of `AUTOMATIC`,
    /// `SEMIAUTOMATIC`, `MANUAL`. Defaults to `AUTOMATIC`.
    pub generation_algorithm: String,
    /// (3006,00A4) RT ROI Interpreted Type: a Defined Term of C.8.8.8.
    pub interpreted_type: String,
    /// The contours that make up this ROI, in the order the model supplied
    /// them. At least one.
    pub contours: Vec<RtContour>,
}

/// An `rtstruct` block: one PS3.3 A.19 structure set for the study.
#[derive(Debug, Clone, PartialEq)]
pub struct RtStruct {
    /// (3006,0002) Structure Set Label: SH, non-empty, length-bounded.
    pub label: String,
    /// (3006,0004) Structure Set Name. Falls back to the rule's series
    /// description when absent.
    pub name: Option<String>,
    /// The ROIs. At least one, because a structure set with none is not a
    /// result.
    pub rois: Vec<RtRoi>,
}

/// The maximum points one polygon may carry.
///
/// A cap is needed because `ContourData` is a DS of three values per point,
/// so an unbounded polygon is an unbounded object. 4096 is a power of two
/// above any real organ boundary at the resolutions clinfer fetches.
///
/// This cap does NOT bound the encoded SIZE, and reading it as if it did is
/// a mistake this comment used to make: "4096 * 3 values stays comfortably
/// inside the 65534-byte boundary" is false, because a value is the TEXT of
/// a coordinate and its width depends on the value. A live CT contour of
/// 3874 points produced 69,342 bytes of `ContourData` - over the 65,534 a
/// DS element can hold - because patient coordinates such as `-342.8` are
/// seven characters each rather than a fixed two. Three values per point
/// means three text fields per point, so the ceiling is a BYTE ceiling
/// (about 2400 points at worst-case width) and it is enforced where the
/// bytes exist: `rtstruct::serialize` refuses a `ContourData` that its own
/// VR cannot carry, rather than writing a wrapped length.
///
/// It is a REFUSAL, not a silent downsample: moving a contour's geometry
/// without telling its author would change a clinical result, whereas a
/// named error says which polygon to fix and where the tolerance is known.
pub const MAX_CONTOUR_POINTS: usize = 4096;

/// The fewest points a polygon may carry.
///
/// A CLOSED_PLANAR contour is a closed shape, so two points are a line and
/// one is a point: three is the first count that encloses an area.
pub const MIN_CONTOUR_POINTS: usize = 3;

/// ROI Generation Algorithm (3006,0036): the Defined Terms of C.8.8.5.
pub const ROI_GENERATION_ALGORITHMS: [&str; 3] = ["AUTOMATIC", "SEMIAUTOMATIC", "MANUAL"];

/// RT ROI Interpreted Type (3006,00A4): the Defined Terms of C.8.8.8.
///
/// The list is closed, so a model that wants to say "tooth" says `ORGAN`
/// and puts the clinical vocabulary in `ROI Name`, where A.19.6 places no
/// vocabulary constraint at all.
pub const RT_ROI_INTERPRETED_TYPES: [&str; 25] = [
    "EXTERNAL",
    "PTV",
    "CTV",
    "GTV",
    "TREATED_VOLUME",
    "IRRAD_VOLUME",
    "OAR",
    "BOLUS",
    "AVOIDANCE",
    "ORGAN",
    "MARKER",
    "REGISTRATION",
    "ISOCENTER",
    "CONTRAST_AGENT",
    "CAVITY",
    "BRACHY_CHANNEL",
    "BRACHY_ACCESSORY",
    "BRACHY_SRC_APP",
    "BRACHY_CHNL_SHLD",
    "SUPPORT",
    "FIXATION",
    "DOSE_REGION",
    "CONTROL",
    "DOSE_MEASUREMENT",
    "DEVICE",
];

/// ROI Generation Algorithm (3006,0036) when the model declares none.
///
/// AUTOMATIC is what the standard calls a computed result, and every
/// processor this ABI ships is one; a manual producer says so explicitly.
pub const DEFAULT_ROI_GENERATION_ALGORITHM: &str = "AUTOMATIC";

/// The longest (3006,0002) Structure Set Label, in bytes.
///
/// SH is a 16-byte value, and a reader decodes it as text, so a candidate
/// longer than that is cut at a UTF-8 character boundary rather than
/// mid-codepoint.
pub const MAX_STRUCTURE_SET_LABEL: usize = 16;

/// A parsed, validated `result.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultFile {
    pub producer: Producer,
    /// `None` when the script produced no boxes.
    pub pr: Option<PrResult>,
    pub sr: Option<SrResult>,
    /// Numeric findings for an Enhanced SR. Separate block, separate SOP
    /// class - see [`Measurements`].
    pub measurements: Option<Measurements>,
    pub derived: Option<Derived>,
    pub seg: Option<Seg>,
    pub stl: Option<Stl>,
    /// `None` when the script produced no structure set.
    pub rtstruct: Option<RtStruct>,
}

impl ResultFile {
    /// Whether the script produced anything at all.
    ///
    /// An empty but valid result is not a success with nothing to show: it
    /// is a cycle that produced nothing, which the caller turns into a
    /// cancellation through the same transition rule the native path uses.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pr.is_none()
            && self.sr.is_none()
            && self.measurements.is_none()
            && self.derived.is_none()
            && self.seg.is_none()
            && self.stl.is_none()
            && self.rtstruct.is_none()
    }

    /// Emit a surface mesh, if an `stl` block is present.
    ///
    /// A block whose `file` is blank names nothing to read, so it is
    /// treated as absent rather than as a file that cannot be opened: the
    /// schema refuses a blank name outright, which makes this predicate the
    /// only place the question is asked.
    #[must_use]
    pub fn has_surface(&self) -> bool {
        self.stl.as_ref().is_some_and(|stl| !stl.file.is_empty())
    }

    /// Emit a segmentation, if a `seg` block is present.
    #[must_use]
    pub fn has_segmentation(&self) -> bool {
        self.seg
            .as_ref()
            .is_some_and(|seg| !seg.segments.is_empty() && !seg.files.is_empty())
    }

    /// Emit a structure set, if an `rtstruct` block is present AND carries
    /// at least one ROI.
    ///
    /// The second half of the predicate is what makes an ROI-less block a
    /// cycle that produced nothing rather than a stored object with an
    /// empty Structure Set ROI Sequence. The schema refuses `"rois": []`
    /// outright, so this is the same two-level guard `has_segmentation`
    /// applies: the parser refuses the nonsensical document, and the
    /// predicate independently decides whether there is an object at all.
    #[must_use]
    pub fn has_rtstruct(&self) -> bool {
        self.rtstruct
            .as_ref()
            .is_some_and(|rtstruct| !rtstruct.rois.is_empty())
    }

    /// Draw at least one graphic, if a `pr` block is present: a box OR a
    /// traced contour. Both travel the same GSPS path (`pr::serialize`), so
    /// the predicate a caller uses to decide whether an overlay exists must
    /// see both - a contours-only block is the segmenter's normal shape.
    #[must_use]
    pub fn has_graphics(&self) -> bool {
        self.pr
            .as_ref()
            .is_some_and(|pr| !pr.boxes.is_empty() || !pr.contours.is_empty())
    }

    /// Replace frames, if a `derived` block is present.
    #[must_use]
    pub fn has_derived(&self) -> bool {
        self.derived
            .as_ref()
            .is_some_and(|derived| !derived.files.is_empty())
    }

    /// The frame numbers a `seg` block declares, checked as the 1..N run.
    ///
    /// Kept as a predicate on the parsed result so a caller can re-assert the
    /// invariant without re-parsing; the parser applies the same rule while
    /// reporting WHICH frames were wrong.
    #[must_use]
    pub fn seg_frames_are_complete(&self) -> bool {
        let Some(seg) = &self.seg else {
            return false;
        };
        frames_form_a_run(&seg.files)
    }

    /// Report text, if an `sr` block carries any.
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.sr
            .as_ref()
            .is_some_and(|sr| !sr.text.trim().is_empty())
    }

    /// Emit numeric findings, if a `measurements` block carries any.
    ///
    /// A block with no groups, or every group empty, carries no number and
    /// so no document: the schema refuses both shapes outright, which makes
    /// this the only place the question is asked - the same two-level guard
    /// `has_segmentation` and `has_rtstruct` apply.
    #[must_use]
    pub fn has_measurements(&self) -> bool {
        self.measurements.as_ref().is_some_and(|block| {
            block
                .groups
                .iter()
                .any(|group| !group.measurements.is_empty())
        })
    }

    /// The largest `frame` any box names, or `None` when there are no boxes.
    ///
    /// The worker needs this BEFORE it walks the frames: a box naming a
    /// frame the study does not have is a contract violation, and finding
    /// that out while holding a half-written series would leave orphaned
    /// overlays behind. One pass, no allocation.
    #[must_use]
    pub fn max_box_frame(&self) -> Option<u32> {
        self.pr.as_ref()?.boxes.iter().map(|b| b.frame).max()
    }

    /// Call `emit` once per box whose `frame` matches `frame`.
    ///
    /// An iterator rather than a collected `Vec` on purpose: the boxes are
    /// the one part of a result whose count is the processor's to choose,
    /// and grouping them by frame must not cost a second copy of every box
    /// (the ap101 rule this path carries). The closure shape also keeps the
    /// caller's borrowing simple - it holds the encoder's identity mutably
    /// while this borrows the result immutably.
    pub fn boxes_on_frame<F>(&self, frame: u32, mut emit: F)
    where
        F: FnMut(&PrBox),
    {
        let Some(pr) = &self.pr else {
            return;
        };
        for entry in &pr.boxes {
            if entry.frame == frame {
                emit(entry);
            }
        }
    }

    /// Every contour the processor traced on one frame, in the order it
    /// emitted them.
    ///
    /// The same per-frame shape as [`Self::boxes_on_frame`], for the same
    /// reason: one presentation state is bound to one SOP instance, so a
    /// contour has to be selected by the frame it belongs to.
    pub fn contours_on_frame<F>(&self, frame: u32, mut emit: F)
    where
        F: FnMut(&PrContour),
    {
        let Some(pr) = &self.pr else {
            return;
        };
        for entry in &pr.contours {
            if entry.frame == frame {
                emit(entry);
            }
        }
    }
}

/// Parse and validate one `result.json`.
///
/// Every failure is a named error carrying the offending path, so a script
/// author can find the line without guessing.
///
/// `max_bytes` is the budget of the model that produced the document, so a
/// refusal names the number that actually applied instead of a build-wide
/// constant the operator never chose. Use [`DEFAULT_MAX_RESULT_BYTES`] where
/// no model is in hand.
pub fn parse(bytes: &[u8], max_bytes: u64) -> Result<ResultFile> {
    if bytes.len() as u64 > max_bytes {
        // The message names the CONFIGURATION KEY, not just the number: an
        // operator who finds this line in the log has to be able to act on
        // it without reading the source, and the only actionable thing here
        // is which knob to raise.
        return Err(SchemaError::new(
            "",
            format!(
                "result.json is {} bytes, over the model's max_result_bytes limit of {max_bytes}",
                bytes.len()
            ),
        ));
    }
    let document =
        crate::json::parse(bytes).map_err(|err| SchemaError::new("", err.to_string()))?;
    let Json::Obj(entries) = &document else {
        return Err(SchemaError::new(
            "",
            format!("the document is {}, not an object", document.type_name()),
        ));
    };

    // Walk the keys ourselves rather than reaching into the document
    // repeatedly: the strictness rule is about the KEY SET, so the keys are
    // what has to be enumerated and checked.
    let mut protocol_seen = false;
    let mut producer: Option<Producer> = None;
    let mut pr: Option<PrResult> = None;
    let mut sr: Option<SrResult> = None;
    let mut measurements: Option<Measurements> = None;
    let mut derived: Option<Derived> = None;
    let mut seg: Option<Seg> = None;
    let mut stl: Option<Stl> = None;
    let mut rtstruct: Option<RtStruct> = None;

    for (key, value) in entries {
        match key.as_str() {
            "protocol" => {
                check_protocol(value)?;
                protocol_seen = true;
            }
            "producer" => producer = Some(parse_producer(value)?),
            "pr" => pr = Some(parse_pr(value)?),
            "sr" => sr = Some(parse_sr(value)?),
            "measurements" => measurements = Some(parse_measurements(value)?),
            "derived" => derived = Some(parse_derived(value)?),
            "seg" => seg = Some(parse_seg(value)?),
            "stl" => stl = Some(parse_stl(value)?),
            "rtstruct" => rtstruct = Some(parse_rtstruct(value)?),
            other => {
                return Err(SchemaError::new(
                    other,
                    "unknown key; the result schema is strict, so a misspelling is an error \
                     rather than a silently ignored line",
                ));
            }
        }
    }

    if !protocol_seen {
        return Err(SchemaError::new(
            "protocol",
            "missing required key; every clinfer-abi document names its protocol and version",
        ));
    }
    let producer = producer.ok_or_else(|| {
        SchemaError::new(
            "producer",
            "missing required key; the script declares what produced the result, because \
             clinfer cannot know which model or script version was used",
        )
    })?;

    Ok(ResultFile {
        producer,
        pr,
        sr,
        measurements,
        derived,
        seg,
        stl,
        rtstruct,
    })
}

/// The `protocol` object, with the version checked against this build.
fn check_protocol(value: &Json) -> Result<()> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "protocol",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut name: Option<&str> = None;
    let mut version: Option<u64> = None;
    for (key, field) in entries {
        match key.as_str() {
            "name" => {
                name = Some(
                    field
                        .as_str()
                        .ok_or_else(|| SchemaError::new("protocol.name", "expected a string"))?,
                );
            }
            "version" => {
                version =
                    Some(field.as_u64().ok_or_else(|| {
                        SchemaError::new("protocol.version", "expected a number")
                    })?);
            }
            other => {
                return Err(SchemaError::new(format!("protocol.{other}"), "unknown key"));
            }
        }
    }
    let name = name.ok_or_else(|| SchemaError::new("protocol.name", "missing required key"))?;
    if name != PROTOCOL_NAME {
        return Err(SchemaError::new(
            "protocol.name",
            format!("expected {PROTOCOL_NAME:?}, found {name:?}"),
        ));
    }
    let version =
        version.ok_or_else(|| SchemaError::new("protocol.version", "missing required key"))?;
    if version != PROTOCOL_VERSION {
        // Both versions in the message: an operator has to know which side
        // to change, and "unsupported version" alone does not say.
        return Err(SchemaError::new(
            "protocol.version",
            format!(
                "the script implements {PROTOCOL_NAME} version {version}, this build speaks \
                 version {PROTOCOL_VERSION}"
            ),
        ));
    }
    Ok(())
}

fn parse_producer(value: &Json) -> Result<Producer> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "producer",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut producer = Producer::default();
    let mut script_seen = false;
    for (key, field) in entries {
        let text = || {
            field
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| SchemaError::new(format!("producer.{key}"), "expected a string"))
        };
        match key.as_str() {
            "script" => {
                producer.script = text()?;
                script_seen = true;
            }
            "script_version" => producer.script_version = Some(text()?),
            "model" => producer.model = Some(text()?),
            "model_version" => producer.model_version = Some(text()?),
            "seed" => producer.seed = Some(text()?),
            other => {
                return Err(SchemaError::new(format!("producer.{other}"), "unknown key"));
            }
        }
    }
    if !script_seen {
        return Err(SchemaError::new(
            "producer.script",
            "missing required key; the script names itself so a stored result can be traced \
             back to the program that produced it",
        ));
    }
    Ok(producer)
}

fn parse_pr(value: &Json) -> Result<PrResult> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "pr",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut boxes: Option<Vec<PrBox>> = None;
    let mut contours: Option<Vec<PrContour>> = None;
    for (key, field) in entries {
        match key.as_str() {
            "boxes" => {
                let Json::Arr(items) = field else {
                    return Err(SchemaError::new(
                        "pr.boxes",
                        format!("expected an array, found {}", field.type_name()),
                    ));
                };
                let mut parsed = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    parsed.push(parse_box(item, index)?);
                }
                boxes = Some(parsed);
            }
            "contours" => {
                let Json::Arr(items) = field else {
                    return Err(SchemaError::new(
                        "pr.contours",
                        format!("expected an array, found {}", field.type_name()),
                    ));
                };
                let mut parsed = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    parsed.push(parse_pr_contour(item, index)?);
                }
                contours = Some(parsed);
            }
            other => return Err(SchemaError::new(format!("pr.{other}"), "unknown key")),
        }
    }
    // A `pr` block must carry SOMETHING: boxes, contours, or both. Each
    // list alone may be absent (a segmenter has no boxes; a detector has
    // no contours), but an entirely empty block is a producer error, the
    // same way an empty `measurements` block is.
    if boxes.is_none() && contours.is_none() {
        return Err(SchemaError::new(
            "pr",
            "the block carries nothing: it needs a boxes list, a contours list, or both",
        ));
    }
    Ok(PrResult {
        boxes: boxes.unwrap_or_default(),
        contours: contours.unwrap_or_default(),
    })
}

fn parse_pr_contour(value: &Json, index: usize) -> Result<PrContour> {
    let at = |field: &str| format!("pr.contours[{index}].{field}");
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            format!("pr.contours[{index}]"),
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut label: Option<String> = None;
    let mut points: Option<Vec<[f64; 2]>> = None;
    let mut frame: Option<u32> = None;
    for (key, field) in entries {
        match key.as_str() {
            "label" => {
                label = Some(
                    field
                        .as_str()
                        .ok_or_else(|| SchemaError::new(at("label"), "expected a string"))?
                        .to_string(),
                );
            }
            "points" => {
                // The same floor a `region` has, and for the same reason: a
                // closed outline needs a ring to enclose an area.
                points = Some(parse_points(field, &at("points"), MIN_CONTOUR_POINTS)?);
            }
            "frame" => frame = Some(one_based(field, &at("frame"))?),
            other => {
                return Err(SchemaError::new(
                    format!("pr.contours[{index}].{other}"),
                    "unknown key",
                ));
            }
        }
    }
    let label = label.ok_or_else(|| SchemaError::new(at("label"), "missing required key"))?;
    let points = points.ok_or_else(|| SchemaError::new(at("points"), "missing required key"))?;
    // Closedness is a DATA contract, not a shape the parser can infer: a
    // three- or four-point ring might be a legitimate open trace the
    // producer mislabelled, and silently closing it would hide that. So the
    // check is exact - first point equal to last - and the error says how to
    // fix it.
    if points.first() != points.last() {
        return Err(SchemaError::new(
            at("points"),
            "this outline is not closed: the last point must repeat the first. \
             A presentation-state contour is a closed POLYLINE, so send the \
             ring with its first point appended",
        ));
    }
    Ok(PrContour {
        label,
        points,
        frame: frame.unwrap_or(1),
    })
}

fn parse_box(value: &Json, index: usize) -> Result<PrBox> {
    let at = |field: &str| format!("pr.boxes[{index}].{field}");
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            format!("pr.boxes[{index}]"),
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut label: Option<String> = None;
    let mut confidence: Option<f64> = None;
    let mut coords: [Option<f64>; 4] = [None; 4];
    let mut frame: Option<u32> = None;
    for (key, field) in entries {
        match key.as_str() {
            "label" => {
                label = Some(
                    field
                        .as_str()
                        .ok_or_else(|| SchemaError::new(at("label"), "expected a string"))?
                        .to_string(),
                );
            }
            "confidence" => confidence = Some(number(field, &at("confidence"))?),
            "x1" => coords[0] = Some(number(field, &at("x1"))?),
            "y1" => coords[1] = Some(number(field, &at("y1"))?),
            "x2" => coords[2] = Some(number(field, &at("x2"))?),
            "y2" => coords[3] = Some(number(field, &at("y2"))?),
            "frame" => {
                // A frame number is a POSITION, so zero and negatives are
                // not "the first frame": they are a processor that lost
                // count, and a named error says so instead of silently
                // attaching the box to frame 1.
                let value = number(field, &at("frame"))?;
                let as_int = u32::try_from(value as i64).unwrap_or(0);
                if as_int == 0 || (value - f64::from(as_int)).abs() > f64::EPSILON {
                    return Err(SchemaError::new(
                        at("frame"),
                        format!("{value} is not a 1-based frame number"),
                    ));
                }
                frame = Some(as_int);
            }
            other => {
                return Err(SchemaError::new(
                    format!("pr.boxes[{index}].{other}"),
                    "unknown key",
                ));
            }
        }
    }
    let label = label.ok_or_else(|| SchemaError::new(at("label"), "missing required key"))?;
    Ok(PrBox {
        label,
        confidence: confidence.unwrap_or(0.0),
        x1: coords[0].ok_or_else(|| SchemaError::new(at("x1"), "missing required key"))?,
        y1: coords[1].ok_or_else(|| SchemaError::new(at("y1"), "missing required key"))?,
        x2: coords[2].ok_or_else(|| SchemaError::new(at("x2"), "missing required key"))?,
        y2: coords[3].ok_or_else(|| SchemaError::new(at("y2"), "missing required key"))?,
        frame: frame.unwrap_or(1),
    })
}

fn parse_sr(value: &Json) -> Result<SrResult> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "sr",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut text: Option<String> = None;
    for (key, field) in entries {
        match key.as_str() {
            "text" => {
                text = Some(
                    field
                        .as_str()
                        .ok_or_else(|| SchemaError::new("sr.text", "expected a string"))?
                        .to_string(),
                );
            }
            other => return Err(SchemaError::new(format!("sr.{other}"), "unknown key")),
        }
    }
    Ok(SrResult {
        text: text.ok_or_else(|| SchemaError::new("sr.text", "missing required key"))?,
    })
}

/// The `measurements` object.
///
/// Strict like every other block: an unknown key is an error, so a
/// misspelled `unit` cannot silently become a measurement with no unit.
fn parse_measurements(value: &Json) -> Result<Measurements> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "measurements",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut unit: Option<Code> = None;
    let mut groups: Option<Vec<MeasurementGroup>> = None;
    for (key, field) in entries {
        match key.as_str() {
            "unit" => unit = Some(parse_code(field, "measurements.unit")?),
            "groups" => {
                groups = Some(parse_measurement_groups(field)?);
            }
            other => {
                return Err(SchemaError::new(
                    format!("measurements.{other}"),
                    "unknown key",
                ));
            }
        }
    }
    let groups = groups.ok_or_else(|| {
        SchemaError::new(
            "measurements.groups",
            "missing required key; a measurements block is a list of findings",
        )
    })?;
    if groups.is_empty() {
        return Err(SchemaError::new(
            "measurements.groups",
            "the list is empty; a block with no finding carries nothing to store",
        ));
    }
    Ok(Measurements {
        unit: unit.ok_or_else(|| {
            SchemaError::new(
                "measurements.unit",
                "missing required key; the unit carries the meaning of every value in the \
                 block - 1 (no units) for a fraction and % for a 0..100 scale are both \
                 correct for a probability, so clinfer copies the unit rather than guessing",
            )
        })?,
        groups,
    })
}

fn parse_measurement_groups(value: &Json) -> Result<Vec<MeasurementGroup>> {
    let Json::Arr(items) = value else {
        return Err(SchemaError::new(
            "measurements.groups",
            format!("expected an array, found {}", value.type_name()),
        ));
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            parse_measurement_group(item, &format!("measurements.groups[{index}]"))
        })
        .collect()
}

fn parse_measurement_group(value: &Json, at: &str) -> Result<MeasurementGroup> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            at,
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut finding: Option<Code> = None;
    let mut measurements: Option<Vec<Measurement>> = None;
    for (key, field) in entries {
        match key.as_str() {
            "finding" => finding = Some(parse_code(field, &format!("{at}.finding"))?),
            "measurements" => {
                measurements = Some(parse_measurements_list(field, at)?);
            }
            other => return Err(SchemaError::new(format!("{at}.{other}"), "unknown key")),
        }
    }
    let measurements = measurements.ok_or_else(|| {
        SchemaError::new(
            format!("{at}.measurements"),
            "missing required key; a finding with no measurement carries no number",
        )
    })?;
    if measurements.is_empty() {
        return Err(SchemaError::new(
            format!("{at}.measurements"),
            "the list is empty; a group with no measurement carries no number",
        ));
    }
    Ok(MeasurementGroup {
        finding: finding.ok_or_else(|| {
            SchemaError::new(
                format!("{at}.finding"),
                "missing required key; a number whose subject is unknown cannot be interpreted",
            )
        })?,
        measurements,
    })
}

fn parse_measurements_list(value: &Json, group_at: &str) -> Result<Vec<Measurement>> {
    let Json::Arr(items) = value else {
        return Err(SchemaError::new(
            format!("{group_at}.measurements"),
            format!("expected an array, found {}", value.type_name()),
        ));
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_measurement(item, &format!("{group_at}.measurements[{index}]")))
        .collect()
}

fn parse_measurement(value: &Json, at: &str) -> Result<Measurement> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            at,
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut concept: Option<Code> = None;
    let mut measured: Option<f64> = None;
    let mut threshold: Option<f64> = None;
    let mut region: Option<Region> = None;
    for (key, field) in entries {
        match key.as_str() {
            "concept" => concept = Some(parse_code(field, &format!("{at}.concept"))?),
            "value" => {
                measured = Some(as_number(field, &format!("{at}.value"))?);
            }
            "threshold" => {
                threshold = Some(as_number(field, &format!("{at}.threshold"))?);
            }
            "region" => region = Some(parse_region(field, &format!("{at}.region"))?),
            other => return Err(SchemaError::new(format!("{at}.{other}"), "unknown key")),
        }
    }
    Ok(Measurement {
        concept,
        value: measured.ok_or_else(|| {
            SchemaError::new(
                format!("{at}.value"),
                "missing required key; clinfer never invents a measured value",
            )
        })?,
        threshold,
        region,
    })
}

/// The `region` object of a measurement.
///
/// Strict like every other block. The two checks that matter beyond the key
/// set are the graphic type (always `POLYLINE`, the only form in which DICOM
/// expresses an outline - see [`Region`]) and the point count (an outline
/// needs at least three points to enclose anything).
fn parse_region(value: &Json, at: &str) -> Result<Region> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            at,
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut typed = false;
    let mut points: Option<Vec<[f64; 2]>> = None;
    for (key, field) in entries {
        match key.as_str() {
            "type" => {
                let Json::Str(text) = field else {
                    return Err(SchemaError::new(
                        format!("{at}.type"),
                        format!("expected a string, found {}", field.type_name()),
                    ));
                };
                // Only POLYLINE is legal here. PS3.3 C.18.6.1.2 enumerates
                // (0070,0023) as POINT, MULTIPOINT, POLYLINE, CIRCLE and
                // ELLIPSE: "POLYGON" is NOT one of them, so it is refused by
                // name with the reason, rather than accepted and written out
                // as a value a strict reader would reject.
                match text.as_str() {
                    "POLYLINE" => typed = true,
                    "POLYGON" => {
                        return Err(SchemaError::new(
                            format!("{at}.type"),
                            "POLYGON is not a legal SCOORD Graphic Type: PS3.3 C.18.6.1.2 \
                             enumerates POINT, MULTIPOINT, POLYLINE, CIRCLE and ELLIPSE. A \
                             closed outline IS a POLYLINE whose last point repeats the first, \
                             so send POLYLINE with the ring closed",
                        ));
                    }
                    other => {
                        return Err(SchemaError::new(
                            format!("{at}.type"),
                            format!(
                                "unknown graphic type {other:?}; this ABI accepts POLYLINE. \
                                 POINT, MULTIPOINT, CIRCLE and ELLIPSE are legal SCOORD \
                                 values but are not point lists, so accepting one here would \
                                 mean inventing its parameters"
                            ),
                        ));
                    }
                }
            }
            "points" => {
                points = Some(parse_points(field, &format!("{at}.points"), 3)?);
            }
            other => return Err(SchemaError::new(format!("{at}.{other}"), "unknown key")),
        }
    }
    if !typed {
        return Err(SchemaError::new(
            format!("{at}.type"),
            "missing required key; a region without a graphic type cannot be read",
        ));
    }
    let points = points.ok_or_else(|| {
        SchemaError::new(
            format!("{at}.points"),
            "missing required key; a region with no points traces nothing",
        )
    })?;
    Ok(Region { points })
}

/// A JSON number as `f64`, accepting either the integer or the fractional
/// form.
///
/// Rejects a non-number explicitly rather than coercing: a value sent as a
/// STRING would otherwise reach the writer and be re-emitted as whatever the
/// string happened to be, which is how a malformed number becomes a stored
/// one. `as_f64` itself refuses an integer too large to be exact, so a
/// silent precision loss cannot pass here either.
fn as_number(value: &Json, at: &str) -> Result<f64> {
    value.as_f64().ok_or_else(|| {
        SchemaError::new(
            at,
            format!("expected a number, found {}", value.type_name()),
        )
    })
}

fn parse_derived(value: &Json) -> Result<Derived> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "derived",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut files: Option<Vec<DerivedFile>> = None;
    for (key, field) in entries {
        match key.as_str() {
            "files" => {
                let Json::Arr(items) = field else {
                    return Err(SchemaError::new(
                        "derived.files",
                        format!("expected an array, found {}", field.type_name()),
                    ));
                };
                let mut parsed = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    parsed.push(parse_derived_file(item, index)?);
                }
                files = Some(parsed);
            }
            other => {
                return Err(SchemaError::new(format!("derived.{other}"), "unknown key"));
            }
        }
    }
    Ok(Derived {
        files: files.ok_or_else(|| SchemaError::new("derived.files", "missing required key"))?,
    })
}

/// One `derived.files` entry: the name, plus any declared geometry.
fn parse_derived_file(value: &Json, index: usize) -> Result<DerivedFile> {
    let at = |field: &str| format!("derived.files[{index}].{field}");
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            format!("derived.files[{index}]"),
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut file: Option<String> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut bits: Option<u16> = None;
    for (key, field) in entries {
        match key.as_str() {
            "file" => {
                let name = field.as_str().ok_or_else(|| {
                    SchemaError::new(
                        at("file"),
                        format!("expected a string, found {}", field.type_name()),
                    )
                })?;
                // A name, not a path: the workdir is the unit of work, and a
                // result that reached outside it would be one clinfer could
                // not bound or clean up.
                if !is_workdir_file_name(name) {
                    return Err(SchemaError::new(
                        at("file"),
                        format!("{name:?} is not a file name inside the workdir"),
                    ));
                }
                file = Some(name.to_string());
            }
            "width" => width = Some(positive(field, &at("width"))?),
            "height" => height = Some(positive(field, &at("height"))?),
            "bits" => {
                let declared = number(field, &at("bits"))?;
                let as_int = u16::try_from(declared as i64).unwrap_or(0);
                if as_int == 0 || (declared - f64::from(as_int)).abs() > f64::EPSILON {
                    return Err(SchemaError::new(
                        at("bits"),
                        format!("{declared} is not a positive whole number of bits"),
                    ));
                }
                bits = Some(as_int);
            }
            other => {
                return Err(SchemaError::new(
                    format!("derived.files[{index}].{other}"),
                    "unknown key",
                ));
            }
        }
    }
    Ok(DerivedFile {
        file: file.ok_or_else(|| SchemaError::new(at("file"), "missing required key"))?,
        width,
        height,
        bits,
    })
}

/// The `seg` object: the declared segments and the mask files.
fn parse_seg(value: &Json) -> Result<Seg> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "seg",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut segments: Option<Vec<Segment>> = None;
    let mut files: Option<Vec<SegFile>> = None;
    let mut overlap: Option<String> = None;
    for (key, field) in entries {
        match key.as_str() {
            "segments" => {
                let Json::Arr(items) = field else {
                    return Err(SchemaError::new(
                        "seg.segments",
                        format!("expected an array, found {}", field.type_name()),
                    ));
                };
                // A segmentation with no segments is not a result: there
                // would be nothing for a mask bit to mean. Caught here
                // rather than silently stored as an empty object.
                if items.is_empty() {
                    return Err(SchemaError::new(
                        "seg.segments",
                        "an empty array; a segmentation declares at least one segment, because \
                         a mask bit with no segment behind it means nothing",
                    ));
                }
                let mut parsed = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    parsed.push(parse_segment(item, index)?);
                }
                segments = Some(parsed);
            }
            "files" => {
                let Json::Arr(items) = field else {
                    return Err(SchemaError::new(
                        "seg.files",
                        format!("expected an array, found {}", field.type_name()),
                    ));
                };
                // An empty mask list is the same defect as an empty segment
                // list: there is nothing to store, so it is named here
                // rather than accepted and later refused as a count
                // mismatch with the study.
                if items.is_empty() {
                    return Err(SchemaError::new(
                        "seg.files",
                        "an empty array; one mask per source image, so a segmentation carries \
                         at least one",
                    ));
                }
                let mut parsed = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    parsed.push(parse_seg_file(item, index)?);
                }
                files = Some(parsed);
            }
            "overlap" => {
                let Json::Str(value) = field else {
                    return Err(SchemaError::new(
                        "seg.overlap",
                        format!("expected a string, found {}", field.type_name()),
                    ));
                };
                if !matches!(value.as_str(), "YES" | "NO" | "UNDEFINED") {
                    return Err(SchemaError::new(
                        "seg.overlap",
                        format!("\"{value}\" is not YES, NO or UNDEFINED (PS3.3 C.8.20.3)"),
                    ));
                }
                overlap = Some(value.clone());
            }
            other => return Err(SchemaError::new(format!("seg.{other}"), "unknown key")),
        }
    }
    let segments =
        segments.ok_or_else(|| SchemaError::new("seg.segments", "missing required key"))?;
    let files = files.ok_or_else(|| SchemaError::new("seg.files", "missing required key"))?;
    // The declared frames must be exactly 1..=N: the masks are one per
    // source image, so a gap or a repeat is a processor that lost count,
    // and a named error beats a stored object with a hole in it.
    let count = files.len();
    // A frame number is a POSITION in a run, so the only legal shape is a
    // permutation of 1..=count. One bool per frame is the check that needs
    // no sorting and scales with the study rather than the square of it.
    // The reported path names the entry that broke the run, so an operator
    // has a line to look at rather than a rule to re-derive.
    if !frames_form_a_run(&files) {
        let declared: Vec<u32> = files.iter().map(|f| f.frame).collect();
        let at = files
            .iter()
            .position(|entry| {
                let index = entry.frame as usize;
                index == 0 || index > count
            })
            .map_or("seg.files[].frame".to_string(), |index| {
                format!("seg.files[{index}].frame")
            });
        return Err(SchemaError::new(
            at,
            format!(
                "the frames {declared:?} are not the run 1..={count}; one mask per source \
                 image, no gaps and no repeats"
            ),
        ));
    }
    Ok(Seg {
        segments,
        files,
        overlap,
    })
}

/// Whether the declared frames are exactly `1..=len`.
///
/// The masks are one per source image, so the only legal shape is that run:
/// a gap means an image with no mask, a repeat means one mask claimed twice.
fn frames_form_a_run(files: &[SegFile]) -> bool {
    let count = files.len();
    if count == 0 {
        return false;
    }
    let mut seen = vec![false; count];
    for entry in files {
        let index = entry.frame as usize;
        if index == 0 || index > count || seen[index - 1] {
            return false;
        }
        seen[index - 1] = true;
    }
    true
}

/// One declared segment.
fn parse_segment(value: &Json, index: usize) -> Result<Segment> {
    let at = |field: &str| format!("seg.segments[{index}].{field}");
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            format!("seg.segments[{index}]"),
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut label: Option<String> = None;
    let mut category: Option<Code> = None;
    let mut property_type: Option<Code> = None;
    let mut color: Option<[u16; 3]> = None;
    let mut algorithm: Option<String> = None;
    for (key, field) in entries {
        match key.as_str() {
            "label" => {
                label = Some(
                    field
                        .as_str()
                        .ok_or_else(|| SchemaError::new(at("label"), "expected a string"))?
                        .to_string(),
                );
            }
            "category" => category = Some(parse_code(field, &at("category"))?),
            "type" => property_type = Some(parse_code(field, &at("type"))?),
            "color" => color = Some(parse_color(field, &at("color"))?),
            "algorithm" => {
                let value = field
                    .as_str()
                    .ok_or_else(|| SchemaError::new(at("algorithm"), "expected a string"))?;
                // (0062,0008) is an enumerated, non-empty CS value.
                if value.trim().is_empty() {
                    return Err(SchemaError::new(
                        at("algorithm"),
                        "empty; omit the key to take the default AUTOMATIC",
                    ));
                }
                algorithm = Some(value.to_string());
            }
            other => {
                return Err(SchemaError::new(
                    format!("seg.segments[{index}].{other}"),
                    "unknown key",
                ));
            }
        }
    }
    Ok(Segment {
        label: label.ok_or_else(|| SchemaError::new(at("label"), "missing required key"))?,
        // The clinical vocabulary is the processor's to state. clinfer
        // inventing a category would be clinfer making a clinical claim.
        category: category.ok_or_else(|| {
            SchemaError::new(
                at("category"),
                "missing required key; the processor declares what the segment is, because \
                 clinfer must not invent a clinical meaning",
            )
        })?,
        property_type: property_type.ok_or_else(|| {
            SchemaError::new(
                at("type"),
                "missing required key; the processor declares what the segment is, because \
                 clinfer must not invent a clinical meaning",
            )
        })?,
        color,
        algorithm: algorithm.unwrap_or_else(|| DEFAULT_SEGMENT_ALGORITHM.to_string()),
    })
}

/// Segment Algorithm Type (0062,0008) when the processor declares none.
///
/// AUTOMATIC is what the standard calls a computed segmentation, and every
/// processor this ABI ships is one; a MANUAL producer says so explicitly.
pub const DEFAULT_SEGMENT_ALGORITHM: &str = "AUTOMATIC";

/// The `{code, scheme, meaning}` triple.
fn parse_code(value: &Json, at: &str) -> Result<Code> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            at,
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut code: Option<String> = None;
    let mut scheme: Option<String> = None;
    let mut meaning: Option<String> = None;
    for (key, field) in entries {
        let text = |field: &Json| {
            field
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| SchemaError::new(format!("{at}.{key}"), "expected a string"))
        };
        match key.as_str() {
            "code" => code = Some(text(field)?),
            "scheme" => scheme = Some(text(field)?),
            "meaning" => meaning = Some(text(field)?),
            other => {
                return Err(SchemaError::new(format!("{at}.{other}"), "unknown key"));
            }
        }
    }
    let missing = |field: &str| {
        SchemaError::new(
            format!("{at}.{field}"),
            "missing required key; a coded concept is only interpretable with all three parts",
        )
    };
    Ok(Code {
        code: code.ok_or_else(|| missing("code"))?,
        scheme: scheme.ok_or_else(|| missing("scheme"))?,
        meaning: meaning.ok_or_else(|| missing("meaning"))?,
    })
}

/// Recommended Display CIELab Value (0062,000D): three unsigned integers.
fn parse_color(value: &Json, at: &str) -> Result<[u16; 3]> {
    let Json::Arr(items) = value else {
        return Err(SchemaError::new(
            at,
            format!(
                "expected an array of three numbers, found {}",
                value.type_name()
            ),
        ));
    };
    if items.len() != 3 {
        return Err(SchemaError::new(
            at,
            format!("expected three values, found {}", items.len()),
        ));
    }
    let mut out = [0u16; 3];
    for (index, item) in items.iter().enumerate() {
        let number = number(item, at)?;
        let as_int = u16::try_from(number as i64).unwrap_or(u16::MAX);
        // A CIELab triple is three integers in each VR's range; a
        // fractional or negative component is a processor that meant
        // something else, so it is refused rather than rounded.
        if number < 0.0 || (number - f64::from(as_int)).abs() > f64::EPSILON {
            return Err(SchemaError::new(
                at,
                format!("{number} is not a whole non-negative CIELab component"),
            ));
        }
        out[index] = as_int;
    }
    Ok(out)
}

/// One `seg.files` entry.
fn parse_seg_file(value: &Json, index: usize) -> Result<SegFile> {
    let at = |field: &str| format!("seg.files[{index}].{field}");
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            format!("seg.files[{index}]"),
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut file: Option<String> = None;
    let mut frame: Option<u32> = None;
    for (key, field) in entries {
        match key.as_str() {
            "file" => {
                let name = field.as_str().ok_or_else(|| {
                    SchemaError::new(
                        at("file"),
                        format!("expected a string, found {}", field.type_name()),
                    )
                })?;
                if !is_workdir_file_name(name) {
                    return Err(SchemaError::new(
                        at("file"),
                        format!("{name:?} is not a file name inside the workdir"),
                    ));
                }
                file = Some(name.to_string());
            }
            "frame" => frame = Some(one_based(field, &at("frame"))?),
            other => {
                return Err(SchemaError::new(
                    format!("seg.files[{index}].{other}"),
                    "unknown key",
                ));
            }
        }
    }
    Ok(SegFile {
        file: file.ok_or_else(|| SchemaError::new(at("file"), "missing required key"))?,
        frame: frame.unwrap_or(1),
    })
}

/// The `stl` object: one binary surface mesh for the study.
fn parse_stl(value: &Json) -> Result<Stl> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "stl",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut file: Option<String> = None;
    for (key, field) in entries {
        match key.as_str() {
            "file" => {
                let name = field.as_str().ok_or_else(|| {
                    SchemaError::new(
                        "stl.file",
                        format!("expected a string, found {}", field.type_name()),
                    )
                })?;
                if !is_workdir_file_name(name) {
                    return Err(SchemaError::new(
                        "stl.file",
                        format!("{name:?} is not a file name inside the workdir"),
                    ));
                }
                file = Some(name.to_string());
            }
            other => {
                return Err(SchemaError::new(format!("stl.{other}"), "unknown key"));
            }
        }
    }
    Ok(Stl {
        file: file.ok_or_else(|| SchemaError::new("stl.file", "missing required key"))?,
    })
}

/// The `rtstruct` object: one structure set for the study.
///
/// A structure set with no ROIs is refused here rather than accepted and
/// later found empty, and the reason differs from the `pr` block's empty
/// `boxes`: a PR is one object PER FRAME, so "this frame had no boxes" is a
/// meaningful per-frame statement, whereas an RTSTRUCT is one object per
/// study, so "this study had no ROIs" means there is no object at all.
fn parse_rtstruct(value: &Json) -> Result<RtStruct> {
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            "rtstruct",
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut label: Option<String> = None;
    let mut name: Option<String> = None;
    let mut rois: Option<Vec<RtRoi>> = None;
    for (key, field) in entries {
        match key.as_str() {
            "label" => {
                let text = field.as_str().ok_or_else(|| {
                    SchemaError::new(
                        "rtstruct.label",
                        format!("expected a string, found {}", field.type_name()),
                    )
                })?;
                // (3006,0002) is Type 1 and an SH: an empty value is not an
                // acceptable value for an attribute whose whole job is to
                // name the object.
                if text.trim().is_empty() {
                    return Err(SchemaError::new(
                        "rtstruct.label",
                        "empty; (3006,0002) Structure Set Label is Type 1, so it must name the \
                         structure set",
                    ));
                }
                if text.len() > MAX_STRUCTURE_SET_LABEL {
                    return Err(SchemaError::new(
                        "rtstruct.label",
                        format!(
                            "{} bytes; (3006,0002) is an SH, so the label is at most {} bytes",
                            text.len(),
                            MAX_STRUCTURE_SET_LABEL
                        ),
                    ));
                }
                label = Some(text.to_string());
            }
            "name" => {
                name = Some(
                    field
                        .as_str()
                        .ok_or_else(|| SchemaError::new("rtstruct.name", "expected a string"))?
                        .to_string(),
                );
            }
            "rois" => {
                let Json::Arr(items) = field else {
                    return Err(SchemaError::new(
                        "rtstruct.rois",
                        format!("expected an array, found {}", field.type_name()),
                    ));
                };
                // Unlike `pr.boxes`, an empty list is not a valid empty
                // result: the object the block describes has one ROI
                // sequence and it would have no items.
                if items.is_empty() {
                    return Err(SchemaError::new(
                        "rtstruct.rois",
                        "an empty array; a structure set with no ROIs is not a result",
                    ));
                }
                let mut parsed = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    parsed.push(parse_roi(item, index)?);
                }
                rois = Some(parsed);
            }
            other => {
                return Err(SchemaError::new(format!("rtstruct.{other}"), "unknown key"));
            }
        }
    }
    Ok(RtStruct {
        label: label.ok_or_else(|| SchemaError::new("rtstruct.label", "missing required key"))?,
        name,
        rois: rois.ok_or_else(|| SchemaError::new("rtstruct.rois", "missing required key"))?,
    })
}

/// One `rtstruct.rois[]` entry.
fn parse_roi(value: &Json, index: usize) -> Result<RtRoi> {
    let at = |field: &str| format!("rtstruct.rois[{index}].{field}");
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            format!("rtstruct.rois[{index}]"),
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut color: Option<[u8; 3]> = None;
    let mut generation_algorithm: Option<String> = None;
    let mut interpreted_type: Option<String> = None;
    let mut contours: Option<Vec<RtContour>> = None;
    for (key, field) in entries {
        match key.as_str() {
            "name" => {
                name = Some(
                    field
                        .as_str()
                        .ok_or_else(|| SchemaError::new(at("name"), "expected a string"))?
                        .to_string(),
                );
            }
            "description" => {
                description = Some(
                    field
                        .as_str()
                        .ok_or_else(|| SchemaError::new(at("description"), "expected a string"))?
                        .to_string(),
                );
            }
            "color" => color = Some(parse_display_color(field, &at("color"))?),
            "generation_algorithm" => {
                let text = field.as_str().ok_or_else(|| {
                    SchemaError::new(at("generation_algorithm"), "expected a string")
                })?;
                if !ROI_GENERATION_ALGORITHMS.contains(&text) {
                    return Err(SchemaError::new(
                        at("generation_algorithm"),
                        format!(
                            "\"{text}\" is not one of the ROI Generation Algorithm Defined Terms \
                             of (3006,0036) in PS3.3 C.8.8.5: {}",
                            ROI_GENERATION_ALGORITHMS.join(", ")
                        ),
                    ));
                }
                generation_algorithm = Some(text.to_string());
            }
            "interpreted_type" => {
                let text = field
                    .as_str()
                    .ok_or_else(|| SchemaError::new(at("interpreted_type"), "expected a string"))?;
                if !RT_ROI_INTERPRETED_TYPES.contains(&text) {
                    return Err(SchemaError::new(
                        at("interpreted_type"),
                        format!(
                            "\"{text}\" is not one of the Defined Terms of (3006,00A4) in PS3.3 \
                             C.8.8.8; a value outside that list is not a term any reader can \
                             interpret"
                        ),
                    ));
                }
                interpreted_type = Some(text.to_string());
            }
            "contours" => {
                let Json::Arr(items) = field else {
                    return Err(SchemaError::new(
                        at("contours"),
                        format!("expected an array, found {}", field.type_name()),
                    ));
                };
                // An ROI with no contours draws nothing, which is not what
                // any model means to say: it is a lost polygon or a wrong
                // key, and both are worth a named error.
                if items.is_empty() {
                    return Err(SchemaError::new(
                        at("contours"),
                        "an empty array; an ROI is drawn by at least one contour",
                    ));
                }
                let mut parsed = Vec::with_capacity(items.len());
                for (position, item) in items.iter().enumerate() {
                    parsed.push(parse_contour(item, &at("contours"), position)?);
                }
                contours = Some(parsed);
            }
            other => {
                return Err(SchemaError::new(
                    format!("rtstruct.rois[{index}].{other}"),
                    "unknown key",
                ));
            }
        }
    }
    Ok(RtRoi {
        name: name.ok_or_else(|| SchemaError::new(at("name"), "missing required key"))?,
        description,
        // The display colour is the model's, not clinfer's: the model is
        // the only party that knows which structure it outlined, and the
        // default that `pr` applies to a box label has no analogue here
        // because two ROIs of one structure set must be distinguishable.
        color: color.ok_or_else(|| {
            SchemaError::new(
                at("color"),
                "missing required key; the model states the display colour, because two ROIs \
                 of one structure set have to be told apart",
            )
        })?,
        generation_algorithm: generation_algorithm
            .unwrap_or_else(|| DEFAULT_ROI_GENERATION_ALGORITHM.to_string()),
        interpreted_type: interpreted_type.ok_or_else(|| {
            SchemaError::new(
                at("interpreted_type"),
                "missing required key; the model declares what the ROI is, because clinfer must \
                 not invent a clinical meaning",
            )
        })?,
        contours: contours
            .ok_or_else(|| SchemaError::new(at("contours"), "missing required key"))?,
    })
}

/// One `rtstruct.rois[].contours[]` entry.
fn parse_contour(value: &Json, at: &str, position: usize) -> Result<RtContour> {
    let here = |field: &str| format!("{at}[{position}].{field}");
    let Json::Obj(entries) = value else {
        return Err(SchemaError::new(
            format!("{at}[{position}]"),
            format!("expected an object, found {}", value.type_name()),
        ));
    };
    let mut frame: Option<u32> = None;
    let mut points: Option<Vec<[f64; 2]>> = None;
    for (key, field) in entries {
        match key.as_str() {
            "frame" => frame = Some(one_based(field, &here("frame"))?),
            "points" => points = Some(parse_points(field, &here("points"), MIN_CONTOUR_POINTS)?),
            other => {
                return Err(SchemaError::new(
                    format!("{at}[{position}].{other}"),
                    "unknown key",
                ));
            }
        }
    }
    Ok(RtContour {
        frame: frame.unwrap_or(1),
        points: points.ok_or_else(|| SchemaError::new(here("points"), "missing required key"))?,
    })
}

/// A point list: an array of `[column, row]` pairs, at least `minimum` of
/// them.
///
/// Shared by the rtstruct contours and a measurement's `region`, because
/// both carry the same thing in the same order - a trace in the source
/// frame's PIXEL coordinates. `minimum` is the caller's, because the two
/// shapes differ: a closed polygon needs three points to enclose an area,
/// and an open polyline needs two to be a line at all. A caller that
/// cannot know its minimum until another key is parsed passes 0 and checks
/// the count itself afterwards.
fn parse_points(value: &Json, at: &str, minimum: usize) -> Result<Vec<[f64; 2]>> {
    let Json::Arr(items) = value else {
        return Err(SchemaError::new(
            at,
            format!(
                "expected an array of [column, row] pairs, found {}",
                value.type_name()
            ),
        ));
    };
    if items.len() < minimum {
        return Err(SchemaError::new(
            at,
            format!(
                "{} point(s); this shape needs at least {minimum}",
                items.len()
            ),
        ));
    }
    // The cap is a refusal rather than a silent downsample: downsampling
    // would move the clinical geometry without telling its author, and the
    // author is the only party that knows the tolerance.
    if items.len() > MAX_CONTOUR_POINTS {
        return Err(SchemaError::new(
            at,
            format!(
                "{} points; the ABI accepts at most {MAX_CONTOUR_POINTS} per polygon, \
                 downsample in the script where the tolerance is known",
                items.len()
            ),
        ));
    }
    let mut points = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Json::Arr(pair) = item else {
            return Err(SchemaError::new(
                format!("{at}[{index}]"),
                format!("expected [column, row], found {}", item.type_name()),
            ));
        };
        if pair.len() != 2 {
            return Err(SchemaError::new(
                format!("{at}[{index}]"),
                format!("expected two values, found {}", pair.len()),
            ));
        }
        let column = number(&pair[0], &format!("{at}[{index}][0]"))?;
        let row = number(&pair[1], &format!("{at}[{index}][1]"))?;
        points.push([column, row]);
    }
    Ok(points)
}

/// ROI Display Color (3006,002A): three integers, each 0..=255.
///
/// The VR is IS, so each channel is an integer string rather than a
/// fraction: a model that writes `[0.0, 1.0, 0.5]` means an RGB fraction,
/// and writing it as `0.0\1.0\0.5` would have a reader see 0, 1 and 0.
/// Refused rather than rounded, on the same rule `parse_color` applies to
/// the CIELab triple.
fn parse_display_color(value: &Json, at: &str) -> Result<[u8; 3]> {
    let Json::Arr(items) = value else {
        return Err(SchemaError::new(
            at,
            format!(
                "expected an array of three numbers, found {}",
                value.type_name()
            ),
        ));
    };
    if items.len() != 3 {
        return Err(SchemaError::new(
            at,
            format!("expected three values, found {}", items.len()),
        ));
    }
    let mut out = [0u8; 3];
    for (index, item) in items.iter().enumerate() {
        let component = number(item, at)?;
        let as_int = u8::try_from(component as i64).unwrap_or(u8::MAX);
        if component < 0.0 || (component - f64::from(as_int)).abs() > f64::EPSILON {
            return Err(SchemaError::new(
                at,
                format!(
                    "[ROI Display Color] (3006,002A) as PS3.3 C.8.8.6 defines it: three \
                     integers 0-255, so {component} is not a channel value"
                ),
            ));
        }
        out[index] = as_int;
    }
    Ok(out)
}

/// Whether a declared name is a file INSIDE the work directory.
///
/// One predicate rather than a copy per block: `derived`, `seg` and `stl`
/// all name files the same way, and three copies of the rule would be three
/// chances for one of them to drift into accepting a path. A name may not
/// contain a separator (of either platform's kind) and may not traverse, so
/// it cannot address anything outside the directory clinfer owns.
///
/// A colon is refused as well. It is not a separator on POSIX, but it is on
/// Windows in the `drive:` form, and it is the stream separator on NTFS
/// (`name:stream`) - so `C:research` or `blob:secret` pass a rule that only
/// looks for `/` and `\\` and still name something other than a plain file in
/// the directory. The work directory owns plain names; anything else is a
/// processor writing outside its remit.
fn is_workdir_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.contains(':')
}

/// A 1-based position: zero or a fraction is a processor that lost count,
/// not a request for the first frame.
fn one_based(value: &Json, at: &str) -> Result<u32> {
    let number = number(value, at)?;
    let as_int = u32::try_from(number as i64).unwrap_or(0);
    if as_int == 0 || (number - f64::from(as_int)).abs() > f64::EPSILON {
        return Err(SchemaError::new(
            at,
            format!("{number} is not a 1-based number"),
        ));
    }
    Ok(as_int)
}

/// A positive whole number, for a declared pixel dimension.
fn positive(value: &Json, at: &str) -> Result<u32> {
    let number = number(value, at)?;
    let as_int = u32::try_from(number as i64).unwrap_or(0);
    if as_int == 0 || (number - f64::from(as_int)).abs() > f64::EPSILON {
        return Err(SchemaError::new(
            at,
            format!("{number} is not a positive whole number of pixels"),
        ));
    }
    Ok(as_int)
}

fn number(value: &Json, at: &str) -> Result<f64> {
    match value {
        Json::F64(number) => Ok(*number),
        Json::U64(number) => Ok(*number as f64),
        other => Err(SchemaError::new(
            at,
            format!("expected a number, found {}", other.type_name()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document-level cases here are about the SCHEMA, not about the
    /// size budget, so they run at the default. The budget has its own tests
    /// below, which call [`parse`] with an explicit number.
    fn parse(bytes: &[u8]) -> Result<ResultFile> {
        super::parse(bytes, DEFAULT_MAX_RESULT_BYTES)
    }

    /// The smallest valid document: protocol, producer, and nothing else.
    const EMPTY: &str = r#"{
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {"script": "clahe.py"}
    }"#;

    /// A colon is not a POSIX separator but IS the Windows drive and NTFS
    /// stream separator, so a name that passes a `/`-and-`\\`-only rule can
    /// still address something other than a plain file in the work directory.
    #[test]
    fn a_work_directory_name_with_a_colon_is_refused() {
        assert!(is_workdir_file_name("mask.dcm"));
        assert!(is_workdir_file_name("sub-mask_1.dcm"));
        for bad in ["C:mask.dcm", "mask.dcm:hidden", ":", "a:b"] {
            assert!(
                !is_workdir_file_name(bad),
                "{bad} must not be a work-directory file name"
            );
        }
        // The existing rules still hold.
        for bad in ["", "../x.dcm", "a/b.dcm", "a\\b.dcm"] {
            assert!(!is_workdir_file_name(bad), "{bad} must be refused");
        }
    }

    #[test]
    fn a_minimal_result_parses_and_carries_nothing() {
        let parsed = parse(EMPTY.as_bytes()).expect("parses");
        assert!(parsed.is_empty(), "no pr, sr or derived block");
        assert_eq!(parsed.producer.script, "clahe.py");
        assert!(parsed.producer.model.is_none());
        assert!(!parsed.has_graphics());
        assert!(!parsed.has_derived());
        assert!(!parsed.has_text());
        assert!(!parsed.has_measurements());
    }

    // -- The measurements block --------------------------------------------

    /// A document with one finding and one measurement, the shape a
    /// classifier like torchxrayvision produces.
    const MEASURED: &str = r#"{
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {"script": "cxr10.py", "model": "densenet121-res224-all", "model_version": "1.5.4"},
        "measurements": {
            "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
            "groups": [{
                "finding": {"code": "110000", "scheme": "DCM", "meaning": "Atelectasis"},
                "measurements": [{"value": 0.62, "threshold": 0.0522}]
            }]
        }
    }"#;

    #[test]
    fn a_measurements_block_parses_and_is_a_block() {
        let parsed = parse(MEASURED.as_bytes()).expect("parses");
        assert!(!parsed.is_empty(), "a document with numbers is not empty");
        assert!(parsed.has_measurements());
        let block = parsed.measurements.as_ref().expect("the block");
        assert_eq!(block.unit.code, "1");
        assert_eq!(block.unit.scheme, "UCUM");
        assert_eq!(block.groups.len(), 1);
        let group = &block.groups[0];
        assert_eq!(group.finding.code, "110000");
        assert_eq!(group.measurements.len(), 1);
        assert_eq!(group.measurements[0].value, 0.62);
        assert_eq!(group.measurements[0].threshold, Some(0.0522));
        // The concept is absent, so the writer supplies the block default.
        assert!(group.measurements[0].concept.is_none());
        assert_eq!(default_measurement_concept().code, "122157");
    }

    #[test]
    fn the_measurements_unit_is_required() {
        // A block without a unit cannot be interpreted: 0.62 is a
        // probability, 0.62 of a percent and 0.62 of a millimetre differ.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {"groups": [{
                "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                "measurements": [{"value": 0.5}]
            }]}
        }"#;
        let error = parse(text.as_bytes()).expect_err("no unit is refused");
        assert!(
            error.at.contains("unit"),
            "the error must name the missing key: {error}"
        );
    }

    #[test]
    fn a_measurement_without_a_value_is_refused() {
        // clinfer never invents a number: an M NUM may be sent without a
        // value only for failure, which is not what a missing key means.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{"threshold": 0.5}]
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("no value is refused");
        assert!(error.at.contains("value"), "{error}");
    }

    #[test]
    fn a_non_numeric_value_is_refused_rather_than_coerced() {
        // A value sent as a STRING would otherwise reach the writer and be
        // re-emitted as whatever the string happened to be.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{"value": "0.5"}]
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("a string is refused");
        assert!(error.at.contains("value"), "{error}");
    }

    #[test]
    fn the_measurement_threshold_is_optional() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{"value": 0.5}]
                }]
            }
        }"#;
        let parsed = parse(text.as_bytes()).expect("a block without thresholds parses");
        let block = parsed.measurements.as_ref().expect("the block");
        assert!(block.groups[0].measurements[0].threshold.is_none());
        // The value is still carried verbatim.
        assert_eq!(block.groups[0].measurements[0].value, 0.5);
    }

    #[test]
    fn a_measurement_region_is_optional_and_absent_by_default() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{"value": 0.5}]
                }]
            }
        }"#;
        let parsed = parse(text.as_bytes()).expect("a block without regions parses");
        let block = parsed.measurements.as_ref().expect("the block");
        assert!(
            block.groups[0].measurements[0].region.is_none(),
            "a whole-image score has no region, and none is invented"
        );
    }

    #[test]
    fn a_closed_outline_parses_with_its_points_in_order() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{
                        "value": 0.25,
                        "region": {
                            "type": "POLYLINE",
                            "points": [[10, 20], [30.5, 20], [30.5, 40], [10, 20]]
                        }
                    }]
                }]
            }
        }"#;
        let parsed = parse(text.as_bytes()).expect("a region parses");
        let block = parsed.measurements.as_ref().expect("the block");
        let region = block.groups[0].measurements[0]
            .region
            .as_ref()
            .expect("the region");
        // [column, row] in that order, not swapped.
        assert_eq!(
            region.points,
            vec![[10.0, 20.0], [30.5, 20.0], [30.5, 40.0], [10.0, 20.0]]
        );
    }

    #[test]
    fn polygon_is_refused_with_the_standard_cited() {
        // PS3.3 C.18.6.1.2 enumerates POINT, MULTIPOINT, POLYLINE, CIRCLE
        // and ELLIPSE. POLYGON is not among them, so it is refused BY NAME
        // with the reason and the fix, rather than accepted and written out
        // as a value a strict reader would reject.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{
                        "value": 0.25,
                        "region": {"type": "POLYGON", "points": [[1,2],[3,4],[5,6]]}
                    }]
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("POLYGON is not a legal graphic type");
        assert!(error.at.contains("region.type"), "{error}");
        assert!(
            error.to_string().contains("C.18.6.1.2"),
            "the refusal cites the clause that enumerates the legal values: {error}"
        );
    }

    #[test]
    fn an_outline_with_two_points_is_refused() {
        // Two points are a line, not an area: the only shape this ABI accepts
        // is a closed outline, which needs a ring.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{
                        "value": 0.25,
                        "region": {"type": "POLYLINE", "points": [[1, 2], [3, 4]]}
                    }]
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("two points enclose no area");
        assert!(error.at.contains("region.points"), "{error}");
    }

    #[test]
    fn a_region_point_that_is_not_a_pair_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{
                        "value": 0.25,
                        "region": {"type": "POLYLINE", "points": [[1, 2, 3], [4, 5], [6, 7]]}
                    }]
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("a triple is not a point");
        assert!(error.at.contains("region.points[0]"), "{error}");
    }

    #[test]
    fn an_unsupported_graphic_type_is_refused() {
        // POINT/CIRCLE/ELLIPSE/MULTIPOINT are legal SCOORD values but are not
        // closed point lists, so this ABI does not accept them.
        for kind in ["POINT", "CIRCLE", "ELLIPSE", "MULTIPOINT", "polyline"] {
            let text = format!(
                r#"{{
                "protocol": {{"name": "clinfer-abi", "version": 1}},
                "producer": {{"script": "s.py"}},
                "measurements": {{
                    "unit": {{"code": "1", "scheme": "UCUM", "meaning": "no units"}},
                    "groups": [{{
                        "finding": {{"code": "1", "scheme": "DCM", "meaning": "F"}},
                        "measurements": [{{
                            "value": 0.25,
                            "region": {{"type": "{kind}", "points": [[1,2],[3,4],[5,6]]}}
                        }}]
                    }}]
                }}
            }}"#
            );
            let error = parse(text.as_bytes()).expect_err(kind);
            assert!(error.at.contains("region.type"), "{kind}: {error}");
        }
    }

    #[test]
    fn a_region_without_a_type_or_points_is_refused() {
        for body in [
            r#"{"points": [[1,2],[3,4],[5,6]]}"#,
            r#"{"type": "POLYLINE"}"#,
        ] {
            let text = format!(
                r#"{{
                "protocol": {{"name": "clinfer-abi", "version": 1}},
                "producer": {{"script": "s.py"}},
                "measurements": {{
                    "unit": {{"code": "1", "scheme": "UCUM", "meaning": "no units"}},
                    "groups": [{{
                        "finding": {{"code": "1", "scheme": "DCM", "meaning": "F"}},
                        "measurements": [{{"value": 0.25, "region": {body}}}]
                    }}]
                }}
            }}"#
            );
            let error = parse(text.as_bytes()).expect_err(body);
            assert!(error.at.contains("region."), "{body}: {error}");
        }
    }

    /// A `pr` block with one closed contour and nothing else.
    fn pr_with_contours(contours: &str) -> String {
        format!(
            r#"{{
            "protocol": {{"name": "clinfer-abi", "version": 1}},
            "producer": {{"script": "s.py"}},
            "pr": {{"boxes": [], "contours": [{contours}]}}
        }}"#
        )
    }

    #[test]
    fn a_contour_parses_with_its_label_points_and_frame() {
        let text = pr_with_contours(
            r#"{"label": "Left Lung", "points": [[1,2],[3,4],[1,2]], "frame": 2}"#,
        );
        let parsed = parse(text.as_bytes()).expect("a closed contour parses");
        let block = parsed.pr.as_ref().expect("the pr block");
        assert!(block.boxes.is_empty());
        assert_eq!(block.contours.len(), 1);
        let contour = &block.contours[0];
        assert_eq!(contour.label, "Left Lung");
        assert_eq!(contour.frame, 2);
        assert_eq!(
            contour.points,
            vec![[1.0, 2.0], [3.0, 4.0], [1.0, 2.0]],
            "the ring keeps its repeated closing point"
        );
    }

    #[test]
    fn a_contour_defaults_to_the_first_frame() {
        // Same default as a box: the common processor is a per-frame
        // segmenter that returns the outline for the frame it was handed.
        let text = pr_with_contours(r#"{"label": "Heart", "points": [[1,2],[3,4],[1,2]]}"#);
        let parsed = parse(text.as_bytes()).expect("a contour without a frame parses");
        let contour = &parsed.pr.as_ref().expect("the pr block").contours[0];
        assert_eq!(contour.frame, 1);
    }

    #[test]
    fn a_contours_only_pr_block_parses_without_a_boxes_key() {
        // The mirror case of the boxes-only test: a segmenter emits
        // contours and no boxes, so `boxes` is absent from the document
        // entirely. The live run caught the previous parser refusing this
        // shape with "pr.boxes: missing required key".
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "pr": {"contours": [{"label": "Left Lung", "points": [[1,2],[3,4],[1,2]]}]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("a contours-only block parses");
        let block = parsed.pr.as_ref().expect("the pr block");
        assert!(
            block.boxes.is_empty(),
            "boxes default to empty when the key is absent"
        );
        assert_eq!(block.contours.len(), 1);
        assert!(parsed.has_graphics(), "contours are graphics to store");
    }

    #[test]
    fn contours_are_optional_so_a_boxes_only_block_still_parses() {
        // The wire contract is additive: a processor that emits only boxes
        // is exactly as valid as it was before contours existed.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "pr": {"boxes": [{"label": "Nodule", "confidence": 0.9,
                              "x1": 1, "y1": 2, "x2": 3, "y2": 4}]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("a boxes-only block parses");
        let block = parsed.pr.as_ref().expect("the pr block");
        assert_eq!(block.boxes.len(), 1);
        assert!(block.contours.is_empty());
    }

    #[test]
    fn an_unclosed_contour_is_refused_by_name() {
        // Closedness is the PRODUCER's obligation and the parser checks it
        // exactly: silently closing a ring would hide a producer bug.
        let text = pr_with_contours(r#"{"label": "Left Lung", "points": [[1,2],[3,4],[5,6]]}"#);
        let error = parse(text.as_bytes()).expect_err("an open ring is not a contour");
        assert!(error.at.contains("pr.contours[0].points"), "{error}");
        assert!(
            error.to_string().contains("closed"),
            "the refusal says how to fix it: {error}"
        );
    }

    #[test]
    fn a_contour_with_two_points_is_refused() {
        let text = pr_with_contours(r#"{"label": "L", "points": [[1,2],[1,2]]}"#);
        let error = parse(text.as_bytes()).expect_err("two points enclose no area");
        assert!(error.at.contains("pr.contours[0].points"), "{error}");
    }

    #[test]
    fn a_contour_without_a_label_or_points_is_refused() {
        for body in [r#"{"points": [[1,2],[3,4],[1,2]]}"#, r#"{"label": "L"}"#] {
            let error = parse(pr_with_contours(body).as_bytes()).expect_err(body);
            assert!(error.at.contains("pr.contours[0]."), "{body}: {error}");
        }
    }

    #[test]
    fn an_unknown_key_in_a_contour_is_refused() {
        // A confidence is the interesting case: a segmenter's mask edge has
        // no score, so the key must not be quietly ignored.
        let text =
            pr_with_contours(r#"{"label": "L", "points": [[1,2],[3,4],[1,2]], "confidence": 0.9}"#);
        let error = parse(text.as_bytes()).expect_err("a confidence is not a contour key");
        assert!(error.at.contains("pr.contours[0].confidence"), "{error}");
    }

    #[test]
    fn an_unknown_key_in_the_pr_block_is_still_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "pr": {"boxes": [], "regions": []}
        }"#;
        let error = parse(text.as_bytes()).expect_err("regions is not a pr key");
        assert!(error.at.contains("pr.regions"), "{error}");
    }

    #[test]
    fn a_contour_frame_of_zero_is_refused() {
        let text = pr_with_contours(r#"{"label": "L", "points": [[1,2],[3,4],[1,2]], "frame": 0}"#);
        let error = parse(text.as_bytes()).expect_err("frame 0 is not a frame");
        assert!(error.at.contains("pr.contours[0].frame"), "{error}");
    }

    #[test]
    fn an_unknown_key_in_a_region_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{
                        "value": 0.25,
                        "region": {
                            "type": "POLYLINE",
                            "points": [[1,2],[3,4],[5,6]],
                            "color": "red"
                        }
                    }]
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("an unknown key is an error");
        assert!(error.at.contains("region.color"), "{error}");
    }

    #[test]
    fn a_region_point_is_never_coerced_from_a_string() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{
                        "value": 0.25,
                        "region": {
                            "type": "POLYLINE",
                            "points": [["1", 2], [3, 4], [5, 6]]
                        }
                    }]
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("a string coordinate is refused");
        assert!(error.at.contains("region.points[0][0]"), "{error}");
    }

    #[test]
    fn an_empty_group_list_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": []
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("an empty block carries nothing");
        assert!(error.at.contains("groups"), "{error}");
    }

    #[test]
    fn a_group_with_no_measurement_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": []
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("a finding with no number carries nothing");
        assert!(error.at.contains("measurements"), "{error}");
    }

    #[test]
    fn a_group_without_a_finding_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{"measurements": [{"value": 0.5}]}]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("a number needs a subject");
        assert!(error.at.contains("finding"), "{error}");
    }

    #[test]
    fn an_unknown_key_in_the_measurements_block_is_refused() {
        // The schema is strict everywhere: a misspelled "units" must not
        // silently become a block with no unit.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "units": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{"value": 0.5}]
                }]
            }
        }"#;
        let error = parse(text.as_bytes()).expect_err("an unknown key is refused");
        assert!(error.at.contains("units"), "{error}");
    }

    #[test]
    fn an_explicit_measurement_concept_overrides_the_default() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py"},
            "measurements": {
                "unit": {"code": "%", "scheme": "UCUM", "meaning": "Percent"},
                "groups": [{
                    "finding": {"code": "110000", "scheme": "DCM", "meaning": "Atelectasis"},
                    "measurements": [{
                        "concept": {"code": "111047", "scheme": "DCM", "meaning": "Probability of cancer"},
                        "value": 62
                    }]
                }]
            }
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        let block = parsed.measurements.as_ref().expect("the block");
        let concept = block.groups[0].measurements[0]
            .concept
            .as_ref()
            .expect("the explicit concept");
        assert_eq!(concept.code, "111047");
        // The unit is carried verbatim, not defaulted to the (1) constant.
        assert_eq!(block.unit.code, "%");
    }

    #[test]
    fn a_result_may_carry_both_a_text_report_and_measurements() {
        // The two blocks are independent: a model may answer in prose AND
        // emit numbers, and each goes to its own document.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "s.py", "model": "m", "model_version": "1"},
            "sr": {"text": "No acute finding."},
            "measurements": {
                "unit": {"code": "1", "scheme": "UCUM", "meaning": "no units"},
                "groups": [{
                    "finding": {"code": "1", "scheme": "DCM", "meaning": "F"},
                    "measurements": [{"value": 0.1}]
                }]
            }
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(parsed.has_text());
        assert!(parsed.has_measurements());
    }

    #[test]
    fn a_measurements_block_with_only_empty_groups_has_none() {
        // The predicate and the parser agree: a block whose every group is
        // empty is a block the parser refuses, so `has_measurements` can
        // never be asked that question - and answers `false` for a block
        // that is absent.
        let parsed = parse(EMPTY.as_bytes()).expect("parses");
        assert!(!parsed.has_measurements());
    }

    #[test]
    fn an_unknown_top_level_key_is_a_named_error() {
        // The strictness rule: a typo must not look like a valid empty
        // result, or a script that meant to report boxes silently reports
        // nothing and the workitem is cancelled with the wrong reason.
        let text = EMPTY.replace("\"producer\"", "\"detectionz\": {}, \"producer\"");
        let err = parse(text.as_bytes()).expect_err("unknown key");
        assert_eq!(err.at, "detectionz");
        assert!(err.message.contains("unknown key"), "{err}");
    }

    #[test]
    fn an_unknown_nested_key_names_its_path() {
        let text = EMPTY.replace(
            "\"script\": \"clahe.py\"",
            "\"script\": \"clahe.py\", \"seedy\": 1",
        );
        let err = parse(text.as_bytes()).expect_err("unknown key");
        assert_eq!(err.at, "producer.seedy");
    }

    #[test]
    fn a_missing_protocol_is_refused_by_name() {
        let text = r#"{"producer": {"script": "x.py"}}"#;
        let err = parse(text.as_bytes()).expect_err("no protocol");
        assert_eq!(err.at, "protocol");
        assert!(err.message.contains("missing"), "{err}");
    }

    #[test]
    fn a_missing_producer_is_refused_by_name() {
        let text = r#"{"protocol": {"name": "clinfer-abi", "version": 1}}"#;
        let err = parse(text.as_bytes()).expect_err("no producer");
        assert_eq!(err.at, "producer");
    }

    #[test]
    fn a_producer_without_a_script_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"model": "yolo"}
        }"#;
        let err = parse(text.as_bytes()).expect_err("no script");
        assert_eq!(err.at, "producer.script");
    }

    #[test]
    fn a_future_protocol_version_names_both_versions() {
        // A version mismatch that parsed would be worse than one that does
        // not, so the message has to say which side to change.
        let text = EMPTY.replace("\"version\": 1", "\"version\": 2");
        let err = parse(text.as_bytes()).expect_err("wrong version");
        assert_eq!(err.at, "protocol.version");
        assert!(err.message.contains("version 2"), "{err}");
        assert!(err.message.contains("version 1"), "{err}");
    }

    #[test]
    fn a_wrong_protocol_name_is_refused() {
        let text = EMPTY.replace("clinfer-abi", "some-other-abi");
        let err = parse(text.as_bytes()).expect_err("wrong name");
        assert_eq!(err.at, "protocol.name");
    }

    #[test]
    fn a_box_list_round_trips() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "detect.py", "model": "yolo11n", "model_version": "2026.1",
                         "seed": "42", "script_version": "1.0.0"},
            "pr": {"boxes": [
                {"label": "Nodule", "confidence": 0.87, "x1": 10, "y1": 20, "x2": 30, "y2": 40},
                {"label": "Mass", "x1": 1, "y1": 2, "x2": 3, "y2": 4}
            ]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(parsed.has_graphics());
        assert!(!parsed.has_derived());
        let pr = parsed.pr.as_ref().expect("pr");
        assert_eq!(pr.boxes.len(), 2);
        assert_eq!(pr.boxes[0].label, "Nodule");
        assert!((pr.boxes[0].confidence - 0.87).abs() < f64::EPSILON);
        // Integers are accepted where a float is expected: JSON has one
        // number type and a script writing `"x1": 10` means ten.
        assert_eq!(pr.boxes[1].x1, 1.0);
        // A missing confidence is not an error; it defaults to zero.
        assert_eq!(pr.boxes[1].confidence, 0.0);
        let producer = &parsed.producer;
        assert_eq!(producer.model.as_deref(), Some("yolo11n"));
        assert_eq!(producer.seed.as_deref(), Some("42"));
    }

    #[test]
    fn a_box_missing_a_coordinate_is_refused_at_its_path() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "pr": {"boxes": [{"label": "A", "x1": 1, "y1": 2, "x2": 3}]}
        }"#;
        let err = parse(text.as_bytes()).expect_err("no y2");
        assert_eq!(err.at, "pr.boxes[0].y2");
    }

    #[test]
    fn a_box_with_a_non_numeric_coordinate_is_refused_at_its_path() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "pr": {"boxes": [{"label": "A", "x1": "left", "y1": 2, "x2": 3, "y2": 4}]}
        }"#;
        let err = parse(text.as_bytes()).expect_err("string coordinate");
        assert_eq!(err.at, "pr.boxes[0].x1");
        assert!(err.message.contains("expected a number"), "{err}");
    }

    #[test]
    fn a_box_without_a_frame_belongs_to_the_first_frame() {
        // The common processor is a per-frame detector handed one frame, so
        // omitting the frame is the normal case and must not be an error.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "pr": {"boxes": [{"label": "A", "x1": 1, "y1": 2, "x2": 3, "y2": 4}]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        let boxes = &parsed.pr.as_ref().expect("pr").boxes;
        assert_eq!(boxes[0].frame, 1);
    }

    #[test]
    fn a_declared_frame_is_carried_and_groups_the_boxes() {
        // Two boxes on frame 2 and one on frame 1: the worker emits one
        // overlay per frame that HAS boxes, so the grouping is the
        // contract, not a convenience.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "pr": {"boxes": [
                {"label": "OnOne", "x1": 1, "y1": 2, "x2": 3, "y2": 4},
                {"label": "OnTwo", "frame": 2, "x1": 5, "y1": 6, "x2": 7, "y2": 8},
                {"label": "AlsoTwo", "frame": 2, "x1": 9, "y1": 10, "x2": 11, "y2": 12}
            ]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert_eq!(parsed.max_box_frame(), Some(2));

        let mut on_two: Vec<String> = Vec::new();
        parsed.boxes_on_frame(2, |entry| on_two.push(entry.label.clone()));
        assert_eq!(on_two, vec!["OnTwo", "AlsoTwo"]);

        let mut on_one: Vec<String> = Vec::new();
        parsed.boxes_on_frame(1, |entry| on_one.push(entry.label.clone()));
        assert_eq!(on_one, vec!["OnOne"]);

        // A frame nothing is drawn on yields nothing rather than the first
        // frame's boxes: that is what keeps an empty overlay from being
        // stored.
        let mut on_three: Vec<String> = Vec::new();
        parsed.boxes_on_frame(3, |entry| on_three.push(entry.label.clone()));
        assert!(on_three.is_empty());
    }

    #[test]
    fn a_frame_that_is_not_a_positive_whole_number_is_refused() {
        // `frame` is a POSITION, so zero and 1.5 are a processor that lost
        // count. Refusing by name beats attaching the box to frame 1 and
        // drawing it over the wrong image.
        for bad in ["0", "-1", "1.5"] {
            let text = format!(
                r#"{{
                    "protocol": {{"name": "clinfer-abi", "version": 1}},
                    "producer": {{"script": "x.py"}},
                    "pr": {{"boxes": [{{"label": "A", "frame": {bad},
                                        "x1": 1, "y1": 2, "x2": 3, "y2": 4}}]}}
                }}"#
            );
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("frame {bad} was accepted"));
            assert_eq!(err.at, "pr.boxes[0].frame");
            assert!(err.message.contains("1-based"), "{err}");
        }
    }

    #[test]
    fn a_result_with_no_boxes_has_no_highest_frame() {
        // `max_box_frame` is what the worker checks a study's frame count
        // against, so "no boxes" must be distinguishable from "frame 1".
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "pr": {"boxes": []}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert_eq!(parsed.max_box_frame(), None);
    }

    #[test]
    fn an_empty_pr_block_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "pr": {}
        }"#;
        let err = parse(text.as_bytes()).expect_err("an empty block");
        assert_eq!(err.at, "pr");
    }

    #[test]
    fn an_sr_block_carries_its_text_verbatim() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "sr_llm.py"},
            "sr": {"text": "Очаговых изменений не выявлено."}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(parsed.has_text());
        assert!(!parsed.has_graphics());
        assert_eq!(
            parsed.sr.as_ref().expect("sr").text,
            "Очаговых изменений не выявлено."
        );
    }

    #[test]
    fn a_derived_block_names_its_files() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "clahe.py"},
            "derived": {"files": [{"file": "0001.png.raw"}, {"file": "0002.png.raw"}]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(parsed.has_derived());
        let files = &parsed.derived.as_ref().expect("derived").files;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file, "0001.png.raw");
        assert_eq!(files[1].file, "0002.png.raw");
        // Omitted geometry means the source geometry for that axis.
        assert_eq!(files[0].width, None);
        assert_eq!(files[0].height, None);
        assert_eq!(files[0].bits, None);
    }

    #[test]
    fn a_derived_file_may_declare_its_own_geometry() {
        // The relaxed rule: DICOM allows any size, so upscale, downscale
        // and crop are all legal and the processor declares what it made.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "upscale.py"},
            "derived": {"files": [{"file": "0001.raw", "width": 1024, "height": 1024}]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        let files = &parsed.derived.as_ref().expect("derived").files;
        assert_eq!(files[0].width, Some(1024));
        assert_eq!(files[0].height, Some(1024));
        assert_eq!(files[0].bits, None, "bits omitted means the source's");
    }

    #[test]
    fn a_declared_geometry_that_is_not_a_positive_whole_number_is_refused() {
        // Zero width is a processor that lost count, not a request for an
        // empty image: a stored object of no size is worse than a refusal.
        for (field, value) in [("width", "0"), ("height", "-4"), ("width", "12.5")] {
            let text = format!(
                r#"{{
                    "protocol": {{"name": "clinfer-abi", "version": 1}},
                    "producer": {{"script": "x.py"}},
                    "derived": {{"files": [{{"file": "a.raw", "{field}": {value}}}]}}
                }}"#
            );
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("{field}={value} was accepted"));
            assert_eq!(err.at, format!("derived.files[0].{field}"));
        }
    }

    #[test]
    fn a_derived_block_with_an_unknown_key_is_refused() {
        // The strictness rule applies to every block. This one used to be
        // the exception: an unknown key there was silently ignored.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "derived": {"filez": []}
        }"#;
        let err = parse(text.as_bytes()).expect_err("an unknown key is an error");
        assert_eq!(err.at, "derived.filez");
    }

    #[test]
    fn a_derived_file_that_escapes_the_workdir_is_refused() {
        // The workdir is the unit of work. A result naming a path outside
        // it would be one clinfer cannot bound, validate or clean up.
        //
        // The names are the strings the SCHEMA sees, so the backslash is
        // written as JSON requires it - an unescaped one is a malformed
        // document, which the parser refuses one layer earlier.
        for name in [
            "../escape.raw",
            "sub/dir.raw",
            "sub\\\\dir.raw",
            "/etc/passwd",
        ] {
            let text = format!(
                r#"{{
                    "protocol": {{"name": "clinfer-abi", "version": 1}},
                    "producer": {{"script": "x.py"}},
                    "derived": {{"files": [{{"file": "{name}"}}]}}
                }}"#
            );
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("{name} was accepted"));
            assert_eq!(err.at, "derived.files[0].file");
            assert!(err.message.contains("workdir"), "{err}");
        }
    }

    #[test]
    fn all_four_blocks_may_appear_together() {
        // One processor is one pipeline stage, and its result may cover
        // every kind at once - the contract does not force a choice.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "all.py"},
            "pr": {"boxes": [{"label": "A", "x1": 0, "y1": 0, "x2": 1, "y2": 1}]},
            "sr": {"text": "one finding"},
            "derived": {"files": [{"file": "0001.raw"}]},
            "seg": {"segments": [{"label": "M", "category": {"code": "c", "scheme": "s", "meaning": "m"}, "type": {"code": "c", "scheme": "s", "meaning": "m"}}],
                    "files": [{"file": "0001.mask"}]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(!parsed.is_empty());
        assert!(parsed.has_graphics() && parsed.has_text() && parsed.has_derived());
        assert!(parsed.has_segmentation());
    }

    #[test]
    fn a_non_object_document_is_refused() {
        for text in ["[]", "\"text\"", "42", "null"] {
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("{text} was accepted"));
            assert!(err.message.contains("not an object"), "{err}");
        }
    }

    #[test]
    fn an_oversized_result_is_refused_before_it_is_parsed() {
        // The bound also protects the parser: the check runs on the byte
        // count, so a runaway document never reaches the reader.
        let padding = " ".repeat((DEFAULT_MAX_RESULT_BYTES + 1) as usize);
        let err = parse(padding.as_bytes()).expect_err("too large");
        assert!(err.message.contains("max_result_bytes"), "{err}");
    }

    /// The budget is the CALLER's, and the refusal names BOTH the number in
    /// force and the configuration key that sets it.
    ///
    /// Naming the key is the point: an operator reading the log line has to
    /// know which knob to raise without going back to the source.
    #[test]
    fn the_size_refusal_names_the_budget_and_the_key_that_sets_it() {
        let padding = " ".repeat(1024);
        let err = super::parse(padding.as_bytes(), 512).expect_err("too large");
        assert!(
            err.message
                .contains("over the model's max_result_bytes limit of 512"),
            "the message must name the key and the budget in force: {err}"
        );
        assert!(
            err.message.contains("1024 bytes"),
            "and the size it saw: {err}"
        );
        // The same bytes under a larger budget pass the size check and fail
        // in the parser instead, which is what proves the budget was the
        // only thing refusing them.
        let err = super::parse(padding.as_bytes(), 2048).expect_err("not valid JSON");
        assert!(
            !err.message.contains("max_result_bytes"),
            "a document inside its budget is not refused for size: {err}"
        );
    }

    /// A document that fits the default is refused by a smaller budget and
    /// accepted by a larger one, which is the whole point of the knob.
    #[test]
    fn a_larger_budget_admits_what_a_smaller_one_refuses() {
        let document = format!("{EMPTY}{}", " ".repeat(2048));
        let bytes = document.as_bytes();

        assert!(
            super::parse(bytes, 1024).is_err(),
            "a 1 KiB budget refuses a 2 KiB document"
        );
        assert!(
            super::parse(bytes, 8192).is_ok(),
            "an 8 KiB budget admits the same document"
        );
        assert!(
            super::parse(bytes, DEFAULT_MAX_RESULT_BYTES).is_ok(),
            "the default admits it too"
        );
    }

    #[test]
    fn a_whitespace_only_text_is_not_reported_as_present() {
        // `has_text` decides whether an sr block is worth a document. A
        // block whose text is blank carries nothing, so it must not be
        // mistaken for content the way a non-empty string would be.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "sr": {"text": "   \n  "}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(!parsed.has_text(), "blank text carries nothing");
        assert!(!parsed.is_empty(), "but the block itself was produced");
    }

    #[test]
    fn an_empty_box_list_is_not_a_box_result() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "pr": {"boxes": []}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(
            !parsed.has_graphics(),
            "a detector that found nothing drew nothing"
        );
        assert!(!parsed.is_empty(), "the block was still produced");
    }

    // -- seg ----------------------------------------------------------

    /// A minimal valid `seg` block, with the parts a test wants to vary
    /// spliced in as text.
    fn seg_document(segments: &str, files: &str) -> String {
        format!(
            r#"{{
                "protocol": {{"name": "clinfer-abi", "version": 1}},
                "producer": {{"script": "threshold_seg.py"}},
                "seg": {{"segments": [{segments}], "files": [{files}]}}
            }}"#
        )
    }

    /// One segment with a category and a type, which are both required.
    const SEGMENT: &str = r#"{"label": "MASK",
        "category": {"code": "M-01000", "scheme": "SRT", "meaning": "Morphologically Altered Structure"},
        "type": {"code": "M-01000", "scheme": "SRT", "meaning": "Morphologically Altered Structure"}}"#;

    #[test]
    fn a_seg_block_parses_with_its_segments_and_masks() {
        let text = seg_document(SEGMENT, r#"{"file": "0001.mask"}"#);
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(parsed.has_segmentation());
        let seg = parsed.seg.as_ref().expect("seg");
        assert_eq!(seg.segments.len(), 1);
        assert_eq!(seg.segments[0].label, "MASK");
        assert_eq!(seg.segments[0].category.code, "M-01000");
        assert_eq!(seg.segments[0].category.scheme, "SRT");
        assert_eq!(
            seg.segments[0].property_type.meaning,
            "Morphologically Altered Structure"
        );
        assert_eq!(
            seg.segments[0].algorithm, "AUTOMATIC",
            "the default when the processor declares none"
        );
        assert_eq!(seg.segments[0].color, None);
        assert_eq!(seg.files[0].file, "0001.mask");
        assert_eq!(seg.files[0].frame, 1, "the default when none is declared");
    }

    #[test]
    fn a_segment_may_declare_its_colour_and_algorithm() {
        let segment = r#"{"label": "L",
            "category": {"code": "c", "scheme": "s", "meaning": "m"},
            "type": {"code": "c", "scheme": "s", "meaning": "m"},
            "color": [255, 128, 0], "algorithm": "MANUAL"}"#;
        let text = seg_document(segment, r#"{"file": "a.mask"}"#);
        let parsed = parse(text.as_bytes()).expect("parses");
        let segment = &parsed.seg.as_ref().expect("seg").segments[0];
        assert_eq!(segment.color, Some([255, 128, 0]));
        assert_eq!(segment.algorithm, "MANUAL");
    }

    #[test]
    fn a_segment_without_a_category_or_type_is_refused() {
        // The clinical vocabulary is the processor's to state. clinfer
        // inventing a category would be clinfer making a clinical claim,
        // so a missing one is an error rather than a default.
        for (what, segment) in [
            (
                "category",
                r#"{"label": "L", "type": {"code": "c", "scheme": "s", "meaning": "m"}}"#,
            ),
            (
                "type",
                r#"{"label": "L", "category": {"code": "c", "scheme": "s", "meaning": "m"}}"#,
            ),
        ] {
            let text = seg_document(segment, r#"{"file": "a.mask"}"#);
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("a segment with no {what} was accepted"));
            assert_eq!(err.at, format!("seg.segments[0].{what}"));
            assert!(err.message.contains("clinical meaning"), "{err}");
        }
    }

    #[test]
    fn a_code_missing_any_of_its_three_parts_is_refused() {
        // A code with no scheme is not resolvable, and a code with no
        // meaning cannot be shown to a reader: all three are required.
        for field in ["code", "scheme", "meaning"] {
            let others: Vec<String> = ["code", "scheme", "meaning"]
                .iter()
                .filter(|name| **name != field)
                .map(|name| format!("\"{name}\": \"x\""))
                .collect();
            let segment = format!(
                r#"{{"label": "L",
                    "category": {{{}}},
                    "type": {{"code": "c", "scheme": "s", "meaning": "m"}}}}"#,
                others.join(", ")
            );
            let text = seg_document(&segment, r#"{"file": "a.mask"}"#);
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("a category with no {field} was accepted"));
            assert_eq!(err.at, format!("seg.segments[0].category.{field}"));
        }
    }

    #[test]
    fn an_empty_segment_list_is_refused() {
        // A mask bit with no segment behind it means nothing, so a
        // segmentation with no segments is not a result.
        let text = seg_document("", r#"{"file": "a.mask"}"#);
        let err = parse(text.as_bytes()).expect_err("no segments");
        assert_eq!(err.at, "seg.segments");
        assert!(err.message.contains("at least one segment"), "{err}");
    }

    #[test]
    fn segment_frames_must_be_the_run_one_to_n() {
        // The masks are one per source image, so a gap means an image with
        // no mask and a repeat means one mask claimed twice.
        for (what, files) in [
            (
                "a gap",
                r#"{"file": "a.mask", "frame": 1}, {"file": "b.mask", "frame": 3}"#,
            ),
            (
                "a repeat",
                r#"{"file": "a.mask", "frame": 1}, {"file": "b.mask", "frame": 1}"#,
            ),
        ] {
            let text = seg_document(SEGMENT, files);
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("{what} was accepted"));
            assert!(
                err.at.starts_with("seg.files[") && err.at.ends_with(".frame"),
                "the path names the offending entry: {}",
                err.at
            );
            assert!(err.message.contains("no gaps and no repeats"), "{err}");
        }

        // A zero is refused one layer earlier, while the entry is read: the
        // frame field is a 1-based position wherever it appears, and the
        // message says so rather than describing the run.
        let zero = seg_document(SEGMENT, r#"{"file": "a.mask", "frame": 0}"#);
        let err = parse(zero.as_bytes()).expect_err("a zero frame was accepted");
        assert_eq!(err.at, "seg.files[0].frame");
        assert!(err.message.contains("1-based"), "{err}");
    }

    #[test]
    fn segment_frames_in_any_order_are_the_same_run() {
        // The declaration may list the masks in any order as long as the
        // set is 1..=N: the frame number carries the association, not the
        // position in the array.
        let files = r#"{"file": "second.mask", "frame": 2}, {"file": "first.mask", "frame": 1}"#;
        let text = seg_document(SEGMENT, files);
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(parsed.seg_frames_are_complete());
        assert_eq!(parsed.seg.as_ref().expect("seg").files[0].frame, 2);
    }

    #[test]
    fn an_empty_mask_list_is_refused() {
        let text = seg_document(SEGMENT, "");
        let err = parse(text.as_bytes()).expect_err("no masks");
        assert_eq!(err.at, "seg.files");
    }

    #[test]
    fn a_mask_that_escapes_the_workdir_is_refused() {
        for name in ["../escape.mask", "sub/dir.mask", "/etc/passwd"] {
            let text = seg_document(SEGMENT, &format!(r#"{{"file": "{name}"}}"#));
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("{name} was accepted"));
            assert_eq!(err.at, "seg.files[0].file");
            assert!(err.message.contains("workdir"), "{err}");
        }
    }

    #[test]
    fn a_seg_block_with_an_unknown_key_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "seg": {"labelz": 1}
        }"#;
        let err = parse(text.as_bytes()).expect_err("unknown key");
        assert_eq!(err.at, "seg.labelz");
    }

    #[test]
    fn a_binary_only_segmentation_needs_no_algorithm_the_processor_did_not_state() {
        // The default is the standard's own word for a computed
        // segmentation, so an omitted key is not a guess.
        let text = seg_document(SEGMENT, r#"{"file": "a.mask"}"#);
        let parsed = parse(text.as_bytes()).expect("parses");
        assert_eq!(
            parsed.seg.as_ref().expect("seg").segments[0].algorithm,
            DEFAULT_SEGMENT_ALGORITHM
        );
        assert_eq!(DEFAULT_SEGMENT_ALGORITHM, "AUTOMATIC");
    }

    #[test]
    fn a_blank_algorithm_is_refused_rather_than_defaulted() {
        // The key is present, so the processor meant something specific; an
        // empty value is a mistake, and silently taking the default would
        // hide it.
        let segment = r#"{"label": "L",
            "category": {"code": "c", "scheme": "s", "meaning": "m"},
            "type": {"code": "c", "scheme": "s", "meaning": "m"},
            "algorithm": "  "}"#;
        let text = seg_document(segment, r#"{"file": "a.mask"}"#);
        let err = parse(text.as_bytes()).expect_err("blank algorithm");
        assert_eq!(err.at, "seg.segments[0].algorithm");
    }

    #[test]
    fn a_colour_that_is_not_three_whole_components_is_refused() {
        for bad in ["[1, 2]", "[1, 2, 3, 4]", "[1, 2.5, 3]", "[1, -2, 3]"] {
            let segment = format!(
                r#"{{"label": "L",
                    "category": {{"code": "c", "scheme": "s", "meaning": "m"}},
                    "type": {{"code": "c", "scheme": "s", "meaning": "m"}},
                    "color": {bad}}}"#
            );
            let text = seg_document(&segment, r#"{"file": "a.mask"}"#);
            let err = parse(text.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("colour {bad} was accepted"));
            assert_eq!(err.at, "seg.segments[0].color");
        }
    }

    #[test]
    fn a_segmentation_with_no_files_declared_is_not_a_segmentation() {
        // Two shapes: the key absent entirely, and the key present but
        // empty. Both say the same thing - no masks were produced - and
        // both are refused rather than stored as an object with nothing in
        // it.
        let absent = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "seg": {"segments": []}
        }"#;
        let err = parse(absent.as_bytes()).expect_err("the segments list is empty");
        assert_eq!(err.at, "seg.segments");

        let empty_files = seg_document(SEGMENT, "");
        let err = parse(empty_files.as_bytes()).expect_err("the files list is empty");
        assert_eq!(err.at, "seg.files");
        assert!(err.message.contains("at least one"), "{err}");

        let no_segments = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "x.py"},
            "seg": {"files": [{"file": "a.mask"}]}
        }"#;
        let err = parse(no_segments.as_bytes()).expect_err("no segments key");
        assert_eq!(err.at, "seg.segments");
    }

    // -- stl ----------------------------------------------------------

    /// A minimal valid `stl` block, with the name spliced in as text.
    fn stl_document(file: &str) -> String {
        format!(
            r#"{{
                "protocol": {{"name": "clinfer-abi", "version": 1}},
                "producer": {{"script": "stl_mesh.py"}},
                "stl": {{"file": {file}}}
            }}"#
        )
    }

    #[test]
    fn an_stl_block_parses_with_its_file() {
        let text = stl_document(r#""mesh.stl""#);
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(parsed.has_surface());
        assert_eq!(parsed.stl.as_ref().expect("stl").file, "mesh.stl");
    }

    #[test]
    fn an_stl_block_without_a_file_is_refused() {
        // `file` names the mesh. An absent key is a processor that thinks
        // it returned a surface and did not.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "stl_mesh.py"},
            "stl": {}
        }"#;
        let err = parse(text.as_bytes()).expect_err("no file");
        assert_eq!(err.at, "stl.file");
        assert!(err.message.contains("missing required key"), "{err}");
    }

    #[test]
    fn an_empty_stl_block_carries_nothing() {
        // The block is optional: a result without one carries no surface,
        // and a result with only a surface is still something (the
        // emptiness rule decides whether a cycle produced anything).
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "boxes.py"},
            "pr": {"boxes": [{"label": "A", "x1": 1, "y1": 1, "x2": 2, "y2": 2}]}
        }"#;
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(!parsed.has_surface());
        assert!(parsed.stl.is_none());
        assert!(!parsed.is_empty(), "boxes alone are still something");
    }

    #[test]
    fn an_unknown_key_in_the_stl_block_is_an_error() {
        let text = stl_document(r#""mesh.stl""#).replace("\"file\"", "\"files\"");
        let err = parse(text.as_bytes()).expect_err("unknown key");
        assert_eq!(err.at, "stl.files");
    }

    #[test]
    fn a_result_carrying_only_a_surface_is_not_empty() {
        // The emptiness rule decides whether a cycle produced anything at
        // all. A result with only an stl block produced a mesh, so it must
        // not be treated as an empty cycle and cancelled.
        let text = stl_document(r#""mesh.stl""#);
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(!parsed.is_empty(), "a surface is something");
        assert!(parsed.has_surface());
    }

    #[test]
    fn a_file_name_that_leaves_the_workdir_is_refused() {
        // The workdir is the unit of work: a name that traverses is a
        // result clinfer could not bound or clean up.
        for name in ["../mesh.stl", "sub/mesh.stl", "sub\\\\mesh.stl"] {
            let text = stl_document(&format!("\"{name}\""));
            let err = parse(text.as_bytes()).expect_err(name);
            assert_eq!(err.at, "stl.file", "{name}");
            assert!(err.message.contains("file name inside"), "{name}: {err}");
        }
    }

    #[test]
    fn a_blank_file_name_is_refused() {
        let text = stl_document(r#""""#);
        let err = parse(text.as_bytes()).expect_err("blank name");
        assert_eq!(err.at, "stl.file");
        assert!(err.message.contains("file name inside"), "{err}");
    }

    /// One ROI carrying one three-point contour, for the rtstruct tests.
    fn roi_json() -> &'static str {
        r#"{
            "name": "LUNG_LEFT",
            "color": [0, 255, 102],
            "interpreted_type": "ORGAN",
            "contours": [{"frame": 3, "points": [[12.5, 40.0], [13.0, 41.5], [11.0, 42.0]]}]
        }"#
    }

    /// A document with one `label` and the given ROI array body.
    fn rtstruct_document(rois: &str) -> String {
        format!(
            r#"{{
                "protocol": {{"name": "clinfer-abi", "version": 1}},
                "producer": {{"script": "totalseg.py"}},
                "rtstruct": {{"label": "TOTALSEG", "rois": [{rois}]}}
            }}"#
        )
    }

    #[test]
    fn an_rtstruct_block_parses_with_its_roi_and_contour() {
        let text = rtstruct_document(roi_json());
        let parsed = parse(text.as_bytes()).expect("parses");
        assert!(parsed.has_rtstruct());
        assert!(!parsed.is_empty(), "a structure set is something");
        let rtstruct = parsed.rtstruct.as_ref().expect("rtstruct");
        assert_eq!(rtstruct.label, "TOTALSEG");
        assert_eq!(rtstruct.rois.len(), 1);
        let roi = &rtstruct.rois[0];
        assert_eq!(roi.name, "LUNG_LEFT");
        assert_eq!(roi.color, [0, 255, 102]);
        assert_eq!(roi.interpreted_type, "ORGAN");
        // generation_algorithm is optional and defaults to AUTOMATIC.
        assert_eq!(roi.generation_algorithm, DEFAULT_ROI_GENERATION_ALGORITHM);
        assert_eq!(roi.contours.len(), 1);
        assert_eq!(roi.contours[0].frame, 3);
        assert_eq!(roi.contours[0].points.len(), 3);
        assert_eq!(roi.contours[0].points[0], [12.5, 40.0]);
    }

    #[test]
    fn a_contour_frame_defaults_to_the_first_frame() {
        let rois = r#"{
            "name": "A",
            "color": [1, 2, 3],
            "interpreted_type": "ORGAN",
            "contours": [{"points": [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]}]
        }"#;
        let parsed = parse(rtstruct_document(rois).as_bytes()).expect("parses");
        let contour = &parsed.rtstruct.expect("rtstruct").rois[0].contours[0];
        assert_eq!(contour.frame, 1, "an omitted frame means the first frame");
    }

    #[test]
    fn an_rtstruct_block_with_no_rois_is_refused() {
        // The `pr` empty-boxes precedent does NOT apply: a PR is one object
        // per frame, so "no boxes here" is a per-frame statement, whereas a
        // structure set with no ROIs is not an object at all.
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "totalseg.py"},
            "rtstruct": {"label": "TOTALSEG", "rois": []}
        }"#;
        let err = parse(text.as_bytes()).expect_err("no rois");
        assert_eq!(err.at, "rtstruct.rois");
        assert!(err.message.contains("no ROIs is not a result"), "{err}");
    }

    #[test]
    fn an_rtstruct_block_without_a_label_is_refused() {
        let text = r#"{
            "protocol": {"name": "clinfer-abi", "version": 1},
            "producer": {"script": "totalseg.py"},
            "rtstruct": {"rois": [ROIS]}
        }"#
        .replace("ROIS", roi_json());
        let err = parse(text.as_bytes()).expect_err("no label");
        assert_eq!(err.at, "rtstruct.label");
    }

    #[test]
    fn a_blank_structure_set_label_is_refused() {
        // (3006,0002) is Type 1: an empty value is not an acceptable value
        // for the attribute whose job is to name the object.
        let text = rtstruct_document(roi_json()).replace("TOTALSEG", "   ");
        let err = parse(text.as_bytes()).expect_err("blank label");
        assert_eq!(err.at, "rtstruct.label");
        assert!(err.message.contains("Type 1"), "{err}");
    }

    #[test]
    fn an_over_long_structure_set_label_is_refused() {
        // SH is a 16-byte value. Refused at parse rather than truncated,
        // because the label is the model's statement about the object.
        let text = rtstruct_document(roi_json()).replace("TOTALSEG", "TOTALSEGMENTATOR_ORGANS");
        let err = parse(text.as_bytes()).expect_err("too long");
        assert_eq!(err.at, "rtstruct.label");
        assert!(err.message.contains("SH"), "{err}");
    }

    #[test]
    fn a_fractional_roi_display_color_is_refused() {
        // The natural mistake: an RGB fraction instead of the 0-255 range.
        // Writing 0.0\1.0\0.5 would have a reader see 0, 1 and 0.
        let rois = roi_json().replace("[0, 255, 102]", "[0.0, 1.0, 0.5]");
        let err = parse(rtstruct_document(&rois).as_bytes()).expect_err("fraction");
        assert_eq!(err.at, "rtstruct.rois[0].color");
        assert!(err.message.contains("3006,002A"), "{err}");
        assert!(err.message.contains("0-255"), "{err}");
    }

    #[test]
    fn an_out_of_range_roi_display_color_is_refused() {
        let rois = roi_json().replace("[0, 255, 102]", "[0, 256, 102]");
        let err = parse(rtstruct_document(&rois).as_bytes()).expect_err("out of range");
        assert_eq!(err.at, "rtstruct.rois[0].color");
    }

    #[test]
    fn a_generation_algorithm_outside_the_enumeration_is_refused() {
        // "AUTO" is not a Defined Term: the list is closed.
        let rois = roi_json().replace(
            r#""interpreted_type": "ORGAN","#,
            r#""interpreted_type": "ORGAN", "generation_algorithm": "AUTO","#,
        );
        let err = parse(rtstruct_document(&rois).as_bytes()).expect_err("bad algorithm");
        assert_eq!(err.at, "rtstruct.rois[0].generation_algorithm");
        assert!(err.message.contains("3006,0036"), "{err}");
        assert!(err.message.contains("AUTOMATIC"), "{err}");
    }

    #[test]
    fn an_interpreted_type_outside_the_enumeration_is_refused() {
        // "LUNGS" is a clinical idea, not a Defined Term of (3006,00A4).
        let rois = roi_json().replace("\"ORGAN\"", "\"LUNGS\"");
        let err = parse(rtstruct_document(&rois).as_bytes()).expect_err("bad type");
        assert_eq!(err.at, "rtstruct.rois[0].interpreted_type");
        assert!(err.message.contains("3006,00A4"), "{err}");
        assert!(err.message.contains("C.8.8.8"), "{err}");
    }

    #[test]
    fn every_defined_interpreted_type_is_accepted() {
        // The list is the standard's, so the parser must accept all of it -
        // a test that only tried ORGAN would not notice a truncated table.
        for term in RT_ROI_INTERPRETED_TYPES {
            let rois = roi_json().replace("\"ORGAN\"", &format!("\"{term}\""));
            let parsed = parse(rtstruct_document(&rois).as_bytes())
                .unwrap_or_else(|err| panic!("{term} must be accepted: {err}"));
            assert_eq!(
                parsed.rtstruct.expect("rtstruct").rois[0].interpreted_type,
                term
            );
        }
        assert_eq!(RT_ROI_INTERPRETED_TYPES.len(), 25, "C.8.8.8 has 25 terms");
    }

    #[test]
    fn an_roi_without_contours_is_refused() {
        let rois = r#"{
            "name": "A",
            "color": [1, 2, 3],
            "interpreted_type": "ORGAN",
            "contours": []
        }"#;
        let err = parse(rtstruct_document(rois).as_bytes()).expect_err("no contours");
        assert_eq!(err.at, "rtstruct.rois[0].contours");
    }

    #[test]
    fn a_polygon_with_fewer_than_three_points_is_refused() {
        let rois = roi_json().replace(
            "[[12.5, 40.0], [13.0, 41.5], [11.0, 42.0]]",
            "[[1.0, 2.0], [3.0, 4.0]]",
        );
        let err = parse(rtstruct_document(&rois).as_bytes()).expect_err("two points");
        assert_eq!(err.at, "rtstruct.rois[0].contours[0].points");
        assert!(err.message.contains("at least 3"), "{err}");
    }

    #[test]
    fn a_polygon_over_the_point_cap_is_refused_not_downsampled() {
        // The cap is a refusal: downsampling would move a clinical geometry
        // without telling its author, who is the only party that knows the
        // tolerance.
        let points: Vec<String> = (0..=MAX_CONTOUR_POINTS)
            .map(|index| format!("[{index}.0, 0.0]"))
            .collect();
        let rois = roi_json().replace(
            "[[12.5, 40.0], [13.0, 41.5], [11.0, 42.0]]",
            &format!("[{}]", points.join(", ")),
        );
        let err = parse(rtstruct_document(&rois).as_bytes()).expect_err("over the cap");
        assert_eq!(err.at, "rtstruct.rois[0].contours[0].points");
        assert!(err.message.contains("at most 4096"), "{err}");
        assert!(err.message.contains("downsample in the script"), "{err}");
    }

    #[test]
    fn a_polygon_exactly_at_the_point_cap_is_accepted() {
        // The boundary itself is legal, so the cap cannot be off by one.
        let points: Vec<String> = (0..MAX_CONTOUR_POINTS)
            .map(|index| format!("[{index}.0, 0.0]"))
            .collect();
        let rois = roi_json().replace(
            "[[12.5, 40.0], [13.0, 41.5], [11.0, 42.0]]",
            &format!("[{}]", points.join(", ")),
        );
        let parsed = parse(rtstruct_document(&rois).as_bytes()).expect("parses at the cap");
        assert_eq!(
            parsed.rtstruct.expect("rtstruct").rois[0].contours[0]
                .points
                .len(),
            MAX_CONTOUR_POINTS
        );
    }

    #[test]
    fn a_point_that_is_not_a_pair_is_refused() {
        let rois = roi_json().replace(
            "[[12.5, 40.0], [13.0, 41.5], [11.0, 42.0]]",
            "[[1.0, 2.0, 3.0], [4.0, 5.0], [6.0, 7.0]]",
        );
        let err = parse(rtstruct_document(&rois).as_bytes()).expect_err("triple");
        assert_eq!(err.at, "rtstruct.rois[0].contours[0].points[0]");
    }

    #[test]
    fn an_roi_without_a_color_is_refused() {
        let rois = roi_json().replace(r#""color": [0, 255, 102],"#, "");
        let err = parse(rtstruct_document(&rois).as_bytes()).expect_err("no color");
        assert_eq!(err.at, "rtstruct.rois[0].color");
    }

    #[test]
    fn an_unknown_key_in_the_rtstruct_block_is_an_error() {
        let text = rtstruct_document(roi_json()).replace("\"label\"", "\"labels\"");
        let err = parse(text.as_bytes()).expect_err("unknown key");
        assert_eq!(err.at, "rtstruct.labels");
    }

    #[test]
    fn a_result_carrying_only_an_rtstruct_is_not_empty() {
        let parsed = parse(rtstruct_document(roi_json()).as_bytes()).expect("parses");
        assert!(!parsed.is_empty(), "a structure set is something");
        assert!(parsed.has_rtstruct());
        assert!(!parsed.has_segmentation());
    }

    #[test]
    fn an_rtstruct_block_is_absent_by_default() {
        // Additive: a document that does not use the key is unaffected, and
        // the sixth field is simply None.
        let parsed = parse(EMPTY.as_bytes()).expect("parses");
        assert!(parsed.rtstruct.is_none());
        assert!(!parsed.has_rtstruct());
        assert!(parsed.is_empty());
    }
}
