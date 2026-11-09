// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! `manifest.json` and the workdir layout: what clinfer HANDS to a processor.
//!
//! The workdir is the unit of work, and both the batch path and the resident
//! mode contemplated for later speak it unchanged. Its layout is:
//!
//! ```text
//! <workdir>/
//!   manifest.json      this document
//!   frames/0001.raw    raw pixel bytes, one file per frame, in frame order
//!   frames/0002.raw
//!   ...
//!   result.json        written by the processor, atomically
//! ```
//!
//! The frames are the bytes the origin already returned: clinfer asks for
//! explicit VR little endian and hands the pixel element's own bytes on, so
//! the processor sees exactly what the engine would have seen. Nothing is
//! re-encoded, resized or normalised here - a processor that wants a
//! different shape does it in its own code, where it is visible.
//!
//! The manifest carries the IDENTITY of the run (study UID, frame SOP
//! Instance UIDs, this build's version) because the processor cannot know
//! it and a result that cannot be traced back to its input is not
//! reproducible. It also carries the geometry, which is what lets the
//! processor and clinfer agree about what a valid output looks like.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One frame's entry in the manifest.
///
/// `Eq` is deliberately NOT derived: four of the fields are floats, and
/// claiming a total equivalence over them would be a promise the type cannot
/// keep once a NaN is in play. `PartialEq` is what the tests need.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameEntry {
    /// 1-based position, which is also the file name's number. The mapping
    /// from outputs to frames is by POSITION, so this is the contract's
    /// frame identity.
    pub index: u32,
    /// The file name inside `frames/`, never a path.
    pub file: String,
    /// (0008,0018) of the instance this frame came from: the run's own
    /// record of which image a result belongs to.
    pub sop_uid: String,
    pub width: u32,
    pub height: u32,
    /// Bits allocated, as the instance declared them.
    pub bits: u32,
    /// (0028,0004), e.g. `MONOCHROME2`.
    pub photometric: String,
    /// (0028,0103) Pixel Representation: `true` when the stored samples are
    /// two's complement and must be read signed.
    ///
    /// Published for the same reason the geometry fields are: a producer
    /// that reads a signed source as unsigned sees an inverted-contrast
    /// volume, and on a CBCT stack (which is commonly signed 16-bit) that
    /// is the difference between a mask and an empty mask. Absent in a
    /// manifest means unsigned, which is what every producer assumed before
    /// the key existed - the document stays additive and
    /// `protocol.version` stays 1.
    pub signed: bool,
    /// Size of the frame file clinfer wrote, in bytes.
    ///
    /// Declared so the processor can size its own buffer without stat-ing
    /// the file, and so the audit line reports the exact bytes clinfer
    /// wrote rather than whatever is on disk when the reader looks.
    pub bytes: u64,
    /// (0028,0030) Pixel Spacing, `[row spacing, column spacing]`.
    ///
    /// One of four OPTIONAL geometry fields, all of them PS3.3 C.7.6.2
    /// Image Plane Module attributes. `None` means the instance declared
    /// none - it is NOT a default of 1 mm, and a processor that needs real
    /// millimetres must refuse rather than invent one. These are the values
    /// clinfer already reads to write its own SEG and RS geometry; carrying
    /// them here is publication, not a second reading of the bytes.
    pub pixel_spacing: Option<[f64; 2]>,
    /// (0018,0088) Spacing Between Slices. `None` when the instance
    /// declared none, which for a sparse stack is the difference between a
    /// volume at its real z spacing and one a processor would have to guess.
    pub slice_spacing: Option<f64>,
    /// (0020,0032) Image Position Patient: the first voxel's centre in the
    /// patient coordinate system, `[x, y, z]`.
    pub position: Option<[f64; 3]>,
    /// (0020,0037) Image Orientation (Patient): the direction cosines, six
    /// values, row direction first.
    pub orientation: Option<[f64; 6]>,
}

