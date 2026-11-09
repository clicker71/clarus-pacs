// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The LIVE RTSTRUCT fixture, through the encoder and a STRICT walk.
//!
//! This test exists because a live end-to-end run on 22.09 produced a
//! structure set the server refused with HTTP 400 and the comment
//! `Invalid DICOM: Invalid VR in undefined-length element` (FailedSOPSequence
//! (0008,1198), attribute position 272). The previous acceptance parsed the
//! serialized RS with the crate's own SOFT parser, which read around the
//! defect; this test walks the bytes the way a STRICT consumer does and
//! fails on the first thing a validating parser would refuse.
//!
//! The input is the model's own `result.json` from the live run
//! (`rtstruct_live_9_22_2_result.json`: 2 ROIs, 528 contours). Small
//! synthetic fixtures do NOT reproduce the defect - it depends on how much
//! of the structure set the encoder emits - so the test uses the real one.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The Python interpreter to run a cross-check with, or `None` on a machine
/// without one.
///
/// The same search every other suite in this crate uses: `python3` first on
/// Unix ("python" is not installed there by default), `python` first on
/// Windows. Guessing a single name made the cross-check silently skip on the
/// platform whose interpreter is spelled the other way - which is how a
/// pydicom check can look green while never having run.
fn python_interpreter() -> Option<String> {
    let candidates: &[&str] = if cfg!(windows) {
        &["python", "python3"]
    } else {
        &["python3", "python"]
    };
    for candidate in candidates {
        let works = std::process::Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if works {
            return Some((*candidate).to_string());
        }
    }
    None
}

/// PS3.5 Table 6.2-1: every VR a receiving parser must recognise.
const VALID_VR: &[&str] = &[
    "AE", "AS", "AT", "CS", "DA", "DS", "DT", "FL", "FD", "IS", "LO", "LT", "OB", "OD", "OF", "OL",
    "OV", "OW", "PN", "SH", "SL", "SQ", "SS", "ST", "SV", "TM", "UC", "UI", "UL", "UN", "UR", "US",
    "UT", "UV",
];

/// The VRs whose length field is 32-bit in explicit encoding.
const LONG_VR: &[&str] = &[
    "OB", "OD", "OF", "OL", "OV", "OW", "SQ", "UC", "UN", "UR", "UT",
];

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rtstruct_live_9_22_2_result.json")
}

/// One defect a strict parser would find.
#[derive(Debug, PartialEq, Eq)]
enum Defect {
    /// An element header whose VR bytes are not a VR from PS3.5 Table 6.2-1.
    InvalidVr { offset: usize, vr: [u8; 2] },
    /// A header that ran off the end of the buffer.
    TruncatedHeader { offset: usize },
    /// An element body that ran off the end of the buffer.
    TruncatedBody {
        offset: usize,
        vr: &'static str,
        length: u32,
    },
    /// A delimiter without its opener, or a stranded opener at the end.
    UnbalancedDelimiters { offset: usize, depth: isize },
    /// A value whose declared length is ODD. PS3.5 8.1: the Value Field of
    /// every element (Pixel Data included) shall be an even number of bytes,
    /// and the pad belongs to the writer's declared length. A pad byte this
    /// walker had to skip on the reader's side is a writer defect, and this
    /// walker exists to name writer defects.
    OddLength { offset: usize, length: u32 },
}