/// The study context a processor may use as its own model's input.
///
/// Deliberately a small, explicit set rather than "every attribute the
/// origin returned": a processor that needs more can fetch it itself, and a
/// manifest that carried the whole study would silently grow a PHI surface
/// every time the origin added a field.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StudyContext {
    /// (0020,000D).
    pub uid: String,
    /// (0008,1030) Study Description - context an LLM processor wants.
    pub description: Option<String>,
    /// (0008,0061) Modalities in Study.
    pub modalities: Vec<String>,
    /// (0018,0015) Body Part Examined.
    pub body_part: Option<String>,
}

/// The whole `manifest.json`.
///
/// `Eq` is not derived: the frames carry floating-point geometry, so the
/// document is only partially comparable. See [`FrameEntry`].
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub clinfer_version: String,
    pub study: StudyContext,
    /// `result.profile_version` of the rule that matched.
    pub profile_version: u32,
    /// The requester's parameters for this step, flattened from
    /// (0074,1210) Scheduled Processing Parameters Sequence.
    ///
    /// Always rendered, as `{}` when there are none: a script reads
    /// `manifest["parameters"]` unconditionally, so an absent object would
    /// turn "the requester stated nothing" into a lookup failure. The key is
    /// additive - `protocol.version` stays 1.
    pub parameters: Vec<ManifestParameter>,
    pub frames: Vec<FrameEntry>,
}

/// One entry of the manifest's `parameters` object.
///
/// The name is the Concept Name Code Value from the workitem; the value is
/// rendered by its own type ([`ParamValue`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestParameter {
    pub name: String,
    pub value: crate::dicomweb::ParamValue,
}

/// The largest manifest accepted when reading one back (the tests and the
/// forensics reader), in bytes.
///
/// A 500-frame tomo manifest is tens of kilobytes; the bound is generous
/// because a real study has to fit, and finite because a manifest is read
/// back into memory whole.
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

/// The file name of the manifest inside a workdir.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The file name of the result inside a workdir.
pub const RESULT_FILE: &str = "result.json";

/// The subdirectory holding the raw frames.
pub const FRAMES_DIR: &str = "frames";

impl Manifest {
    /// The file name of one frame, `0000.raw`.
    ///
    /// `position` is the frame's ZERO-based position in the study, which is
    /// what [`crate::abi::workdir`] writes; the manifest's own `index` field
    /// is the same number plus one, and that one is the contract's frame
    /// identity. Keeping this function on the position means there is one
    /// place that knows the spelling, and the writer and any reader agree.
    ///
    /// Four digits is not a limit: a study of more than 9999 frames is a real
    /// modality (whole-body PET, 4D CT) and the width simply grows.
    #[must_use]
    pub fn frame_file(position: u32) -> String {
        format!("{position:04}.raw")
    }

    /// Total bytes of every frame, for the audit line.
    ///
    /// Read from the frame entries' declared sizes rather than by stat-ing
    /// the files: the audit line has to report what clinfer WROTE, and a
    /// stat after the fact would report whatever is on disk now, which is a
    /// different question.
    #[must_use]
    pub fn total_frame_bytes(&self) -> u64 {
        self.frames.iter().map(|frame| frame.bytes).sum()
    }

    /// Render the document.
    ///
    /// Built with a `String` rather than a serializer: the crate carries its
    /// own JSON parser and no JSON writer, and the shape is fixed and small.
    /// Every string is escaped by [`write_json_string`], so a study
    /// description containing a quote cannot produce a malformed manifest.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(256 + self.frames.len() * 160);
        out.push_str("{\n");
        out.push_str("  \"protocol\": {\"name\": \"clinfer-abi\", \"version\": 1},\n");
        out.push_str("  \"clinfer_version\": ");
        write_json_string(&mut out, &self.clinfer_version);
        out.push_str(",\n");