/// Walk an explicit-VR-little-endian dataset strictly.
///
/// Deliberately NOT the crate's parser: this one refuses to interpret
/// anything it does not recognise, which is what a validator does and what
/// the soft parser does not.
///
/// An undefined-length element is skipped the way
/// `clarus-adapters::dicom_rs::parser::skip_undefined_element` skips it -
/// a counter of every open undefined-length delimiter, raised by an item
/// (FFFE,E000) and by an attribute whose value is undefined length, lowered
/// by (FFFE,E00D) and (FFFE,E0DD), and ended by the first (FFFE,E0DD) seen
/// while nothing is open. That is the path the server validates a STOW on,
/// so the walk here has to agree with it; a walker with a simpler model
/// disagrees on a nested sequence whose last item has already closed and
/// reports a defect the server does not.
fn strict_walk(raw: &[u8]) -> Vec<Defect> {
    let mut defects = Vec::new();
    let mut pos = if raw.len() >= 132 && &raw[128..132] == b"DICM" {
        132
    } else {
        0
    };
    // Every open undefined-length delimiter. The top-level walk is not
    // inside one, so a delimiter seen here has no opener.
    let mut open: isize = 0;

    while pos < raw.len() {
        if pos + 8 > raw.len() {
            defects.push(Defect::TruncatedHeader { offset: pos });
            break;
        }

        let group = u16::from_le_bytes([raw[pos], raw[pos + 1]]);
        let element = u16::from_le_bytes([raw[pos + 2], raw[pos + 3]]);

        if group == 0xFFFE {
            // A delimiter: no VR, a four-byte length.
            match element {
                0xE000 => open += 1,
                0xE00D | 0xE0DD => {
                    open -= 1;
                    if open < 0 {
                        defects.push(Defect::UnbalancedDelimiters {
                            offset: pos,
                            depth: open,
                        });
                        break;
                    }
                }
                _ => {}
            }
            pos += 8;
            continue;
        }

        let vr_bytes = [raw[pos + 4], raw[pos + 5]];

        // THE CHECK THIS TEST EXISTS FOR: the two bytes after the tag must
        // spell a VR. A writer that emits a tag where a length belongs - or
        // that shifts by four bytes inside a sequence - lands here.
        let Ok(vr) = std::str::from_utf8(&vr_bytes) else {
            defects.push(Defect::InvalidVr {
                offset: pos,
                vr: vr_bytes,
            });
            break;
        };
        if !VALID_VR.contains(&vr) {
            defects.push(Defect::InvalidVr {
                offset: pos,
                vr: vr_bytes,
            });
            break;
        }

        let (length, header) = if LONG_VR.contains(&vr) {
            if pos + 12 > raw.len() {
                defects.push(Defect::TruncatedHeader { offset: pos });
                break;
            }
            (
                u32::from_le_bytes([raw[pos + 8], raw[pos + 9], raw[pos + 10], raw[pos + 11]]),
                12usize,
            )
        } else {
            (
                u32::from(u16::from_le_bytes([raw[pos + 6], raw[pos + 7]])),
                8,
            )
        };

        if length == 0xFFFF_FFFF {
            // A legal undefined length: this attribute opens a delimiter of
            // its own, which its own (FFFE,E0DD) will close. It is NOT a
            // fourth delimiter kind - an item opens the same way.
            open += 1;
            pos += header;
            continue;
        }

        let end = pos + header + length as usize;
        if end > raw.len() {
            defects.push(Defect::TruncatedBody {
                offset: pos,
                vr: match vr {
                    "SQ" => "SQ",
                    "OB" => "OB",
                    _ => "??",
                },
                length,
            });
            break;
        }
        // PS3.5 8.1: an even Value Length IS the writer's obligation - the pad
        // byte is part of the length it declares, so a defined length is
        // ALWAYS even. An odd one is therefore not "a length with a pad
        // outside it": it is our writer having declared a number that no
        // conformant reader will agree with. Walking on past a presumed pad
        // would resynchronise against a byte that is not there and hide every
        // later defect behind it, so the walk stops and records the offset.
        if length % 2 == 1 {
            defects.push(Defect::OddLength {
                offset: pos,
                length,
            });
            break;
        }
        pos = end;
    }

    if open != 0 {
        defects.push(Defect::UnbalancedDelimiters {
            offset: pos,
            depth: open,
        });
    }
    defects
}

/// Every VR the serialized structure set uses, for the coverage assertion.
fn vrs_used(raw: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut pos = 132usize;
    while pos + 8 <= raw.len() {
        let group = u16::from_le_bytes([raw[pos], raw[pos + 1]]);
        if group == 0xFFFE {
            // A delimiter carries no VR, so nothing is recorded for it.
            pos += 8;
            continue;
        }
        let Ok(vr) = std::str::from_utf8(&raw[pos + 4..pos + 6]) else {
            break;
        };
        if !VALID_VR.contains(&vr) {
            break;
        }
        out.insert(vr.to_string());
        if LONG_VR.contains(&vr) {
            let length =
                u32::from_le_bytes([raw[pos + 8], raw[pos + 9], raw[pos + 10], raw[pos + 11]]);
            if length == 0xFFFF_FFFF {
                pos += 12;
                continue;
            }
            pos += 12 + length as usize;
        } else {
            let length = usize::from(u16::from_le_bytes([raw[pos + 6], raw[pos + 7]]));
            pos += 8 + length;
        }
    }
    out
}

// -- Building the encoder input from the live fixture --------------------

/// The fixture's model output, read with the crate's own schema parser.
fn live_result() -> clinfer::abi::schema::ResultFile {
    let bytes = std::fs::read(fixture()).expect("the live fixture is present");
    clinfer::abi::schema::parse(&bytes, clinfer::abi::schema::DEFAULT_MAX_RESULT_BYTES)
        .expect("the live result.json is schema-valid")
}

/// The geometry of the live run's 36 CT instances.
///
/// Read with the crate's own JSON parser rather than a second one, so the
/// fixture is consumed the same way the engine consumes DICOM JSON.
struct LiveSources {
    study_uid: String,
    series_uid: String,
    frame_of_reference_uid: String,
    /// Indexed by 0-based position, so frame N is at N-1.
    by_frame: Vec<clinfer::rtstruct::RtSource>,
}

/// A JSON number as an f64, whichever of the two shapes the parser chose.
///
/// The fixture writes `-388.0` and `0.2`, so the parser stores `F64`, but a
/// whole number written without a fraction parses as `U64` and a helper that
/// only accepted `F64` would silently drop the field.
fn number(value: &clinfer::json::Json) -> Option<f64> {
    match value {
        clinfer::json::Json::F64(raw) => Some(*raw),
        clinfer::json::Json::U64(raw) => Some(*raw as f64),
        _ => None,
    }
}