        out.push_str("  \"study\": {\n");
        out.push_str("    \"uid\": ");
        write_json_string(&mut out, &self.study.uid);
        out.push_str(",\n");
        out.push_str("    \"description\": ");
        write_json_string(&mut out, self.study.description.as_deref().unwrap_or(""));
        out.push_str(",\n    \"modalities\": [");
        for (at, modality) in self.study.modalities.iter().enumerate() {
            if at > 0 {
                out.push_str(", ");
            }
            write_json_string(&mut out, modality);
        }
        out.push_str("],\n    \"body_part\": ");
        write_json_string(&mut out, self.study.body_part.as_deref().unwrap_or(""));
        out.push_str("\n  },\n");

        let _ = writeln!(out, "  \"profile_version\": {},", self.profile_version);

        out.push_str("  \"parameters\": {");
        for (at, parameter) in self.parameters.iter().enumerate() {
            out.push_str(if at > 0 { ",\n    " } else { "\n    " });
            write_json_string(&mut out, &parameter.name);
            out.push_str(": ");
            write_param_value(&mut out, &parameter.value);
        }
        if self.parameters.is_empty() {
            out.push_str("},\n");
        } else {
            out.push_str("\n  },\n");
        }

        out.push_str("  \"frames\": [\n");
        for (at, frame) in self.frames.iter().enumerate() {
            out.push_str("    {");
            let _ = write!(out, "\"index\": {}", frame.index);
            out.push_str(", \"file\": ");
            write_json_string(&mut out, &frame.file);
            out.push_str(", \"sop_uid\": ");
            write_json_string(&mut out, &frame.sop_uid);
            let _ = write!(
                out,
                ", \"width\": {}, \"height\": {}, \"bits\": {}, \"bytes\": {}, \"photometric\": ",
                frame.width, frame.height, frame.bits, frame.bytes
            );
            write_json_string(&mut out, &frame.photometric);
            // `signed` is written whenever it is TRUE, and omitted when it
            // is false: an unsigned source is the historical default, so
            // omitting it keeps an unsigned manifest byte-identical to what
            // the previous build wrote. A reader that does not know the key
            // treats its absence as unsigned, which is correct for both
            // shapes.
            if frame.signed {
                out.push_str(", \"signed\": true");
            }
            // The geometry keys are written ONLY when the instance declared
            // a value: an absent attribute is a fact about the source, and
            // writing a default here would present an invented geometry as
            // a declared one. `protocol.version` stays 1 because the
            // document is additive - readers are Python scripts doing
            // json.loads, and a key they do not know is a key they ignore.
            if let Some([row, column]) = frame.pixel_spacing
                && row.is_finite()
                && column.is_finite()
            {
                let _ = write!(out, ", \"pixel_spacing\": [{row}, {column}]");
            }
            if let Some(spacing) = frame.slice_spacing
                && spacing.is_finite()
            {
                let _ = write!(out, ", \"slice_spacing\": {spacing}");
            }
            if let Some([x, y, z]) = frame.position
                && x.is_finite()
                && y.is_finite()
                && z.is_finite()
            {
                let _ = write!(out, ", \"position\": [{x}, {y}, {z}]");
            }
            // JSON has no NaN and no Infinity (RFC 8259 6). Writing one puts a
            // token in the document that Python's `json.loads` happily reads
            // back but that no other reader accepts - and the value itself is
            // meaningless geometry. A group is written only when EVERY value in
            // it is finite, because a half-valid vector is not a vector.
            if let Some(values) = frame.orientation
                && values.iter().all(|value| value.is_finite())
            {
                out.push_str(", \"orientation\": [");
                for (at, value) in values.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "{value}");
                }
                out.push(']');
            }
            out.push('}');
            if at + 1 < self.frames.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        out
    }

    /// Where one frame's bytes live, given the workdir.
    #[must_use]
    pub fn frame_path(workdir: &Path, file: &str) -> PathBuf {
        workdir.join(FRAMES_DIR).join(file)
    }
}