fn live_sources() -> LiveSources {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rtstruct_live_9_22_2_sources.json");
    let bytes = std::fs::read(&path).expect("the live geometry fixture is present");
    let document = clinfer::json::parse(&bytes).expect("the live geometry fixture is valid JSON");

    let text = |key: &str| -> String {
        document
            .get(key)
            .and_then(clinfer::json::Json::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let pair = |value: Option<&clinfer::json::Json>| -> Option<[f64; 2]> {
        let array = value?.as_array()?;
        Some([number(array.first()?)?, number(array.get(1)?)?])
    };
    let triple = |value: Option<&clinfer::json::Json>| -> Option<[f64; 3]> {
        let array = value?.as_array()?;
        Some([
            number(array.first()?)?,
            number(array.get(1)?)?,
            number(array.get(2)?)?,
        ])
    };
    let six = |value: Option<&clinfer::json::Json>| -> Option<[f64; 6]> {
        let array = value?.as_array()?;
        Some([
            number(array.first()?)?,
            number(array.get(1)?)?,
            number(array.get(2)?)?,
            number(array.get(3)?)?,
            number(array.get(4)?)?,
            number(array.get(5)?)?,
        ])
    };

    let sources = document
        .get("sources")
        .and_then(clinfer::json::Json::as_array)
        .expect("the fixture carries a sources array");
    let by_frame = sources
        .iter()
        .map(|entry| {
            let string = |key: &str| -> String {
                entry
                    .get(key)
                    .and_then(clinfer::json::Json::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            clinfer::rtstruct::RtSource {
                sop_class_uid: string("sop_class_uid"),
                sop_instance_uid: string("sop_instance_uid"),
                series_uid: string("series_uid"),
                position: triple(entry.get("position")),
                orientation: six(entry.get("orientation")),
                pixel_spacing: pair(entry.get("pixel_spacing")),
            }
        })
        .collect::<Vec<_>>();

    LiveSources {
        study_uid: text("study_uid"),
        series_uid: text("series_uid"),
        frame_of_reference_uid: text("frame_of_reference_uid"),
        by_frame,
    }
}

/// The source of one 1-based frame, as the worker resolves it.
fn live_source(live: &LiveSources, frame: u32) -> clinfer::rtstruct::RtSource {
    live.by_frame
        .get((frame - 1) as usize)
        .unwrap_or_else(|| panic!("frame {frame} has a source in the live fixture"))
        .clone()
}

/// The encoder input the worker would build from the live result.
///
/// The geometry comes from `rtstruct_live_9_22_2_sources.json`, which holds
/// the values read out of the 36 CT instances that were stored during the
/// live run:
///
/// ```text
///   position      [-388.0, -388.0, 16.0 ... 100.0]   (2.4 mm steps)
///   pixel_spacing [0.2, 0.2]
///   sop_class_uid 1.2.840.10008.5.1.4.1.1.2          (CT Image Storage)
///   frame_of_ref  2.16.840.114421.12451.9835700778.9867236778
/// ```
///
/// This matters and was the first reproduction's mistake. A synthetic
/// FrameMeta (position z = frame index, spacing 1x1, class `.1.1`) is
/// WELL-FORMED, so it drives the encoder through exactly the same branches as
/// the live input and cannot distinguish a geometry-dependent defect from a
/// healthy file. The live series has none of those round numbers, and the
/// class UID is CT Image Storage rather than the Secondary Capture the first
/// fixture used, so every value the encoder writes into (3006,0016) and every
/// contour's z is a different string.
fn identity_from_live() -> clinfer::rtstruct::RtIdentity {
    let result = live_result();
    let rt = result
        .rtstruct
        .expect("the fixture carries an rtstruct block");
    let live = live_sources();

    let mut contour_images: Vec<clinfer::rtstruct::RtSource> = Vec::new();
    let rois = rt
        .rois
        .iter()
        .enumerate()
        .map(|(index, roi)| {
            let contours = roi
                .contours
                .iter()
                .map(|contour| {
                    let slice = live_source(&live, contour.frame);
                    if !contour_images
                        .iter()
                        .any(|seen| seen.sop_instance_uid == slice.sop_instance_uid)
                    {
                        contour_images.push(slice.clone());
                    }
                    // The model's points are (column, row) pixel pairs; the
                    // worker hands them to `to_patient` with the frame's own
                    // geometry. Computing them here rather than at
                    // frame-index z is what makes the contour DATA the live
                    // run wrote.
                    let points = contour
                        .points
                        .iter()
                        .map(|point| clinfer::rtstruct::to_patient(&slice, point[0], point[1]))
                        .collect::<Vec<[f64; 3]>>();
                    clinfer::rtstruct::RtContour {
                        points,
                        source: slice,
                    }
                })
                .collect::<Vec<_>>();
            clinfer::rtstruct::RtRoiEntry {
                number: u16::try_from(index + 1).expect("a small ROI number"),
                roi: clinfer::abi::schema::RtRoi {
                    name: roi.name.clone(),
                    description: roi.description.clone(),
                    color: roi.color,
                    generation_algorithm: roi.generation_algorithm.clone(),
                    interpreted_type: roi.interpreted_type.clone(),
                    contours: Vec::new(),
                },
                contours,
            }
        })
        .collect::<Vec<_>>();

    clinfer::rtstruct::RtIdentity {
        sop_instance_uid: "2.25.1.2.3.4.5".to_string(),
        study_uid: live.study_uid.clone(),
        series_uid: "1.2.3.4.5.1".to_string(),
        frame_of_reference_uid: Some(live.frame_of_reference_uid.clone()),
        label: rt.label.clone(),
        name: rt.name.clone(),
        rois,
        derivation_description: "clinfer live-fixture regression".to_string(),
        patient_name: Some("Fixture^Live".to_string()),
        patient_id: Some("LIVE-1".to_string()),
        patient_id_issuer: None,
        patient_birth_date: None,
        patient_sex: None,
        study_id: None,
        accession_number: None,
        referring_physician: None,
        station_name: None,
        series_description: None,
        source_series: vec![live.series_uid.clone()],
        contour_images,
    }
}

/// The largest number of points a contour may keep, from the BYTE ceiling.
///
/// `ContourData` is a DS of three text values per point, and the worst case
/// width one coordinate can take is `-388.00000000000006` at the formatter's
/// shortest-round-trip decimal - about 19 characters - plus a separator. The
/// script the model runs uses the same conservative figure: at 0.01 mm
/// quantisation a coordinate is at most 9 characters, so 27 bytes per point
/// plus separators fits about 2400 points in 65,534 bytes. The tests keep
/// this many points so the file they build has the SAME order of magnitude
/// as the live one, which is what makes the walk meaningful.
const DECIMATED_POINTS_PER_CONTOUR: usize = 2400;

/// The live identity, with every contour decimated to fit its own VR.
///
/// This is the remedy the owner's decision puts on the producing side: the
/// encoder refuses an over-long contour, and the model's script keeps each
/// polygon inside the byte ceiling. Decimating for the TEST is not an
/// endorsement of losing points; it is how this file proves that the
/// encoder's output is clean once the input is representable, which the
/// refusal test above cannot show on its own.
fn decimated_identity_from_live() -> clinfer::rtstruct::RtIdentity {
    let mut identity = identity_from_live();
    for entry in &mut identity.rois {
        for contour in &mut entry.contours {
            let stride = contour
                .points
                .len()
                .div_ceil(DECIMATED_POINTS_PER_CONTOUR)
                .max(1);
            contour.points = contour
                .points
                .iter()
                .step_by(stride)
                .copied()
                .collect::<Vec<_>>();
        }
    }
    identity
}

// -- Tests ---------------------------------------------------------------

#[test]
fn the_live_fixture_still_has_the_shape_that_reproduced_the_defect() {
    // A guard on the fixture itself: if the file were replaced with a small
    // synthetic one, the regression below would stop proving anything.
    let result = live_result();
    let rt = result.rtstruct.expect("an rtstruct block");
    let contours: usize = rt.rois.iter().map(|roi| roi.contours.len()).sum();
    assert_eq!(rt.rois.len(), 2, "two ROIs (BONE, TEETH)");
    assert_eq!(contours, 528, "528 contours on 36 frames");
}

#[test]
fn the_live_input_is_refused_rather_than_truncated() {
    // THE regression pin, in the form the live run failed: the real fixture
    // goes through the real encoder, and the encoder refuses it. Before the
    // fix it produced 4,047,854 bytes whose first ContourData declared 3806
    // bytes while the real value was 69,342 - a 16-bit wrap - so a parser
    // advanced 62,730 bytes too few and read contour digits as the next
    // element's VR. That is the live HTTP 400 verbatim.
    //
    // The refusal is asserted on the MESSAGE, not only on `is_err`: the
    // point of the owner's decision is that an operator learns which polygon
    // was too big, so a bare error would satisfy the compiler and lose the
    // feature.
    let identity = identity_from_live();
    let error = clinfer::rtstruct::serialize(&identity)
        .expect_err("a contour over the DS byte ceiling is refused");
    assert!(
        error.contains("ContourData") && error.contains("65534"),
        "the refusal names the element and the ceiling: {error}"
    );
    assert!(
        error.contains("point(s)"),
        "the refusal names the offending polygon's size: {error}"
    );
}

#[test]
fn the_serialized_structure_set_passes_a_strict_walk() {
    // The same live fixture, DECIMATED the way the producing script
    // decimates it, must encode and then survive the strict walk. Together
    // with the refusal test above this pins the whole owner decision: the
    // encoder never truncates, and the producer's remedy produces a file a
    // validating consumer accepts.
    let identity = decimated_identity_from_live();
    let raw =
        clinfer::rtstruct::serialize(&identity).expect("the decimated live structure set encodes");

    let defects = strict_walk(&raw);
    if !defects.is_empty() {
        // Dump the bytes for offline inspection: the defect is a byte-layout
        // question, and a hex walk is easier to read outside the test.
        let dump = std::env::temp_dir().join("clarus-rtstruct-live.bin");
        let _ = std::fs::write(&dump, &raw);
        // Report the FIRST delimiter excursion, not only the final total:
        // the depth at the first negative step is the byte offset a fix has
        // to look at.
        let report = delimiter_report(&raw);
        panic!(
            "a strict parser refuses the serialized structure set ({} bytes); \
             first defect: {:?}\n  dumped to {}\n{}",
            raw.len(),
            defects.first(),
            dump.display(),
            report
        );
    }
}

/// Where the delimiter depth first goes negative, and how deep it gets.
fn delimiter_report(raw: &[u8]) -> String {
    let mut pos = 132usize;
    let mut depth: isize = 0;
    let mut first_negative: Option<(usize, isize)> = None;
    let mut opens = 0usize;
    let mut closes = 0usize;
    let mut minima: Vec<(usize, isize)> = Vec::new();

    while pos + 8 <= raw.len() {
        let group = u16::from_le_bytes([raw[pos], raw[pos + 1]]);
        let element = u16::from_le_bytes([raw[pos + 2], raw[pos + 3]]);
        if group == 0xFFFE {
            match element {
                0xE000 => {
                    depth += 1;
                    opens += 1;
                }
                0xE00D | 0xE0DD => {
                    depth -= 1;
                    closes += 1;
                    if depth < 0 && first_negative.is_none() {
                        first_negative = Some((pos, depth));
                    }
                    minima.push((pos, depth));
                }
                _ => {}
            }
            pos += 8;
            continue;
        }
        let Ok(vr) = std::str::from_utf8(&raw[pos + 4..pos + 6]) else {
            break;
        };
        if !VALID_VR.contains(&vr) {
            break;
        }
        if LONG_VR.contains(&vr) {
            let length =
                u32::from_le_bytes([raw[pos + 8], raw[pos + 9], raw[pos + 10], raw[pos + 11]]);
            pos += if length == 0xFFFF_FFFF {
                12
            } else {
                12 + length as usize
            };
        } else {
            let length = usize::from(u16::from_le_bytes([raw[pos + 6], raw[pos + 7]]));
            pos += 8 + length;
        }
    }

    let lowest = minima.iter().map(|(_, d)| *d).min().unwrap_or(0);
    format!(
        "  delimiters: {opens} opened, {closes} closed, final depth {depth}, lowest depth {lowest}\n\
         \x20 first negative at byte offset {:?}",
        first_negative
    )
}

#[test]
fn the_structure_set_uses_the_vrs_the_writer_intends() {
    // A second, softer pin: the walk above stops at the first defect, so
    // this one asserts the SET of VRs the encoder actually emits. A VR here
    // that the writer did not intend - a single letter, a lowercase letter,
    // a stray pair - is visible as a set difference rather than only as a
    // crash.
    let raw = clinfer::rtstruct::serialize(&decimated_identity_from_live())
        .expect("the decimated live structure set encodes");
    let used = vrs_used(&raw);

    for vr in &used {
        assert!(
            VALID_VR.contains(&vr.as_str()),
            "the encoder emitted {vr:?}, which is not a VR"
        );
    }
    // The structure set's own vocabulary, so a change that drops a VR the
    // IOD requires is noticed here rather than at the origin.
    for required in ["SQ", "UI", "CS", "IS", "DS", "SH", "LO", "PN", "DA", "TM"] {
        assert!(
            used.contains(required),
            "the structure set should use {required}; it used {used:?}"
        );
    }
}

#[test]
fn the_structure_set_is_a_file_an_independent_parser_reads() {
    // The walker above is this crate's own code, so it shares any mistaken
    // assumption the encoder has. This test hands the bytes to a parser that
    // was written from the standard by someone else: pydicom, through a
    // subprocess, because the repository does not ship a Python runtime and
    // the assertion has to be able to run without one. When pydicom is
    // absent the test says so and stops - a skipped check must never look
    // like a passed one.
    let raw = clinfer::rtstruct::serialize(&decimated_identity_from_live())
        .expect("the decimated live structure set encodes");
    let path = std::env::temp_dir().join("clarus-rtstruct-live-for-pydicom.dcm");
    std::fs::write(&path, &raw).expect("the dump is writable");

    let script = "\
import sys
try:
    import pydicom
except Exception:
    print('PYDICOM-ABSENT')
    sys.exit(0)
ds = pydicom.dcmread(sys.argv[1])
missing = []
for tag in ((0x3006, 0x0010), (0x3006, 0x0020), (0x3006, 0x0039), (0x3006, 0x0080)):
    if tag not in ds:
        missing.append(tag)
if missing:
    print('MISSING', missing)
    sys.exit(1)
print('OK', len(ds.get('ReferencedFrameOfReferenceSequence', [])),
      len(ds.get('StructureSetROISequence', [])),
      len(ds.get('ROIContourSequence', [])),
      len(ds.get('RTROIObservationsSequence', [])))
";

    let Some(interpreter) = python_interpreter() else {
        eprintln!(
            "NOTE: pydicom cross-check skipped: no python on PATH. \
             The bytes are at {}",
            path.display()
        );
        return;
    };
    let outcome = std::process::Command::new(&interpreter)
        .arg("-c")
        .arg(script)
        .arg(&path)
        .output();

    let Ok(outcome) = outcome else {
        eprintln!(
            "NOTE: pydicom cross-check skipped: {interpreter} could not be started. \
             The bytes are at {}",
            path.display()
        );
        return;
    };

    let stdout = String::from_utf8_lossy(&outcome.stdout);
    if stdout.contains("PYDICOM-ABSENT") {
        eprintln!(
            "NOTE: pydicom cross-check skipped: pydicom is not installed. \
             The bytes are at {}",
            path.display()
        );
        return;
    }
    assert!(
        outcome.status.success(),
        "an independent parser refused the structure set: stdout={stdout} \
         stderr={}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    // The four top-level sequences are the ones a reference chain is read
    // through, and each count is a fact the fixture pins: one frame of
    // reference, two ROIs, and one entry per ROI in the last two.
    assert!(
        stdout.starts_with("OK 1 2 2 2"),
        "the independent parser read the structure set differently: {stdout}"
    );
}