/// Write a JSON string literal, escaping what has to be escaped.
///
/// A `String` and not a `&str` writer because the escape set needs a
/// branch per byte; the output is the same `\uXXXX` form any JSON reader
/// accepts, which is what keeps a non-ASCII study description readable in
/// the manifest rather than raw UTF-8 that a reader might decode wrongly.
fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Write one parameter's value in the JSON type its DICOM Value Type maps to.
///
/// TEXT becomes a JSON string, NUMERIC a bare JSON number (a script reads it
/// as a number, so quoting it would force a parse), CODE the string
/// `"scheme:value"` - one token that names a code without inventing an object
/// shape no script was written against - and DATETIME a JSON string, which is
/// the form the requester wrote.
///
/// The number is rendered only when finite: the reader that fills
/// `parameters` refuses a non-finite DS, so an infinite one cannot reach here,
/// and `{}` would be a malformed document if one ever did.
fn write_param_value(out: &mut String, value: &crate::dicomweb::ParamValue) {
    match value {
        crate::dicomweb::ParamValue::Text(text) => write_json_string(out, text),
        crate::dicomweb::ParamValue::Numeric(number) => {
            let rendered = format!("{number}");
            out.push_str(&rendered);
            // std's `{}` drops the fraction of an integral float (`2.0` ->
            // `2`), which a reader parses as an INTEGER - a different JSON
            // kind from the `2.0` the README example publishes. The `.0` is
            // appended only when the render would drop it, so `2.5` stays
            // `2.5` and the e/E exponent spellings keep their own form.
            if !rendered.contains('.') && !rendered.contains('e') && !rendered.contains('E') {
                out.push_str(".0");
            }
        }
        crate::dicomweb::ParamValue::Code { scheme, value } => {
            write_json_string(out, &format!("{scheme}:{value}"));
        }
        crate::dicomweb::ParamValue::DateTime(dt) => write_json_string(out, dt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            clinfer_version: "0.3.0-alpha".to_string(),
            study: StudyContext {
                uid: "1.2.826.0.1.3680043.10.1234.9001".to_string(),
                description: Some("CHEST PA".to_string()),
                modalities: vec!["DX".to_string(), "CR".to_string()],
                body_part: Some("CHEST".to_string()),
            },
            profile_version: 1,
            parameters: Vec::new(),
            frames: vec![
                FrameEntry {
                    index: 1,
                    file: Manifest::frame_file(1),
                    sop_uid: "2.25.1".to_string(),
                    width: 640,
                    height: 640,
                    bits: 8,
                    photometric: "MONOCHROME2".to_string(),
                    bytes: 409_600,
                    signed: false,
                    // The instance declared no geometry, which stays
                    // distinguishable from a declared default all the way
                    // into the document.
                    pixel_spacing: None,
                    slice_spacing: None,
                    position: None,
                    orientation: None,
                },
                FrameEntry {
                    index: 2,
                    file: Manifest::frame_file(2),
                    sop_uid: "2.25.2".to_string(),
                    width: 640,
                    height: 640,
                    bits: 8,
                    photometric: "MONOCHROME2".to_string(),
                    bytes: 409_600,
                    signed: true,
                    pixel_spacing: Some([0.31, 0.31]),
                    slice_spacing: Some(12.0),
                    position: Some([-12.4, -102.0, 6.0]),
                    orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
                },
            ],
        }
    }

    #[test]
    fn a_frame_file_name_is_zero_padded_and_positional() {
        assert_eq!(Manifest::frame_file(1), "0001.raw");
        assert_eq!(Manifest::frame_file(42), "0042.raw");
        assert_eq!(Manifest::frame_file(9999), "9999.raw");
        // Beyond four digits the name simply grows: correctness over
        // prettiness, because the name is the frame's identity.
        assert_eq!(Manifest::frame_file(10_000), "10000.raw");
    }

    #[test]
    fn the_manifest_round_trips_through_the_crate_json_parser() {
        // The document clinfer writes must be one clinfer can read: the
        // manifest is also the forensics artifact an operator inspects, so
        // it is parsed by the same reader the rest of the crate uses.
        let text = manifest().to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("the manifest parses");

        assert_eq!(
            parsed.get("clinfer_version").and_then(|v| v.as_str()),
            Some("0.3.0-alpha")
        );
        assert_eq!(
            parsed
                .get("study")
                .and_then(|s| s.get("uid"))
                .and_then(|u| u.as_str()),
            Some("1.2.826.0.1.3680043.10.1234.9001")
        );
        assert_eq!(
            parsed
                .get("study")
                .and_then(|s| s.get("description"))
                .and_then(|d| d.as_str()),
            Some("CHEST PA")
        );
        assert_eq!(
            parsed.get("profile_version").and_then(|p| p.as_u64()),
            Some(1)
        );

        let frames = parsed
            .get("frames")
            .and_then(|f| f.as_array())
            .expect("frames array");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].get("index").and_then(|i| i.as_u64()), Some(1));
        assert_eq!(text_of(&frames[0], "file").as_deref(), Some("0001.raw"));
        assert_eq!(text_of(&frames[0], "sop_uid").as_deref(), Some("2.25.1"));
        assert_eq!(frames[0].get("width").and_then(|w| w.as_u64()), Some(640));
        assert_eq!(
            frames[0].get("bytes").and_then(|b| b.as_u64()),
            Some(409_600)
        );
        assert_eq!(
            text_of(&frames[0], "photometric").as_deref(),
            Some("MONOCHROME2")
        );
        assert_eq!(text_of(&frames[1], "file").as_deref(), Some("0002.raw"));
    }

    #[test]
    fn declared_geometry_reaches_the_document() {
        // The four Image Plane Module attributes are the ONLY honest source
        // of a nifti affine for a producer, and clinfer already holds them:
        // this test is what keeps them published rather than merely held.
        let text = manifest().to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("the manifest parses");
        let frames = parsed
            .get("frames")
            .and_then(|f| f.as_array())
            .expect("frames array");
        let frame = &frames[1];

        assert_eq!(
            numbers_of(frame, "pixel_spacing"),
            vec![0.31, 0.31],
            "pixel spacing is a two-value array"
        );
        assert_eq!(
            frame.get("slice_spacing").and_then(|v| v.as_f64()),
            Some(12.0),
            "slice spacing is a scalar, not an array"
        );
        assert_eq!(
            numbers_of(frame, "position"),
            vec![-12.4, -102.0, 6.0],
            "position is the first voxel's centre"
        );
        assert_eq!(
            numbers_of(frame, "orientation"),
            vec![1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            "orientation is six direction cosines"
        );
    }

    #[test]
    fn a_signed_source_is_published_and_an_unsigned_one_is_not() {
        // A producer reading a signed source as unsigned sees inverted
        // contrast, which on a CBCT stack is the difference between a mask
        // and an empty mask. The key is written when TRUE and omitted when
        // false, so an unsigned manifest stays byte-identical to what the
        // previous build wrote and an older reader is unaffected.
        let text = manifest().to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("the manifest parses");
        let frames = parsed
            .get("frames")
            .and_then(|f| f.as_array())
            .expect("frames array");

        assert_eq!(
            frames[1].get("signed").and_then(|v| v.as_bool()),
            Some(true),
            "a signed frame declares signed: true"
        );
        assert!(
            frames[0].get("signed").is_none(),
            "an unsigned frame omits the key rather than writing false, so \
             the document is unchanged from the previous build"
        );
        assert_eq!(
            frames[0].get("bits").and_then(|v| v.as_f64()),
            Some(8.0),
            "bits is still published beside it: the pair is what a reader \
             needs to pick a dtype"
        );
    }

    #[test]
    fn an_absent_geometry_attribute_is_omitted_rather_than_defaulted() {
        // Absent means the instance declared nothing. Writing 1.0 there
        // would present an invented geometry as a declared one, and a
        // producer building a volume from it would be wrong in a way no
        // reader could detect - so the key must be MISSING, not zero.
        let text = manifest().to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("the manifest parses");
        let frames = parsed
            .get("frames")
            .and_then(|f| f.as_array())
            .expect("frames array");
        let frame = &frames[0];

        for key in ["pixel_spacing", "slice_spacing", "position", "orientation"] {
            assert!(
                frame.get(key).is_none(),
                "{key} must be omitted when the instance declared none, \
                 not written as a default"
            );
        }
        // The keys are per FRAME, not per manifest: one frame may declare
        // geometry and its neighbour none, which is what a mixed series
        // actually looks like.
        assert!(frames[1].get("pixel_spacing").is_some());
    }

    #[test]
    fn geometry_keys_are_absent_from_a_wholly_geometry_free_manifest() {
        // A study whose instances declared no geometry at all must produce
        // a document with no geometry keys anywhere - the shape an older
        // reader already knows.
        let mut manifest = manifest();
        for frame in &mut manifest.frames {
            frame.pixel_spacing = None;
            frame.slice_spacing = None;
            frame.position = None;
            frame.orientation = None;
        }
        let text = manifest.to_json();
        for key in ["pixel_spacing", "slice_spacing", "position", "orientation"] {
            assert!(
                !text.contains(key),
                "{key} must not appear anywhere in a geometry-free manifest"
            );
        }
        // And the document still parses, so a reader that never looks for
        // geometry is unaffected.
        crate::json::parse(text.as_bytes()).expect("the manifest parses");
    }

    #[test]
    fn a_negative_and_fractional_geometry_value_keeps_its_sign_and_precision() {
        // Patient coordinates are negative as a rule and spacing is
        // fractional; a value mangled into an integer or dropped a sign
        // would place the volume outside the body and no reader would say
        // so. The document is text, so the round trip has to be checked on
        // the text.
        let mut manifest = manifest();
        manifest.frames[1].position = Some([-0.5, -102.25, -1000.0]);
        manifest.frames[1].pixel_spacing = Some([0.0009765625, 0.0009765625]);
        let text = manifest.to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("the manifest parses");
        let frame = &parsed
            .get("frames")
            .and_then(|f| f.as_array())
            .expect("frames array")[1];
        assert_eq!(numbers_of(frame, "position"), vec![-0.5, -102.25, -1000.0]);
        assert_eq!(
            numbers_of(frame, "pixel_spacing"),
            vec![0.0009765625, 0.0009765625]
        );
    }

    /// The numbers of a JSON array field, for the geometry assertions.
    fn numbers_of(parent: &crate::json::Json, key: &str) -> Vec<f64> {
        parent
            .get(key)
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("{key} must be an array"))
            .iter()
            .map(|v| v.as_f64().unwrap_or_else(|| panic!("{key} holds numbers")))
            .collect()
    }

    /// The plain-JSON string at `key` of `parent`.
    ///
    /// The manifest is NOT DICOM JSON: it is a plain document, so the
    /// `{"Value":[..]}`-aware `attribute` accessor does not apply and
    /// these helpers read the bare shape.
    fn text_of(parent: &crate::json::Json, key: &str) -> Option<String> {
        parent.get(key).and_then(|v| v.as_str()).map(str::to_string)
    }

    #[test]
    fn the_protocol_block_names_the_contract_and_its_version() {
        let text = manifest().to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("parses");
        let protocol = parsed.get("protocol").expect("protocol");
        assert_eq!(text_of(protocol, "name").as_deref(), Some("clinfer-abi"));
        assert_eq!(protocol.get("version").and_then(|v| v.as_u64()), Some(1));
    }

    #[test]
    fn study_modalities_are_an_array_not_a_joined_string() {
        // The manifest is JSON, so a multi-valued attribute stays an array:
        // a processor that wants "DX,CR" can join it, and one that wants to
        // test membership does not have to split it.
        let text = manifest().to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("parses");
        let modalities = parsed
            .get("study")
            .and_then(|s| s.get("modalities"))
            .and_then(|m| m.as_array())
            .expect("an array");
        assert_eq!(modalities.len(), 2);
        assert_eq!(modalities[0].as_str(), Some("DX"));
        assert_eq!(modalities[1].as_str(), Some("CR"));
    }

    #[test]
    fn a_quote_in_a_study_description_does_not_break_the_document() {
        // A description is free text from the origin. Unescaped it would
        // produce a manifest no reader can parse - and the failure would
        // surface as "the script crashed", not as "the manifest is
        // malformed", which is the worst kind of bug to debug.
        let mut manifest = manifest();
        manifest.study.description = Some(r#"CHEST "PA" \ back\slash"#.to_string());
        let text = manifest.to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("escaped correctly");
        assert_eq!(
            parsed
                .get("study")
                .and_then(|s| text_of(s, "description"))
                .as_deref(),
            Some(r#"CHEST "PA" \ back\slash"#)
        );
    }

    #[test]
    fn a_control_character_in_a_description_is_escaped() {
        let mut manifest = manifest();
        manifest.study.description = Some("line\nbreak\ttab".to_string());
        let text = manifest.to_json();
        // The raw newline inside the value must be escaped: the document's
        // own line structure is the pretty-printing, so an unescaped
        // control byte would break the lines a reader counts.
        assert!(
            text.contains("\\n") && text.contains("\\t"),
            "control characters are escaped: {text}"
        );
        assert!(
            !text.contains("line\nbreak"),
            "the raw control bytes must not appear"
        );
        let parsed = crate::json::parse(text.as_bytes()).expect("parses");
        assert_eq!(
            parsed
                .get("study")
                .and_then(|s| text_of(s, "description"))
                .as_deref(),
            Some("line\nbreak\ttab")
        );
    }

    #[test]
    fn a_description_absent_from_the_study_becomes_an_empty_string() {
        // JSON has no "absent" for a required-looking key, and a processor
        // reading `.description` should get an empty string it can test,
        // not a missing key it has to guard with a default.
        let mut manifest = manifest();
        manifest.study.description = None;
        manifest.study.body_part = None;
        let text = manifest.to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("parses");
        let study = parsed.get("study").expect("study");
        assert_eq!(text_of(study, "description").as_deref(), Some(""));
        assert_eq!(text_of(study, "body_part").as_deref(), Some(""));
    }

    #[test]
    fn an_empty_frame_list_is_still_a_valid_document() {
        // Not a case a real run produces (a study with no frames is refused
        // earlier), but the writer must not emit malformed JSON for it: an
        // operator reading the artifact should see an empty array, not a
        // syntax error.
        let mut manifest = manifest();
        manifest.frames.clear();
        let text = manifest.to_json();
        let parsed = crate::json::parse(text.as_bytes()).expect("parses");
        assert_eq!(
            parsed
                .get("frames")
                .and_then(|f| f.as_array())
                .map(<[_]>::len),
            Some(0)
        );
    }

    #[test]
    fn the_total_frame_bytes_are_the_declared_sizes() {
        // The audit line reports what clinfer wrote. Summing the declared
        // sizes answers that question; stat-ing the files afterwards would
        // answer a different one.
        let manifest = manifest();
        assert_eq!(manifest.total_frame_bytes(), 819_200);
        assert_eq!(manifest.frames[0].bytes, 409_600);
    }

    #[test]
    fn a_frame_path_is_inside_the_frames_directory() {
        let workdir = Path::new("/tmp/work");
        let path = Manifest::frame_path(workdir, "0001.raw");
        assert!(path.starts_with(workdir));
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("0001.raw"));
        assert_eq!(
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some("frames")
        );
    }

    /// A manifest with parameters renders them as a JSON object, and the
    /// three value types keep their JSON kind: TEXT a string, NUMERIC a bare
    /// number, CODE the `scheme:value` string.
    #[test]
    fn the_parameters_object_renders_every_value_type() {
        let mut manifest = manifest();
        manifest.parameters = vec![
            ManifestParameter {
                name: "ALGORITHM".to_string(),
                value: crate::dicomweb::ParamValue::Text("clahe".to_string()),
            },
            ManifestParameter {
                name: "CLIP_LIMIT".to_string(),
                value: crate::dicomweb::ParamValue::Numeric(2.5),
            },
            ManifestParameter {
                name: "BODY_SITE".to_string(),
                value: crate::dicomweb::ParamValue::Code {
                    scheme: "SRT".to_string(),
                    value: "T-D0050".to_string(),
                },
            },
            ManifestParameter {
                name: "SLICE_TIME".to_string(),
                value: crate::dicomweb::ParamValue::DateTime("20261109101530.123+0300".to_string()),
            },
        ];
        let parsed = crate::json::parse(manifest.to_json().as_bytes()).expect("valid JSON");
        let parameters = parsed.get("parameters").expect("parameters object");

        assert_eq!(text_of(parameters, "ALGORITHM").as_deref(), Some("clahe"));
        assert_eq!(
            parameters
                .get("CLIP_LIMIT")
                .and_then(crate::json::Json::as_f64),
            Some(2.5),
            "a NUMERIC parameter is a JSON number, not a string"
        );
        assert_eq!(
            text_of(parameters, "BODY_SITE").as_deref(),
            Some("SRT:T-D0050")
        );
        assert_eq!(
            text_of(parameters, "SLICE_TIME").as_deref(),
            Some("20261109101530.123+0300"),
            "a DATETIME parameter is the string the requester wrote"
        );

        // `profile_version` comes first and `frames` still follows the new
        // key, so a reader seeking either keeps finding it where it looked.
        let text = manifest.to_json();
        let profile = text.find("\"profile_version\"").expect("profile_version");
        let parameters_at = text.find("\"parameters\"").expect("parameters");
        let frames = text.find("\"frames\"").expect("frames");
        assert!(profile < parameters_at, "parameters follow profile_version");
        assert!(parameters_at < frames, "frames follow parameters");
    }

    /// A manifest with no parameters still carries the key, as `{}`: a script
    /// reads `manifest["parameters"]` unconditionally, and an absent object
    /// would turn "the requester stated nothing" into a lookup failure.
    #[test]
    fn an_empty_parameter_list_renders_an_empty_object() {
        let manifest = manifest();
        let text = manifest.to_json();
        assert!(
            text.contains("\"parameters\": {}"),
            "the key is always present: {text}"
        );
        let parsed = crate::json::parse(text.as_bytes()).expect("valid JSON");
        assert!(
            parsed.get("parameters").is_some(),
            "the empty object parses back as an object"
        );
    }

    /// An integral NUMERIC keeps its `.0` in the manifest.
    ///
    /// std's `{}` renders `2.0` as `2`, which a reader parses as an INTEGER
    /// and hands the processor a different JSON kind from the `2.0` the
    /// README example publishes. The document is text, so the round trip has
    /// to be checked on the text, not on a re-parsed float.
    #[test]
    fn an_integral_numeric_parameter_keeps_its_fraction() {
        let mut manifest = manifest();
        manifest.parameters = vec![ManifestParameter {
            name: "CLIP_LIMIT".to_string(),
            value: crate::dicomweb::ParamValue::Numeric(2.0),
        }];
        let text = manifest.to_json();
        assert!(
            text.contains("\"CLIP_LIMIT\": 2.0"),
            "an integral float keeps its fraction: {text}"
        );

        // A fractional value keeps its own spelling: the suffix is added only
        // when the render would drop it.
        manifest.parameters[0].value = crate::dicomweb::ParamValue::Numeric(2.5);
        let text = manifest.to_json();
        assert!(
            text.contains("\"CLIP_LIMIT\": 2.5"),
            "2.5 stays 2.5: {text}"
        );
    }
}
