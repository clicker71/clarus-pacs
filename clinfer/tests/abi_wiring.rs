// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The external path end to end: a script model, a real child process, and
//! the objects clinfer stores from its `result.json`.
//!
//! `abi_seam.rs` exercises the spawn seam directly. THIS file drives the
//! worker, so what is covered is the wiring: `pr{boxes}` to the GSPS
//! encoder, `sr{text}` to the SR encoder, the workdir cleanup policy, and
//! the frame-count check that keeps a box from landing on an image the
//! study does not have.
//!
//! The fake origin answers the requests the worker makes, and STOWed
//! objects are captured, so a test asserts on the bytes clinfer actually
//! stored rather than on an intention.

#[path = "support/fake_origin.rs"]
mod fake_origin;

use clinfer::abi::workdir;
use clinfer::receiver::Counters;
use clinfer::worker;
use fake_origin::{Answer, FakeOrigin};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

// -- The interpreter, if the machine has one --------------------------

fn interpreter() -> Option<String> {
    let candidates: &[&str] = if cfg!(windows) {
        &["python", "python3"]
    } else {
        &["python3", "python"]
    };
    for candidate in candidates {
        let works = Command::new(candidate)
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

macro_rules! require_python {
    () => {
        match interpreter() {
            Some(found) => found,
            None => {
                eprintln!("skipping: no python interpreter on this machine");
                return;
            }
        }
    };
}

// -- Scratch space -----------------------------------------------------

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "clinfer-abi-wire-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, body).expect("script");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// -- A source instance the origin can serve ----------------------------

/// A minimal explicit-VR-little-endian instance, 2x2 8-bit.
///
/// Built by hand rather than by a DICOM library: clinfer has no such
/// dependency, and the tags the external path reads are few enough that the
/// bytes are clearer than an encoder call. Lengths are computed from the
/// values, so an edit cannot leave a length lying.
///
/// There is NO file meta group, and that is deliberate: `pixels::find_tag`
/// walks from byte 132 (past the preamble and the `DICM` magic), which is
/// where a dataset with no meta group starts. The reader is the subject
/// here, so the fixture is built to the shape it accepts - a meta group
/// would have to be skipped and this reader does not skip one.
fn source_instance(sop_instance_uid: &str, pixels: &[u8; 4]) -> Vec<u8> {
    let mut out = vec![0u8; 128];
    out.extend_from_slice(b"DICM");
    out.extend_from_slice(&element(0x0008, 0x0005, b"CS", b"ISO_IR 192"));
    out.extend_from_slice(&element(
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.1.1\0",
    ));
    // A UID is padded to even length with NUL when it is odd, and these are.
    let padded = if sop_instance_uid.len().is_multiple_of(2) {
        sop_instance_uid.to_string()
    } else {
        format!("{sop_instance_uid}\0")
    };
    out.extend_from_slice(&element(0x0008, 0x0018, b"UI", padded.as_bytes()));
    out.extend_from_slice(&element(0x0008, 0x0060, b"CS", b"DX"));
    out.extend_from_slice(&element(0x0028, 0x0002, b"US", &1u16.to_le_bytes()));
    out.extend_from_slice(&element(0x0028, 0x0004, b"CS", b"MONOCHROME2"));
    out.extend_from_slice(&element(0x0028, 0x0010, b"US", &2u16.to_le_bytes()));
    out.extend_from_slice(&element(0x0028, 0x0011, b"US", &2u16.to_le_bytes()));
    out.extend_from_slice(&element(0x0028, 0x0100, b"US", &8u16.to_le_bytes()));
    out.extend_from_slice(&element(0x0028, 0x0101, b"US", &8u16.to_le_bytes()));
    out.extend_from_slice(&element(0x0028, 0x0103, b"US", &0u16.to_le_bytes()));
    out.extend_from_slice(&element(0x7FE0, 0x0010, b"OW", pixels));
    out
}

/// The same fixture, with a Frame of Reference (0020,0052) declared.
///
/// The plain `source_instance` above declares none, which is legal - the
/// attribute is Type 3 in many image IODs - and that is the case the STL
/// writer has to substitute for. This variant is the OTHER case: a source
/// that does define one, whose value the mesh must copy.
fn source_instance_with_frame_of_reference(
    sop_instance_uid: &str,
    frame_of_reference: &str,
    pixels: &[u8; 4],
) -> Vec<u8> {
    let mut out = source_instance(sop_instance_uid, pixels);
    let padded = if frame_of_reference.len().is_multiple_of(2) {
        frame_of_reference.to_string()
    } else {
        format!("{frame_of_reference}\0")
    };
    out.extend_from_slice(&element(0x0020, 0x0052, b"UI", padded.as_bytes()));
    out
}

/// The same fixture, with the geometry a segmentation has to copy: Pixel
/// Spacing, Spacing Between Slices and an Image Position Patient.
///
/// The tags are appended after Pixel Data rather than inserted in tag
/// order. The reader scans by tag and does not require an ordered dataset,
/// and appending keeps this fixture a two-line change to the plain one.
/// Each DS value is padded to an even length with the trailing space the
/// value representation's own padding rule allows.
fn source_instance_with_geometry(
    sop_instance_uid: &str,
    spacing: &[u8],
    slice_spacing: f64,
    position: &[u8],
    pixels: &[u8; 4],
) -> Vec<u8> {
    let mut out = source_instance(sop_instance_uid, pixels);
    out.extend_from_slice(&element(0x0028, 0x0030, b"DS", &ds_pad(spacing)));
    out.extend_from_slice(&element(
        0x0018,
        0x0088,
        b"DS",
        &ds_pad(format!("{slice_spacing}").as_bytes()),
    ));
    out.extend_from_slice(&element(0x0020, 0x0032, b"DS", &ds_pad(position)));
    out
}

/// A DS value padded to an even length with a trailing space.
fn ds_pad(value: &[u8]) -> Vec<u8> {
    let mut padded = value.to_vec();
    if padded.len() % 2 == 1 {
        padded.push(b' ');
    }
    padded
}

/// One dataset element, in the form the reader expects.
fn element(group: u16, element: u16, vr: &[u8; 2], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&group.to_le_bytes());
    out.extend_from_slice(&element.to_le_bytes());
    out.extend_from_slice(vr);
    // The long VRs carry two reserved bytes and a 32-bit length; the
    // reader's `element_header` decides on the VR, so a long VR written
    // in the short form is read as a length at the wrong offset.
    if matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN") {
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    out.extend_from_slice(value);
    out
}

// -- The origin and the configuration ----------------------------------

fn origin(fake: &FakeOrigin) -> clinfer::dicomweb::Dicomweb {
    let client = clinfer::http::Client::new(&fake.base_url(), "clinfer").expect("client");
    clinfer::dicomweb::Dicomweb::new(client)
}

/// A dispatch topology with the configuration's model threads STARTED.
///
/// These tests exercise whole cycles, so the rule they install has to reach
/// a real model thread: a topology with no threads would report a per-rule
/// failure for every rule, which is the honest answer for a configuration
/// whose models have no threads but a wrong answer here.
///
/// The caller must keep the returned `ModelThreads` alive for the duration
/// of the cycle - dropping it stops the threads - and join it afterwards so
/// the process does not exit with a child in flight.
fn topology(
    origin: &clinfer::dicomweb::Dicomweb,
    conf: &clinfer::conf::Conf,
    counters: &std::sync::Arc<Counters>,
) -> (worker::Topology, worker::ModelThreads) {
    let threads = worker::Topology::spawn(
        conf,
        std::sync::Arc::new(topology_origin(origin)),
        std::sync::Arc::clone(counters),
    );
    (threads.topology(), threads)
}

/// A second handle on the same origin, for the calls that need an `Arc`.
fn topology_origin(origin: &clinfer::dicomweb::Dicomweb) -> clinfer::dicomweb::Dicomweb {
    let (host, port, base) = origin.client().target();
    let client = clinfer::http::Client::new(&format!("http://{host}:{port}{base}"), "clinfer")
        .expect("client");
    clinfer::dicomweb::Dicomweb::new(client)
}

fn workitem_json(study_uid: &str) -> String {
    format!(
        r#"{{"00080018":{{"vr":"UI","Value":["1.2.3"]}},
             "00741000":{{"vr":"CS","Value":["SCHEDULED"]}},
             "00404021":{{"vr":"SQ","Value":[{{"0040E020":{{"vr":"CS","Value":["DICOM"]}},
                                                "0020000D":{{"vr":"UI","Value":["{study_uid}"]}}}}]}}}}"#
    )
}

fn study_json() -> String {
    r#"[{"00080061":{"vr":"CS","Value":["DX"]},
        "00081030":{"vr":"LO","Value":["External fixture"]},
        "00180015":{"vr":"CS","Value":["CHEST"]}}]"#
        .to_string()
}

fn series_json() -> String {
    r#"[{"0020000E":{"vr":"UI","Value":["1.2.3.4.5"]},
        "00080060":{"vr":"CS","Value":["DX"]},
        "0008103E":{"vr":"LO","Value":["PA"]}}]"#
        .to_string()
}

fn instances_json(uids: &[&str]) -> String {
    let rows: Vec<String> = uids
        .iter()
        .map(|uid| {
            format!(
                r#"{{"00080018":{{"vr":"UI","Value":["{uid}"]}},
                    "0020000E":{{"vr":"UI","Value":["1.2.3.4.5"]}}}}"#
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// A configuration with ONE external model and one rule that always matches.
///
/// Written as a file rather than built in memory because that is how a
/// deployment configures a processor, and it is the path being tested.
fn external_conf(dir: &Path, script: &Path, interpreter: &str) -> clinfer::conf::Conf {
    let text = format!(
        r#"server:
  base_url: "http://127.0.0.1:1/dicomweb"
  identity_user: "clinfer"
  listen: "127.0.0.1:18124"
engine:
  cache_dir: "{cache}"
models:
  proc:
    script: "{script}"
    interpreter: "{interpreter}"
    timeout_secs: 60
rules:
  - match:
      modality: [DX]
    model: "proc"
    result:
      kind: pr
      profile_version: 3
default_rule: cancel
"#,
        cache = dir.join("cache").display(),
        script = script.display(),
        interpreter = interpreter,
    );
    let path = dir.join("clinfer.conf");
    std::fs::write(&path, text).expect("config");
    clinfer::conf::load(&path).expect("the configuration parses")
}

/// A configuration whose single rule produces an `rtstruct`.
fn rtstruct_conf(dir: &Path, script: &Path, interpreter: &str) -> clinfer::conf::Conf {
    let text = format!(
        r#"server:
  base_url: "http://127.0.0.1:1/dicomweb"
  identity_user: "clinfer"
  listen: "127.0.0.1:18125"
engine:
  cache_dir: "{cache}"
models:
  proc:
    script: "{script}"
    interpreter: "{interpreter}"
    timeout_secs: 60
rules:
  - match:
      modality: [DX]
    model: "proc"
    result:
      kind: rtstruct
      profile_version: 3
default_rule: cancel
"#,
        cache = dir.join("cache").display(),
        script = script.display(),
        interpreter = interpreter,
    );
    let path = dir.join("clinfer-rtstruct.conf");
    std::fs::write(&path, text).expect("config");
    clinfer::conf::load(&path).expect("the configuration parses")
}

// -- A processor that returns what the test wants ----------------------

/// A script that writes a `result.json` with the given body verbatim.
fn result_script(body: &str) -> String {
    format!(
        r#"
import pathlib, sys
workdir = pathlib.Path(sys.argv[1])
body = {body:?}
(workdir / "result.json.tmp").write_text(body)
(workdir / "result.json.tmp").rename(workdir / "result.json")
"#,
        body = body
    )
}

/// A script that writes mask files and then a `result.json`.
///
/// The masks come first because they are the payload the worker reads: a
/// result naming a file that is not there is a different test.
fn seg_script(masks: &[(&str, Vec<u8>)], body: &str) -> String {
    let mut writes = String::new();
    for (name, bytes) in masks {
        writes.push_str(&format!(
            "(workdir / {name:?}).write_bytes(bytes({bytes:?}))\n"
        ));
    }
    format!(
        r#"
import pathlib, sys
workdir = pathlib.Path(sys.argv[1])
{writes}(workdir / "result.json.tmp").write_text({body:?})
(workdir / "result.json.tmp").rename(workdir / "result.json")
"#
    )
}

/// The `seg` result body for one segment and one mask per frame.
fn seg_body(masks: &[(&str, u32)]) -> String {
    let files: Vec<String> = masks
        .iter()
        .map(|(name, frame)| format!(r#"{{"file": "{name}", "frame": {frame}}}"#))
        .collect();
    format!(
        r#"{{"protocol": {{"name": "clinfer-abi", "version": 1}},
             "producer": {{"script": "SEG-threshold.py"}},
             "seg": {{"segments": [{{"label": "THRESHOLD",
                 "category": {{"code": "M-01000", "scheme": "SRT",
                               "meaning": "Morphologically Altered Structure"}},
                 "type": {{"code": "M-01000", "scheme": "SRT",
                           "meaning": "Morphologically Altered Structure"}},
                 "color": [255, 0, 0]}}],
               "files": [{}]}}}}"#,
        files.join(",")
    )
}

/// The `seg` result body for TWO segments, one frame, one mask file.
///
/// The file holds both bit planes one after the other: the schema ties a
/// file to a FRAME, and the worker splits it into one frame per segment
/// (C.8.20.3.1: a BINARY frame is one bit plane of one segment).
fn two_segment_body(mask: &str) -> String {
    format!(
        r#"{{"protocol": {{"name": "clinfer-abi", "version": 1}},
             "producer": {{"script": "SEG-DX-chest.py"}},
             "seg": {{"segments": [
                 {{"label": "RIGHT_LUNG",
                   "category": {{"code": "T-D0050", "scheme": "SRT", "meaning": "Anatomical Structure"}},
                   "type": {{"code": "T-28000", "scheme": "SRT", "meaning": "Lung"}}}},
                 {{"label": "HEART",
                   "category": {{"code": "T-D0050", "scheme": "SRT", "meaning": "Anatomical Structure"}},
                   "type": {{"code": "T-32000", "scheme": "SRT", "meaning": "Heart"}},
                   "color": [0, 200, 0]}}],
               "files": [{{"file": "{mask}", "frame": 1}}]}}}}"#
    )
}

/// One ROI for [`rtstruct_body`]: its label, its interpreted type, and its
/// contours as `(frame, points)`.
type RoiSpec<'a> = (&'a str, &'a str, &'a [(u32, &'a [[f64; 2]])]);

/// The `rtstruct` result body for one ROI with one contour per entry.
///
/// `contours` is `(frame, points)` where `points` is a list of
/// `[column, row]` pixel pairs - the ABI's own coordinate space. A contour
/// carries NO name: `parse_contour` accepts only `frame` and `points`, and
/// an unknown key is a hard schema error.
fn rtstruct_body(rois: &[RoiSpec<'_>]) -> String {
    let mut rendered = Vec::new();
    for (label, interpreted_type, contours) in rois {
        // The name is separate from the label so the schema's Type 1 / Type 3
        // split is visible: a label always exists, a description need not.
        let contour_json: Vec<String> = contours
            .iter()
            .map(|(frame, points)| {
                let pairs: Vec<String> = points
                    .iter()
                    .map(|pair| format!("[{}, {}]", pair[0], pair[1]))
                    .collect();
                format!(r#"{{"frame": {frame}, "points": [{}]}}"#, pairs.join(", "),)
            })
            .collect();
        rendered.push(format!(
            r#"{{"name": "{label}", "description": "auto",
                 "color": [255, 0, 0], "generation_algorithm": "AUTOMATIC",
                 "interpreted_type": "{interpreted_type}",
                 "contours": [{}]}}"#,
            contour_json.join(", "),
        ));
    }
    format!(
        r#"{{"protocol": {{"name": "clinfer-abi", "version": 1}},
             "producer": {{"script": "contour.py"}},
             "rtstruct": {{"label": "AI LUNG", "name": "AI Structure Set",
                           "rois": [{}]}}}}"#,
        rendered.join(", ")
    )
}

/// The result body for a set of boxes, one per frame.
fn boxes_body(entries: &[(&str, u32, f64)]) -> String {
    let boxes: Vec<String> = entries
        .iter()
        .map(|(label, frame, confidence)| {
            format!(
                r#"{{"label": "{label}", "frame": {frame}, "confidence": {confidence},
                     "x1": 1, "y1": 1, "x2": 2, "y2": 2}}"#
            )
        })
        .collect();
    format!(
        r#"{{"protocol": {{"name": "clinfer-abi", "version": 1}},
             "producer": {{"script": "boxes.py", "model": "yolo"}},
             "pr": {{"boxes": [{}]}}}}"#,
        boxes.join(",")
    )
}

/// The origin answers one workitem takes, in the order the worker asks.
///
/// The ORDER is the contract this file pins: read the workitem, read the
/// study for the rule, claim, read the study again for the result object's
/// identity, walk the series, walk each series' instances, retrieve each
/// instance's bytes. Getting it wrong in a test produces a "not valid
/// UTF-8" parse failure rather than a clear message, which is why it is
/// built in one place.
struct Answers {
    items: Vec<Answer>,
}

impl Answers {
    /// `frames` are the instances to serve, in order, all in ONE series:
    /// the walk is one series listing, then one instance listing that names
    /// every instance, then one retrieve per instance. Repeating the series
    /// listing per frame would make the worker see one instance N times.
    fn new(frames: &[Vec<u8>], instance_uids: &[&str]) -> Self {
        let mut items = vec![
            Answer::ok(workitem_json("9.9.9")),        // (1) the workitem
            Answer::ok(study_json()),                  // (2) the study, for the rule
            Answer::ok("{}"),                          // (3) the claim
            Answer::ok(study_json()),                  // (4) the study, for identity
            Answer::ok(series_json()),                 // (5) the study's series
            Answer::ok(instances_json(instance_uids)), // (6) its instances
        ];
        for frame in frames {
            items.push(Answer::multipart_dicom(frame));
        }
        Self { items }
    }

    /// The STOWs: one answer per object clinfer stores.
    fn then(mut self, mut rest: Vec<Answer>) -> Vec<Answer> {
        let mut all = std::mem::take(&mut self.items);
        all.append(&mut rest);
        all
    }
}

/// The tail of a successful cycle: one STOW answer per stored object, then
/// the COMPLETED transition.
fn success_tail(stored: usize) -> Vec<Answer> {
    let mut tail: Vec<Answer> = (0..stored).map(|_| Answer::ok("{}")).collect();
    tail.push(Answer::ok("{}")); // the COMPLETED transition
    tail
}

/// The tail of a cycle that stores `stored` objects and then FAILS: the
/// cancel is the answer to every store after the last successful one, so
/// the run stops mid-store with a non-empty workdir to look at.
fn failure_tail(stored: usize) -> Vec<Answer> {
    let mut tail: Vec<Answer> = (0..stored).map(|_| Answer::ok("{}")).collect();
    tail.push(Answer::status(500));
    tail
}

// -- Driving one workitem ---------------------------------------------

/// The STOWed object bodies the origin received, in order.
///
/// Both framing spellings are read. A single-part `application/dicom` body
/// IS the object; a `multipart/related; type="application/dicom"` body
/// carries it inside a part, which `unparted_body` extracts the way a server
/// does on receipt. A filter that matched only the single-part spelling
/// would report "nothing was stored" for a STOW the origin received, which is
/// a test failing on its own reader rather than on the client.
fn stored_objects(fake: &FakeOrigin) -> Vec<Vec<u8>> {
    fake.requests()
        .into_iter()
        .filter(|request| {
            request.method == "POST"
                && request.header("content-type").is_some_and(|value| {
                    value.starts_with("application/dicom")
                        || (value.starts_with("multipart/related")
                            && value.contains("type=\"application/dicom\""))
                })
        })
        // `unparted_body`, not `body_bytes`: a DICOM object is binary, the
        // lossy-text accessor replaces every byte above 0x7F with U+FFFD, and
        // a multipart body would arrive wrapped in boundary markers that
        // parse as garbage rather than failing.
        .filter_map(|request| request.unparted_body())
        .collect()
}

/// The offset of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read one tag out of a stored object's dataset.
fn tag_text(object: &[u8], group: u16, element: u16) -> Option<String> {
    let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
    let at = find_subslice(object, &needle)?;
    let vr = &object[at + 4..at + 6];
    let (length, head) = if matches!(vr, b"OB" | b"OW" | b"SQ" | b"UT" | b"UN") {
        (
            u32::from_le_bytes([
                object[at + 8],
                object[at + 9],
                object[at + 10],
                object[at + 11],
            ]) as usize,
            12,
        )
    } else {
        (
            u16::from_le_bytes([object[at + 6], object[at + 7]]) as usize,
            8,
        )
    };
    if length == 0 || length > 4096 {
        return None;
    }
    let value = object.get(at + head..at + head + length)?;
    Some(
        String::from_utf8_lossy(value)
            .trim_end_matches('\0')
            .trim()
            .to_string(),
    )
}

/// Read one tag as a pair of 32-bit signed integers, the way the
/// presentation state writes (0070,0052) and (0070,0053).
///
/// The VR is SL and the value is a row\column pair, so the two numbers are
/// the two halves of an eight-byte value. The four bytes of a tag can also
/// occur inside a UID's ASCII digits, so a candidate is taken only when it
/// is a short-form element declaring exactly eight bytes: an ASCII run
/// never spells `SL` followed by a length of 8 by accident.
fn tag_sl_pair(object: &[u8], group: u16, element: u16) -> Option<(i32, i32)> {
    let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
    let mut from = 0;
    while let Some(at) = find_subslice(&object[from..], &needle) {
        let at = from + at;
        from = at + 2;
        if object.get(at + 4..at + 6) != Some(b"SL") {
            continue;
        }
        if u16::from_le_bytes([object[at + 6], object[at + 7]]) != 8 {
            continue;
        }
        let value = object.get(at + 8..at + 16)?;
        let first = i32::from_le_bytes([value[0], value[1], value[2], value[3]]);
        let second = i32::from_le_bytes([value[4], value[5], value[6], value[7]]);
        return Some((first, second));
    }
    None
}

/// Every SOP Instance UID in a stored object.
fn sop_instance_uid(object: &[u8]) -> Option<String> {
    // (0008,0018) is the dataset copy; the file meta has (0002,0003) but
    // the dataset one is what the origin indexes on.
    tag_text(object, 0x0008, 0x0018)
}

fn sop_class_uid(object: &[u8]) -> Option<String> {
    tag_text(object, 0x0008, 0x0016)
}

/// One stored object's Pixel Data, read as the OB/OW element it is.
///
/// The payload is binary and its length is the point, so this returns the
/// bytes rather than a string: a test that compared text here would be
/// comparing a lossy decode of a bit-packed mask. The 1-bit SEG payload
/// is OW (word-packed bits); other writers emit OB.
fn pixel_data(object: &[u8]) -> Option<&[u8]> {
    let at = find_subslice(object, &[0xE0, 0x7F, 0x10, 0x00])?;
    assert!(
        matches!(&object[at + 4..at + 6], b"OB" | b"OW"),
        "pixel data is OB or OW"
    );
    let length = u32::from_le_bytes([
        object[at + 8],
        object[at + 9],
        object[at + 10],
        object[at + 11],
    ]) as usize;
    object.get(at + 12..at + 12 + length)
}

/// Every stored object of one SOP class.
fn objects_of_class<'a>(stored: &'a [Vec<u8>], class: &str) -> Vec<&'a Vec<u8>> {
    stored
        .iter()
        .filter(|object| sop_class_uid(object).is_some_and(|uid| uid == class))
        .collect()
}

// -- Tests: pr --------------------------------------------------------

#[test]
fn one_overlay_is_stored_per_frame_that_has_boxes() {
    let python = require_python!();
    let scratch = Scratch::new("pr-frames");
    let script = scratch.script(
        "boxes.py",
        &result_script(&boxes_body(&[("A", 1, 0.9), ("B", 2, 0.8), ("C", 2, 0.7)])),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let first = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let second = source_instance("1.2.3.4.5.2", &[5, 6, 7, 8]);
    // Two frames, two boxes on the second: two overlays stored, then the
    // COMPLETED transition.
    let fake = FakeOrigin::start(
        Answers::new(&[first, second], &["1.2.3.4.5.1", "1.2.3.4.5.2"]).then(success_tail(2)),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(
        matches!(outcome, worker::Processed::Claimed),
        "expected a completed cycle, got {outcome:?}"
    );

    let stored = stored_objects(&fake);
    let overlays: Vec<&Vec<u8>> = stored
        .iter()
        .filter(|object| {
            sop_class_uid(object).is_some_and(|uid| uid == "1.2.840.10008.5.1.4.1.1.11.1")
        })
        .collect();
    assert_eq!(
        overlays.len(),
        2,
        "one GSPS per frame with boxes, not three"
    );

    // Each overlay names the frame it is bound to: one PR, one instance.
    let referenced: Vec<String> = overlays
        .iter()
        .filter_map(|object| tag_text(object, 0x0008, 0x1155))
        .collect();
    assert!(
        referenced.iter().any(|uid| uid == "1.2.3.4.5.1"),
        "{referenced:?}"
    );
    assert!(
        referenced.iter().any(|uid| uid == "1.2.3.4.5.2"),
        "{referenced:?}"
    );

    // And the two vertices for frame 2 are in the object that names it:
    // the boxes were grouped, not merely counted.
    let second_overlay = overlays
        .iter()
        .find(|object| tag_text(object, 0x0008, 0x1155).as_deref() == Some("1.2.3.4.5.2"))
        .expect("the frame-2 overlay");
    let text = String::from_utf8_lossy(second_overlay);
    assert!(
        text.contains('B') && text.contains('C'),
        "both frame-2 labels"
    );

    // And the coordinates are the processor's, not a default: the box was
    // reported at 1,1-2,2, so (0070,0022) Graphic Data carries the closed
    // polyline (0070,0021 Dimension Index Values says five points). A
    // zeroed pair moves the annotation off the finding while every count
    // and label still looks right.
    for value in [1.0f32, 1.0, 2.0, 1.0, 2.0, 2.0, 1.0, 2.0] {
        assert!(
            find_subslice(second_overlay, &value.to_le_bytes()).is_some(),
            "coordinate {value} is in the graphic data"
        );
    }
}

/// The result body for a set of traced contours, one per frame.
fn contours_body(entries: &[(&str, u32)]) -> String {
    let contours: Vec<String> = entries
        .iter()
        .map(|(label, frame)| {
            format!(
                r#"{{"label": "{label}", "frame": {frame},
                     "points": [[1, 1], [2, 1], [2, 2], [1, 1]]}}"#
            )
        })
        .collect();
    format!(
        r#"{{"protocol": {{"name": "clinfer-abi", "version": 1}},
             "producer": {{"script": "contours.py"}},
             "pr": {{"contours": [{}]}}}}"#,
        contours.join(",")
    )
}

#[test]
fn one_overlay_is_stored_for_a_contours_only_pr_block() {
    // The live run caught the dispatch dropping a contours-only pr block:
    // the storage decision checked BOXES and a segmenter has none. This
    // test pins the fixed shape end to end: one frame with a contour, one
    // GSPS stored, and no other object.
    let python = require_python!();
    let scratch = Scratch::new("pr-contours");
    let script = scratch.script(
        "contours.py",
        &result_script(&contours_body(&[("Lung", 2)])),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let first = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let second = source_instance("1.2.3.4.5.2", &[5, 6, 7, 8]);
    let fake = FakeOrigin::start(
        Answers::new(&[first, second], &["1.2.3.4.5.1", "1.2.3.4.5.2"]).then(success_tail(1)),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let overlays: Vec<&Vec<u8>> = stored
        .iter()
        .filter(|object| {
            sop_class_uid(object).is_some_and(|uid| uid == "1.2.840.10008.5.1.4.1.1.11.1")
        })
        .collect();
    assert_eq!(overlays.len(), 1, "one GSPS for the frame with the contour");

    // The contour's ring is in the graphic data: 4 points, closed.
    let text = String::from_utf8_lossy(overlays[0]);
    for value in [1.0f32, 1.0, 2.0, 1.0, 2.0, 2.0, 1.0, 1.0] {
        assert!(
            find_subslice(overlays[0], &value.to_le_bytes()).is_some(),
            "coordinate {value} is in the graphic data of {text}"
        );
    }
}

#[test]
fn the_displayed_area_is_the_boxs_own_frame() {
    // The overlay is presented over ONE frame, so the displayed area has
    // to be that frame's geometry. A presentation state bound to frame 2
    // that displays frame 1's extents crops or offsets the annotation on
    // the workstation, and nothing else in the object would say so.
    let python = require_python!();
    let scratch = Scratch::new("pr-displayed-area");
    let script = scratch.script("boxes.py", &result_script(&boxes_body(&[("A", 1, 0.9)])));
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let overlay = stored
        .iter()
        .find(|object| {
            sop_class_uid(object).is_some_and(|uid| uid == "1.2.840.10008.5.1.4.1.1.11.1")
        })
        .expect("one overlay was stored");

    // The fixture's frame is 2x2, so the bottom right corner extends 200
    // past 2 in each direction. A constant here means the frame geometry
    // never reached the encoder.
    let (columns, rows) = tag_sl_pair(overlay, 0x0070, 0x0053)
        .expect("(0070,0053) DisplayedAreaBottomRightHandCorner is present");
    assert_eq!(
        (columns, rows),
        (2 + 200, 2 + 200),
        "the displayed area is the frame's own geometry, not a constant"
    );
}

#[test]
fn a_frame_with_no_boxes_gets_no_overlay() {
    let python = require_python!();
    let scratch = Scratch::new("pr-empty-frame");
    let script = scratch.script("boxes.py", &result_script(&boxes_body(&[("Only", 1, 0.9)])));
    let conf = external_conf(scratch.path(), &script, &python);
    let first = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let second = source_instance("1.2.3.4.5.2", &[5, 6, 7, 8]);
    // Only the first frame gets an overlay, so one STOW then COMPLETED.
    let fake = FakeOrigin::start(
        Answers::new(&[first, second], &["1.2.3.4.5.1", "1.2.3.4.5.2"]).then(success_tail(1)),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let overlays = stored
        .iter()
        .filter(|object| {
            sop_class_uid(object).is_some_and(|uid| uid == "1.2.840.10008.5.1.4.1.1.11.1")
        })
        .count();
    assert_eq!(overlays, 1, "the frame with no boxes gets no object");
}

#[test]
fn a_box_on_a_frame_the_study_does_not_have_cancels_the_cycle() {
    // The check happens BEFORE anything is stored: half a study's overlays
    // is worse than none, because it looks complete.
    let python = require_python!();
    let scratch = Scratch::new("pr-out-of-range");
    let script = scratch.script("boxes.py", &result_script(&boxes_body(&[("A", 7, 0.9)])));
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // Nothing is stored: the frame check refuses before any STOW.
    let fake =
        FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(vec![Answer::ok("{}")]));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    match &outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("frame 7"), "{reason}");
            assert!(reason.contains("1 frame"), "{reason}");
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
    // Nothing was stored: the cancel path stores nothing.
    assert!(
        stored_objects(&fake).is_empty(),
        "no overlay may be stored for a refused result"
    );
}

// -- Tests: sr --------------------------------------------------------

#[test]
fn the_report_text_is_stored_verbatim() {
    let python = require_python!();
    let scratch = Scratch::new("sr");
    let text = "No acute cardiopulmonary process.\nSecond line.";
    let body = format!(
        r#"{{"protocol": {{"name": "clinfer-abi", "version": 1}},
             "producer": {{"script": "report.py"}},
             "sr": {{"text": {text:?}}}}}"#
    );
    let script = scratch.script("report.py", &result_script(&body));
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let reports: Vec<&Vec<u8>> = stored
        .iter()
        .filter(|object| {
            sop_class_uid(object).is_some_and(|uid| uid == "1.2.840.10008.5.1.4.1.1.88.11")
        })
        .collect();
    assert_eq!(reports.len(), 1, "one Basic Text SR for the study");
    let text_out = String::from_utf8_lossy(reports[0]);
    // The processor's words, unaltered: clinfer adds no wording of its own.
    assert!(
        text_out.contains("No acute cardiopulmonary process."),
        "{text_out}"
    );
    assert!(text_out.contains("Second line."), "{text_out}");
    // And the evidence names the analyzed image.
    assert_eq!(
        tag_text(reports[0], 0x0008, 0x1155).as_deref(),
        Some("1.2.3.4.5.1")
    );
    assert!(sop_instance_uid(reports[0]).is_some());
}

#[test]
fn an_empty_report_text_stores_nothing() {
    // A blank answer is not a document with an empty text item: there is
    // nothing to report, and storing an empty SR would put an object in the
    // study that says nothing.
    let python = require_python!();
    let scratch = Scratch::new("sr-blank");
    let body = r#"{"protocol": {"name": "clinfer-abi", "version": 1},
                   "producer": {"script": "report.py"},
                   "sr": {"text": "   "},
                   "pr": {"boxes": [{"label": "A", "x1": 1, "y1": 1, "x2": 2, "y2": 2}]}}"#;
    let script = scratch.script("report.py", &result_script(body));
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // The boxes are stored, the blank report is not.
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let reports = stored
        .iter()
        .filter(|object| {
            sop_class_uid(object).is_some_and(|uid| uid == "1.2.840.10008.5.1.4.1.1.88.11")
        })
        .count();
    assert_eq!(reports, 0, "a blank report is not stored");
    // The boxes in the same result still are: the blocks are independent.
    let overlays = stored
        .iter()
        .filter(|object| {
            sop_class_uid(object).is_some_and(|uid| uid == "1.2.840.10008.5.1.4.1.1.11.1")
        })
        .count();
    assert_eq!(overlays, 1, "the pr block is unaffected by the blank sr");
}

// -- Tests: workdir cleanup -------------------------------------------

#[test]
fn a_successful_run_removes_its_workdir() {
    let python = require_python!();
    let scratch = Scratch::new("cleanup-success");
    let script = scratch.script("boxes.py", &result_script(&boxes_body(&[("A", 1, 0.9)])));
    let conf = external_conf(scratch.path(), &script, &python);
    let root = workdir::root(&conf.engine.cache_dir);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");
    assert!(
        !stored_objects(&fake).is_empty(),
        "the run stored something, so it succeeded"
    );
    let leftover: Vec<_> = std::fs::read_dir(&root)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(leftover.is_empty(), "the workdir is gone: {leftover:?}");
}

#[test]
fn a_run_that_fails_while_storing_keeps_its_frames() {
    // The cleanup runs on the way out, so it runs after the stores. If it
    // ran before them, every object the run was about to store would be
    // unreadable - the encoder reads its input from the workdir - and the
    // failure would look like an origin problem rather than a lost run.
    //
    // A failed run keeps its directory either way, so the directory's
    // presence proves nothing here: what proves the order is that the
    // frames are still readable when the store fails.
    let python = require_python!();
    let scratch = Scratch::new("cleanup-order");
    let script = scratch.script("boxes.py", &result_script(&boxes_body(&[("A", 1, 0.9)])));
    let conf = external_conf(scratch.path(), &script, &python);
    let root = workdir::root(&conf.engine.cache_dir);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // The store is refused: the run ends in a cancel, workdir kept.
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(failure_tail(0)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let _ = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }

    let leftover: Vec<_> = std::fs::read_dir(&root)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert_eq!(leftover.len(), 1, "the failed run's workdir is kept");
    let kept = &leftover[0];
    // The store was attempted, so the run got that far with its input
    // intact: the frames it read are still on disk.
    let frames = kept.join("frames");
    let raw: Vec<_> = std::fs::read_dir(&frames)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        !raw.is_empty(),
        "the frames outlived the store that failed: {kept:?}"
    );
    assert!(
        raw.iter().all(|path| {
            std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        }),
        "and they are not empty: {raw:?}"
    );
}

#[test]
fn a_failed_run_keeps_its_workdir_for_forensics() {
    // The whole point of keep-on-failure: the directory an operator needs
    // to reproduce a bad run is the one that failed.
    let python = require_python!();
    let scratch = Scratch::new("cleanup-failure");
    // A non-zero exit AFTER writing a valid result: the failure rule, not
    // the schema rule, is what decides here.
    let script = scratch.script(
        "fail.py",
        &format!(
            r#"
import pathlib, sys
workdir = pathlib.Path(sys.argv[1])
(workdir / "result.json").write_text({body:?})
sys.stderr.write("deliberate failure\n")
sys.exit(4)
"#,
            body = boxes_body(&[("A", 1, 0.9)])
        ),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let root = workdir::root(&conf.engine.cache_dir);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // The failed run stores nothing, so only the cancel is answered.
    let fake =
        FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(vec![Answer::ok("{}")]));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    match &outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("exit 4"), "{reason}")
        }
        other => panic!("expected a cancel, got {other:?}"),
    }

    let leftover: Vec<_> = std::fs::read_dir(&root)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert_eq!(leftover.len(), 1, "the failed run's workdir is kept");
    // And it is usable: the frames and the result are still there.
    let kept = &leftover[0];
    assert!(
        kept.join("manifest.json").is_file(),
        "the manifest survives"
    );
    assert!(kept.join("result.json").is_file(), "the result survives");
    assert!(
        kept.join("frames").join("0000.raw").is_file(),
        "the frames survive - that is what makes a rerun by hand possible"
    );
}

// -- Tests: stl --------------------------------------------------------

/// The SOP Class UID of an Encapsulated STL.
const STL_CLASS: &str = "1.2.840.10008.5.1.4.1.1.104.3";

/// A well-formed binary STL of `triangles` triangles.
///
/// The header is 80 bytes, then a little-endian u32 count, then 50 bytes a
/// triangle. The bytes are zeroes because nothing reads them: the format's
/// own header is what the worker validates, and a test that built a real
/// mesh would be testing arithmetic rather than the writer.
fn stl_bytes(triangles: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 80];
    bytes[0..5].copy_from_slice(b"mesh!");
    bytes.extend_from_slice(&triangles.to_le_bytes());
    bytes.extend(std::iter::repeat_n(0u8, triangles as usize * 50));
    bytes
}

/// A script that writes one file and a `result.json` naming it.
///
/// The same shape as `seg_script`, for one file rather than a list.
fn stl_script(name: &str, bytes: Vec<u8>, body: &str) -> String {
    format!(
        r#"
import pathlib, sys
workdir = pathlib.Path(sys.argv[1])
(workdir / {name:?}).write_bytes(bytes({bytes:?}))
(workdir / "result.json.tmp").write_text({body:?})
(workdir / "result.json.tmp").rename(workdir / "result.json")
"#
    )
}

/// The `stl` result body naming one file.
fn stl_body(name: &str) -> String {
    format!(
        r#"{{"protocol": {{"name": "clinfer-abi", "version": 1}},
             "producer": {{"script": "STL-mesh.py"}},
             "stl": {{"file": "{name}"}}}}"#
    )
}

/// The bytes of one OB element, read by tag.
fn ob_element(object: &[u8], group: u16, element: u16) -> Option<&[u8]> {
    let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
    let at = object
        .windows(4)
        .position(|window| window == needle.as_slice())?;
    assert_eq!(&object[at + 4..at + 6], b"OB", "the element is OB");
    let length = u32::from_le_bytes([
        object[at + 8],
        object[at + 9],
        object[at + 10],
        object[at + 11],
    ]) as usize;
    object.get(at + 12..at + 12 + length)
}

#[test]
fn a_surface_mesh_is_stored_as_an_encapsulated_stl() {
    // The happy path: one frame, one mesh file, one Encapsulated STL whose
    // document is the file's bytes unchanged.
    let python = require_python!();
    let scratch = Scratch::new("stl-happy");
    let mesh = stl_bytes(3);
    let script = scratch.script(
        "mesh.py",
        &stl_script("mesh.stl", mesh.clone(), &stl_body("mesh.stl")),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let surfaces = objects_of_class(&stored, STL_CLASS);
    assert_eq!(surfaces.len(), 1, "one surface object per study");
    let object = surfaces[0];

    // The modality is the ENUMERATED value, not "STL": A.85.1.4.3 has no
    // such value, and a reader looking for M3D would not find an "STL".
    assert_eq!(tag_text(object, 0x0008, 0x0060).as_deref(), Some("M3D"));
    // The MIME type is what tells a viewer which application opens the
    // payload, so it is checked exactly.
    assert_eq!(
        tag_text(object, 0x0042, 0x0012).as_deref(),
        Some("model/stl")
    );
    // The document is the file, byte for byte: a re-encode would round
    // floats and the mesh a viewer loads would not be the one computed.
    let document = ob_element(object, 0x0042, 0x0011).expect("the document element");
    assert_eq!(document, mesh.as_slice(), "the STL is copied verbatim");

    // The identity is derived from the STUDY, the model and the profile, so
    // a rerun reproduces the same UID and a bumped profile produces a new
    // one. Pinning the value is what makes the arc itself testable: a
    // mutation that changed the seed - adding a suffix, or dropping the
    // profile from it - would move this number. The form is the crate's
    // convention for every derived writer: `digest128` renders the 128-bit
    // digest under the registered 2.25 root, as the PR, SR and SEG arcs do.
    let uid = sop_instance_uid(object).expect("the object carries its SOP Instance UID");
    assert_eq!(
        uid, "2.25.226478138689859691822324826238773865452",
        "the surface UID is the digest of study, model and profile"
    );
    assert!(
        uid.strip_prefix("2.25.")
            .is_some_and(|digits| digits.chars().all(|c| c.is_ascii_digit())),
        "a derived UID is a 2.25-rooted decimal digest, got {uid:?}"
    );
    let series = tag_text(object, 0x0020, 0x000E).expect("Series Instance UID");
    assert_ne!(series, uid, "the series has its own UID");
}

#[test]
fn a_surface_mesh_declares_a_frame_of_reference() {
    // A.85.1 makes (0020,0052) Type 1. The source image here declares one,
    // so the mesh copies it rather than inventing a frame.
    let python = require_python!();
    let scratch = Scratch::new("stl-for");
    let script = scratch.script(
        "mesh.py",
        &stl_script("mesh.stl", stl_bytes(1), &stl_body("mesh.stl")),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let object = objects_of_class(&stored, STL_CLASS)
        .first()
        .copied()
        .expect("a surface object");
    // The fixture image declares no Frame of Reference, and A.85.1 makes
    // (0020,0052) Type 1. So the mesh must carry a DERIVED one rather than
    // omit the attribute: a mesh has a coordinate system whether or not its
    // source named one, and an absent Type 1 attribute is a malformed
    // object. The derived form is recognisable by its arc.
    let declared =
        tag_text(object, 0x0020, 0x0052).expect("Frame of Reference is Type 1 and always written");
    assert!(
        !declared.is_empty(),
        "a derived Frame of Reference is still a UID"
    );
    assert_ne!(
        declared, "1.2.3.4.5.1",
        "the source declares none, so this cannot be the source's"
    );
}

#[test]
fn a_surface_mesh_copies_the_sources_frame_of_reference_when_it_has_one() {
    // The other half of the rule: when the source DOES define a Frame of
    // Reference, the mesh copies it, because a mesh derived from those
    // images lives in their coordinate system. A derived stand-in here
    // would name a frame nothing else shares.
    let python = require_python!();
    let scratch = Scratch::new("stl-for-source");
    let script = scratch.script(
        "mesh.py",
        &stl_script("mesh.stl", stl_bytes(1), &stl_body("mesh.stl")),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let only =
        source_instance_with_frame_of_reference("1.2.3.4.5.1", "1.2.840.113857.9.9", &[1, 2, 3, 4]);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let object = objects_of_class(&stored, STL_CLASS)
        .first()
        .copied()
        .expect("a surface object");
    assert_eq!(
        tag_text(object, 0x0020, 0x0052).as_deref(),
        Some("1.2.840.113857.9.9"),
        "the source's Frame of Reference is copied, not replaced"
    );
}

#[test]
fn a_mesh_whose_size_disagrees_with_its_header_cancels_the_cycle() {
    // The file says it holds three triangles and does not: that is what a
    // truncated or concatenated mesh looks like, and storing it would put a
    // document in the study that no viewer can load.
    let python = require_python!();
    let scratch = Scratch::new("stl-short");
    let mut mesh = stl_bytes(3);
    mesh.pop();
    let script = scratch.script(
        "mesh.py",
        &stl_script("mesh.stl", mesh, &stl_body("mesh.stl")),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // Nothing is stored, so the tail is the cancellation only.
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(0)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    match outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("header declares"), "{reason}");
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
    assert!(
        stored_objects(&fake).is_empty(),
        "a malformed mesh stores nothing"
    );
}

#[test]
fn a_result_with_no_stl_block_stores_no_surface() {
    // The block is optional, and a result that does not carry one must
    // store no surface object at all.
    let python = require_python!();
    let scratch = Scratch::new("stl-absent");
    let script = scratch.script("boxes.py", &result_script(&boxes_body(&[("A", 1, 0.9)])));
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");
    assert!(
        objects_of_class(&stored_objects(&fake), STL_CLASS).is_empty(),
        "no stl block means no surface object"
    );
}

// -- Tests: seg --------------------------------------------------------

/// The SOP Class UID of a BINARY segmentation.
const SEG_CLASS: &str = "1.2.840.10008.5.1.4.1.1.66.4";
const RTSTRUCT_CLASS: &str = "1.2.840.10008.5.1.4.1.1.481.3";

#[test]
fn a_segmentation_is_stored_with_its_masks_packed_verbatim() {
    // The happy path: two frames, two masks, one segmentation object whose
    // Pixel Data is the two packed masks one after the other.
    let python = require_python!();
    let scratch = Scratch::new("seg-happy");
    let first = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let second = source_instance("1.2.3.4.5.2", &[5, 6, 7, 8]);
    // A 2x2 mask is one byte per row, so two bytes a frame.
    let mask_one: Vec<u8> = vec![0b1010_0000, 0b0101_0000];
    let mask_two: Vec<u8> = vec![0b1100_0000, 0b0011_0000];
    let script = scratch.script(
        "seg.py",
        &seg_script(
            &[
                ("mask-1.raw", mask_one.clone()),
                ("mask-2.raw", mask_two.clone()),
            ],
            &seg_body(&[("mask-1.raw", 1), ("mask-2.raw", 2)]),
        ),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(
        Answers::new(&[first, second], &["1.2.3.4.5.1", "1.2.3.4.5.2"]).then(success_tail(1)),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let segments = objects_of_class(&stored, SEG_CLASS);
    if segments.is_empty() {
        let paths: Vec<String> = fake
            .requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect();
        panic!("no segmentation stored; requests were {paths:?}");
    }
    assert_eq!(
        segments.len(),
        1,
        "one segmentation per study, not per frame"
    );
    let object = segments[0];

    // The masks, concatenated in frame order and unchanged.
    let payload = pixel_data(object).expect("pixel data");
    let mut expected = mask_one.clone();
    expected.extend_from_slice(&mask_two);
    assert_eq!(
        payload,
        expected.as_slice(),
        "the masks are copied verbatim"
    );

    // Bits Allocated 1, and the photometric a segmentation declares.
    let at = find_subslice(object, &[0x28, 0x00, 0x00, 0x01]).expect("(0028,0100)");
    assert_eq!(
        u16::from_le_bytes([object[at + 8], object[at + 9]]),
        1,
        "Bits Allocated is 1"
    );
    assert!(tag_text(object, 0x0028, 0x0004).is_some_and(|v| v == "MONOCHROME2"));

    // The declarations reached the object: BINARY, no overlap declared
    // by the producer (so (0062,0013) is omitted rather than guessed),
    // one segment with its code, colour and default algorithm.
    assert!(tag_text(object, 0x0062, 0x0001).is_some_and(|v| v == "BINARY"));
    assert!(
        tag_text(object, 0x0062, 0x0013).is_none(),
        "overlap is not invented for a producer that did not declare it"
    );
    assert!(tag_text(object, 0x0062, 0x0005).is_some_and(|v| v == "THRESHOLD"));
    assert!(tag_text(object, 0x0008, 0x0060).is_some_and(|v| v == "SEG"));

    // Both sources are named, one per frame.
    let text = String::from_utf8_lossy(object);
    assert!(text.contains("1.2.3.4.5.1"), "frame 1's source");
    assert!(text.contains("1.2.3.4.5.2"), "frame 2's source");

    // Number of Frames is the study's frame count.
    let at = find_subslice(object, &[0x28, 0x00, 0x08, 0x00]).expect("(0028,0008)");
    let length = u16::from_le_bytes([object[at + 6], object[at + 7]]) as usize;
    assert_eq!(
        String::from_utf8_lossy(&object[at + 8..at + 8 + length]).trim(),
        "2"
    );

    // -- The conformance attributes, as a reader would find them ------
    // (0008,0008) Image Type: two values, so the element reads as one
    // multi-valued CS rather than a single "DERIVED".
    assert!(
        tag_text(object, 0x0008, 0x0008).is_some_and(|v| v == "DERIVED\\PRIMARY"),
        "Image Type is DERIVED\\PRIMARY"
    );
    // (0028,2110) Lossy Image Compression, Type 1 and lossless here.
    assert!(
        tag_text(object, 0x0028, 0x2110).is_some_and(|v| v == "00"),
        "lossless"
    );
    // Enhanced General Equipment names the implementation.
    assert!(tag_text(object, 0x0008, 0x1090).is_some(), "(0008,1090)");
    assert!(tag_text(object, 0x0018, 0x1000).is_some(), "(0018,1000)");
    assert!(tag_text(object, 0x0018, 0x1020).is_some(), "(0018,1020)");
    // Content Identification: the label is Type 1, non-empty, and with no
    // rule series description configured it is the first segment's label
    // rather than a placeholder.
    assert_eq!(
        tag_text(object, 0x0070, 0x0080).as_deref(),
        Some("THRESHOLD"),
        "(0070,0080) Content Label falls back to the first segment's label"
    );
    // (0062,0009) names the algorithm that produced the segment, which on
    // this path is the configured model that ran.
    assert!(
        tag_text(object, 0x0062, 0x0009).is_some_and(|v| v == "proc"),
        "(0062,0009) Segment Algorithm Name"
    );
    // The derivation and purpose codes A.51.5.1 mandates.
    let text = String::from_utf8_lossy(object);
    assert!(text.contains("113076"), "(0008,9215) Derivation Code");
    assert!(text.contains("121322"), "(0040,A170) Purpose of Reference");
    // Referenced Series, the grouping a reader follows back to the pixels.
    assert!(
        find_subslice(object, &[0x08, 0x00, 0x15, 0x11]).is_some(),
        "(0008,1115) Referenced Series Sequence"
    );
    assert!(
        find_subslice(object, &[0x08, 0x00, 0x4A, 0x11]).is_some(),
        "(0008,114A) Referenced Instance Sequence"
    );
    // Multi-frame Dimension: the organisation and its index pointer.
    assert!(
        find_subslice(object, &[0x20, 0x00, 0x21, 0x92]).is_some(),
        "(0020,9221) Dimension Organization Sequence"
    );
    assert!(
        find_subslice(object, &[0x20, 0x00, 0x22, 0x92]).is_some(),
        "(0020,9222) Dimension Index Sequence"
    );
    // (0008,001B) is deliberately absent.
    assert!(
        find_subslice(object, &[0x08, 0x00, 0x1B, 0x00]).is_none(),
        "(0008,001B) must not be written"
    );
}

#[test]
fn a_segmentation_copies_the_sources_spacing_and_plane_positions() {
    // The source declares a physical spacing, a slice spacing and a
    // position per frame. All three have to reach the segmentation: a
    // mask whose Pixel Measures say "1\1" when the image says "0.7\0.7"
    // cannot be measured, and a frame with no Plane Position cannot be
    // placed in the study's coordinate system.
    let python = require_python!();
    let scratch = Scratch::new("seg-spacing");
    let first = source_instance_with_geometry(
        "1.2.3.4.5.1",
        b"0.7\\0.9",
        2.5,
        b"1.5\\2.5\\0",
        &[1, 2, 3, 4],
    );
    let second = source_instance_with_geometry(
        "1.2.3.4.5.2",
        b"0.7\\0.9",
        2.5,
        b"1.5\\2.5\\3",
        &[5, 6, 7, 8],
    );
    let mask_one: Vec<u8> = vec![0b1010_0000, 0b0101_0000];
    let mask_two: Vec<u8> = vec![0b1100_0000, 0b0011_0000];
    let script = scratch.script(
        "seg.py",
        &seg_script(
            &[("mask-1.raw", mask_one), ("mask-2.raw", mask_two)],
            &seg_body(&[("mask-1.raw", 1), ("mask-2.raw", 2)]),
        ),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(
        Answers::new(&[first, second], &["1.2.3.4.5.1", "1.2.3.4.5.2"]).then(success_tail(1)),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let object = objects_of_class(&stored, SEG_CLASS)
        .into_iter()
        .next()
        .expect("a segmentation");
    let text = String::from_utf8_lossy(object);
    // The source's own DS spellings survive: the SEG claims to copy its
    // geometry, and a value that fits the VR is written exactly as it was
    // stated rather than re-rounded to the writer's precision.
    assert!(
        text.contains("0.7\\0.9"),
        "the copied pixel spacing, in the source's own order"
    );
    assert!(!text.contains("0.9\\0.7"), "the components are not swapped");
    assert!(!text.contains("1\\1"), "no fabricated unit spacing");
    assert!(text.contains("2.5"), "Spacing Between Slices");
    assert!(text.contains("1.5\\2.5\\0"), "frame 1's plane position");
    assert!(text.contains("1.5\\2.5\\3"), "frame 2's plane position");
    assert!(
        find_subslice(object, &[0x20, 0x00, 0x13, 0x91]).is_some(),
        "(0020,9113) Plane Position Sequence"
    );
}

#[test]
fn a_two_segment_mask_carries_both_bit_planes() {
    // "2+ segments" is the case the wire tests have to cover: the file
    // holds one bit plane per segment, one after the other, and the
    // writer emits ONE FRAME PER SEGMENT (C.8.20.3.1), so the object has
    // two frames whose planes concatenate to the file's bytes.
    let python = require_python!();
    let scratch = Scratch::new("seg-two");
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // A 2x2 frame is one byte per row, so a plane is 2 bytes and a file
    // with two segments is 4.
    let plane_one: Vec<u8> = vec![0b1010_0000, 0b0101_0000];
    let plane_two: Vec<u8> = vec![0b0000_0011, 0b0000_1100];
    let mut mask = plane_one.clone();
    mask.extend_from_slice(&plane_two);
    let script = scratch.script(
        "seg.py",
        &seg_script(&[("both.raw", mask.clone())], &two_segment_body("both.raw")),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let object = objects_of_class(&stored, SEG_CLASS)
        .into_iter()
        .next()
        .expect("a segmentation");
    // Both planes reach Pixel Data, in order, unchanged.
    assert_eq!(
        pixel_data(object).expect("pixel data"),
        mask.as_slice(),
        "both bit planes are copied verbatim"
    );
    // One frame per (source, segment): two frames, each naming its
    // segment in its per-frame Segment Identification item.
    assert_eq!(
        tag_text(object, 0x0028, 0x0008).as_deref(),
        Some("2"),
        "two frames: one bit plane of one segment each"
    );
    let mut named: Vec<u16> = Vec::new();
    let needle = [0x62u8, 0x00, 0x0B, 0x00, 0x55, 0x53];
    let mut from = 0;
    while let Some(at) = find_subslice(&object[from..], &needle) {
        let at = from + at;
        from = at + 2;
        if u16::from_le_bytes([object[at + 6], object[at + 7]]) == 2 {
            named.push(u16::from_le_bytes([object[at + 8], object[at + 9]]));
        }
    }
    assert_eq!(
        named,
        vec![1, 2],
        "each frame names its segment via (0062,000B)"
    );
    // And the object declares two segments, each with its own label and
    // colour.
    let text = String::from_utf8_lossy(object);
    assert!(
        text.contains("RIGHT_LUNG") && text.contains("HEART"),
        "{text}"
    );
    // Segment numbers are 1..N in declaration order, so the two
    // (0062,0004) US values are 1 and 2.
    let mut found: Vec<u16> = Vec::new();
    let needle = [0x62u8, 0x00, 0x04, 0x00, 0x55, 0x53];
    let mut from = 0;
    while let Some(at) = find_subslice(&object[from..], &needle) {
        let at = from + at;
        from = at + 2;
        if u16::from_le_bytes([object[at + 6], object[at + 7]]) == 2 {
            found.push(u16::from_le_bytes([object[at + 8], object[at + 9]]));
        }
    }
    assert_eq!(found, vec![1, 2], "one segment number per declared segment");
}

#[test]
fn the_declared_frames_decide_which_mask_goes_where() {
    // The files list may name the masks in any order: the frame number is
    // what associates a mask with an image, and getting that wrong would
    // silently put one image's mask over another's.
    let python = require_python!();
    let scratch = Scratch::new("seg-order");
    let first = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let second = source_instance("1.2.3.4.5.2", &[5, 6, 7, 8]);
    let mask_one: Vec<u8> = vec![0xAA, 0xAA];
    let mask_two: Vec<u8> = vec![0x55, 0x55];
    // Declared second-then-first, so frame 1 must carry mask-one.
    let script = scratch.script(
        "seg.py",
        &seg_script(
            &[
                ("mask-1.raw", mask_one.clone()),
                ("mask-2.raw", mask_two.clone()),
            ],
            &seg_body(&[("mask-2.raw", 2), ("mask-1.raw", 1)]),
        ),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(
        Answers::new(&[first, second], &["1.2.3.4.5.1", "1.2.3.4.5.2"]).then(success_tail(1)),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let object = objects_of_class(&stored, SEG_CLASS)
        .into_iter()
        .next()
        .expect("a segmentation");
    let payload = pixel_data(object).expect("pixel data");
    let mut expected = mask_one.clone();
    expected.extend_from_slice(&mask_two);
    assert_eq!(
        payload,
        expected.as_slice(),
        "frame 1's mask comes first regardless of the order they were declared"
    );
}

#[test]
fn a_mask_of_the_wrong_size_cancels_the_cycle() {
    // Byte consistency is the invariant: a mask that does not match the
    // frame's geometry is a mask that cannot be laid over that image. A
    // named cancel, never a stored object.
    let python = require_python!();
    let scratch = Scratch::new("seg-size");
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // Two bytes is right for a 2x2 frame; three is not.
    let script = scratch.script(
        "seg.py",
        &seg_script(
            &[("mask.raw", vec![1, 2, 3])],
            &seg_body(&[("mask.raw", 1)]),
        ),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    // The size check runs while the masks are read and before any store, so
    // the only call left is the cancel transition.
    let fake = FakeOrigin::start(
        Answers::new(&[only], &["1.2.3.4.5.1"]).then(vec![Answer::ok("{}"), Answer::ok("{}")]),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    match &outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("3 bytes"), "{reason}");
            assert!(
                reason.contains("2 bytes"),
                "names the expected size: {reason}"
            );
            assert!(reason.contains("1-bit mask"), "{reason}");
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
    assert!(
        stored_objects(&fake).is_empty(),
        "a wrong-sized mask is never stored"
    );
}

#[test]
fn a_mask_count_that_disagrees_with_the_study_cancels_the_cycle() {
    // One mask per source image. A count that disagrees is refused before
    // anything is stored, so a study never gets a partial segmentation
    // that looks complete.
    let python = require_python!();
    let scratch = Scratch::new("seg-count");
    let first = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let second = source_instance("1.2.3.4.5.2", &[5, 6, 7, 8]);
    // Two frames, one mask.
    let script = scratch.script(
        "seg.py",
        &seg_script(
            &[("mask.raw", vec![0xAA, 0xAA])],
            &seg_body(&[("mask.raw", 1)]),
        ),
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(
        Answers::new(&[first, second], &["1.2.3.4.5.1", "1.2.3.4.5.2"])
            .then(vec![Answer::ok("{}")]),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    match &outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("1 segmentation mask"), "{reason}");
            assert!(reason.contains("2 frame(s)"), "{reason}");
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
    assert!(stored_objects(&fake).is_empty());
}

#[test]
fn a_segmentation_missing_a_source_file_cancels_the_cycle() {
    // The result names a mask the processor never wrote: a named error
    // naming the frame and the path, not a panic and not a stored object.
    let python = require_python!();
    let scratch = Scratch::new("seg-missing");
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // The body names a mask, but the script writes none.
    let script = scratch.script("seg.py", &seg_script(&[], &seg_body(&[("absent.raw", 1)])));
    let conf = external_conf(scratch.path(), &script, &python);
    let fake =
        FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(vec![Answer::ok("{}")]));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    match &outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("frame 1"), "{reason}");
            assert!(reason.contains("absent.raw"), "{reason}");
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
}

#[test]
fn a_result_with_no_seg_block_stores_no_segmentation() {
    // Absence is not an empty segmentation: a processor that produced
    // boxes stores boxes and nothing else.
    let python = require_python!();
    let scratch = Scratch::new("seg-absent");
    let script = scratch.script("boxes.py", &result_script(&boxes_body(&[("A", 1, 0.9)])));
    let conf = external_conf(scratch.path(), &script, &python);
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");
    let stored = stored_objects(&fake);
    assert!(
        objects_of_class(&stored, SEG_CLASS).is_empty(),
        "no seg block, no segmentation object"
    );
}

// -- Tests: the relaxed derived rule -----------------------------------

/// A script that writes a derived frame of a declared size.
fn upscale_script(width: u32, height: u32, bytes: usize) -> String {
    format!(
        r#"
import pathlib, sys
workdir = pathlib.Path(sys.argv[1])
(workdir / "0000.raw").write_bytes(bytes({bytes}))
(workdir / "result.json").write_text({body:?})
"#,
        body = format!(
            r#"{{"protocol": {{"name": "clinfer-abi", "version": 1}},
                 "producer": {{"script": "DERIVED-upscale.py"}},
                 "derived": {{"files": [{{"file": "0000.raw",
                     "width": {width}, "height": {height}}}]}}}}"#
        )
    )
}

#[test]
fn a_declared_geometry_larger_than_the_source_is_stored() {
    // The relaxed rule: the processor declares what it made and clinfer
    // checks byte consistency against the DECLARATION, not equality with
    // the source. An upscaled image is a legal derived image.
    let python = require_python!();
    let scratch = Scratch::new("derived-upscale");
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // 2x2 source, 4x4 output at 8 bits = 16 bytes.
    let script = scratch.script("DERIVED-upscale.py", &upscale_script(4, 4, 16));
    let conf = external_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let derived = objects_of_class(&stored, "1.2.840.10008.5.1.4.1.1.7");
    assert_eq!(derived.len(), 1, "one derived image");
    let object = derived[0];
    // The object declares the DECLARED geometry, not the source's.
    for (element, expected) in [(0x0010u16, 4u16), (0x0011, 4)] {
        let at = find_subslice(object, &[0x28, 0x00, element as u8, 0x00])
            .unwrap_or_else(|| panic!("(0028,{element:04X})"));
        assert_eq!(
            u16::from_le_bytes([object[at + 8], object[at + 9]]),
            expected,
            "(0028,{element:04X}) is the declared value"
        );
    }
}

#[test]
fn a_declared_geometry_that_does_not_match_the_file_cancels_the_cycle() {
    // Byte consistency against the declaration is what catches a mask or
    // an image that was truncated or padded by mistake.
    let python = require_python!();
    let scratch = Scratch::new("derived-mismatch");
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    // Declares 4x4 (16 bytes) but writes 9.
    let script = scratch.script("DERIVED-upscale.py", &upscale_script(4, 4, 9));
    let conf = external_conf(scratch.path(), &script, &python);
    let fake =
        FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(vec![Answer::ok("{}")]));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    match &outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("9 bytes"), "{reason}");
            assert!(reason.contains("declared geometry"), "{reason}");
            assert!(reason.contains("16 bytes"), "{reason}");
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
    assert!(stored_objects(&fake).is_empty());
}

#[test]
fn an_undeclared_geometry_must_match_the_source() {
    // The unchanged half of the rule: clahe declares nothing and returns
    // source-sized pixels, so a file of any other size is still refused.
    let python = require_python!();
    let scratch = Scratch::new("derived-source-size");
    let only = source_instance("1.2.3.4.5.1", &[1, 2, 3, 4]);
    let script = scratch.script(
        "filter.py",
        r#"
import pathlib, sys
workdir = pathlib.Path(sys.argv[1])
(workdir / "0000.raw").write_bytes(bytes(9))
(workdir / "result.json").write_text(
    '{"protocol": {"name": "clinfer-abi", "version": 1},'
    ' "producer": {"script": "filter.py"},'
    ' "derived": {"files": [{"file": "0000.raw"}]}}')
"#,
    );
    let conf = external_conf(scratch.path(), &script, &python);
    let fake =
        FakeOrigin::start(Answers::new(&[only], &["1.2.3.4.5.1"]).then(vec![Answer::ok("{}")]));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    match &outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("9 bytes"), "{reason}");
            assert!(
                reason.contains("2x2x1 at 8 bits = 4 bytes"),
                "the source geometry is quoted: {reason}"
            );
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
}

// -- Tests: rtstruct --------------------------------------------------

/// A source instance with the geometry a structure set has to convert
/// through: Pixel Spacing, Image Orientation Patient and Image Position
/// Patient.
///
/// The orientation is deliberately NON-IDENTITY and the spacing
/// ANISOTROPIC, so an implementation that ignored either cannot pass by
/// accident: with the identity orientation and 1 mm spacing, a point in
/// pixel space and the same point in patient space differ by only the
/// origin, and a swap of the two Spacing values is invisible.
fn source_instance_with_oriented_geometry(
    sop_instance_uid: &str,
    spacing: &[u8],
    orientation: &[u8],
    position: &[u8],
    pixels: &[u8; 4],
) -> Vec<u8> {
    let mut out = source_instance(sop_instance_uid, pixels);
    out.extend_from_slice(&element(0x0028, 0x0030, b"DS", &ds_pad(spacing)));
    // (0020,0037) Image Orientation Patient: the row cosines, then the
    // column cosines. This is the element the conversion reads.
    out.extend_from_slice(&element(0x0020, 0x0037, b"DS", &ds_pad(orientation)));
    out.extend_from_slice(&element(0x0020, 0x0032, b"DS", &ds_pad(position)));
    out
}

#[test]
fn a_structure_set_is_stored_with_the_four_geometric_pins_intact() {
    // The happy path, and the test that pins P1-P4 on a SINGLE fixture:
    // two ROIs, one contour each, on two differently oriented and
    // differently spaced slices, so every pin is exercised on real
    // numbers rather than on a degenerate case.
    let python = require_python!();
    let scratch = Scratch::new("rtstruct-happy");

    // Slice 1: 2.0 mm row / 0.5 mm column spacing, row cosines (0,1,0),
    // column cosines (-1,0,0), origin (10,-20,30).
    let first = source_instance_with_oriented_geometry(
        "1.2.3.4.5.1",
        b"2.0\\0.5",
        b"0\\1\\0\\-1\\0\\0",
        b"10\\-20\\30",
        &[1, 2, 3, 4],
    );
    // Slice 2: 1 mm isotropic identity, origin (0,0,40).
    let second = source_instance_with_oriented_geometry(
        "1.2.3.4.5.2",
        b"1.0\\1.0",
        b"1\\0\\0\\0\\1\\0",
        b"0\\0\\40",
        &[5, 6, 7, 8],
    );

    let roi_one = [[12.5, 40.0], [13.0, 41.5], [11.0, 42.0]];
    let roi_two = [[12.5, 40.0], [13.0, 41.5], [11.0, 42.0]];
    let script = scratch.script(
        "contour.py",
        &result_script(&rtstruct_body(&[
            ("LUNG_L", "ORGAN", &[(1, &roi_one)]),
            ("LUNG_R", "PTV", &[(2, &roi_two)]),
        ])),
    );
    let conf = rtstruct_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(
        Answers::new(&[first, second], &["1.2.3.4.5.1", "1.2.3.4.5.2"]).then(success_tail(1)),
    );
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let sets = objects_of_class(&stored, RTSTRUCT_CLASS);
    if sets.is_empty() {
        let paths: Vec<String> = fake
            .requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect();
        panic!("no structure set stored; requests were {paths:?}");
    }
    assert_eq!(sets.len(), 1, "one structure set per study, not per frame");
    let object = sets[0];

    // -- The IOD itself -------------------------------------------------
    assert_eq!(
        sop_class_uid(object).as_deref(),
        Some(RTSTRUCT_CLASS),
        "(0008,0016)"
    );
    assert!(
        tag_text(object, 0x0008, 0x0060).is_some_and(|v| v == "RTSTRUCT"),
        "(0008,0060) Modality is RTSTRUCT (C.8.8.1.1)"
    );
    assert!(
        tag_text(object, 0x0008, 0x0005).is_some_and(|v| v == "ISO_IR 192"),
        "(0008,0005) Specific Character Set"
    );
    // An RT Structure Set has no pixel data at all: no Image Pixel module,
    // and no Number of Frames.
    assert!(
        pixel_data(object).is_none(),
        "a structure set carries no Pixel Data"
    );
    assert!(
        tag_text(object, 0x0028, 0x0008).is_none(),
        "no (0028,0008) Number of Frames"
    );
    assert!(
        tag_text(object, 0x0028, 0x0010).is_none(),
        "no (0028,0010) Rows"
    );

    // -- P1: ContourNumber is present, and unique within its sequence ---
    let numbers = all_values(object, 0x3006, 0x0048);
    assert_eq!(
        numbers,
        vec!["1", "1"],
        "P1: one ContourNumber per contour, restarting per ROI"
    );

    // -- P2: ROIDisplayColor is an IS triplet of integers 0..=255 ------
    let colors = all_values(object, 0x3006, 0x002A);
    assert_eq!(colors.len(), 2, "P2: one colour per ROI");
    for color in &colors {
        let channels: Vec<&str> = color.split('\\').collect();
        assert_eq!(channels.len(), 3, "P2: three channels in {color:?}");
        for channel in channels {
            assert!(
                !channel.contains('.'),
                "P2: a channel is an integer, not a decimal: {color:?}"
            );
            let value: u32 = channel.parse().unwrap_or(u32::MAX);
            assert!(value <= 255, "P2: a channel is 0..=255: {color:?}");
        }
    }
    assert_eq!(
        colors[0], "255\\0\\0",
        "P2: the ROI's own colour, not a default"
    );

    // -- P3: ContourData is patient millimetres through Eq C.7.6.2.1-1 -
    //
    // The oracle is the equation itself, evaluated here from the same
    // numbers the fixture declared. An implementation that swapped the
    // two Spacing values, ignored the orientation, or copied pixel
    // indices through cannot reproduce these numbers.
    let data = all_values(object, 0x3006, 0x0050);
    assert_eq!(data.len(), 2, "P3: one ContourData per contour");
    let expected_first = patient_polygon(
        &roi_one,
        [2.0, 0.5],
        [0.0, 1.0, 0.0, -1.0, 0.0, 0.0],
        [10.0, -20.0, 30.0],
    );
    let expected_second = patient_polygon(
        &roi_two,
        [1.0, 1.0],
        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 40.0],
    );
    assert_eq!(
        data[0], expected_first,
        "P3: ROI 1's contour in patient mm, through the non-identity orientation"
    );
    assert_eq!(
        data[1], expected_second,
        "P3: ROI 2's contour in patient mm"
    );
    // The z of every point is the referenced slice's own z.
    for (text, z) in [(&data[0], 30.0), (&data[1], 40.0)] {
        for triple in text.split('\\').collect::<Vec<&str>>().chunks(3) {
            let value: f64 = triple[2].parse().expect("z parses");
            assert!(
                (value - z).abs() < 1e-6,
                "P3: z {value} is the referenced slice's z {z}: {text:?}"
            );
        }
    }

    // -- P4: the reference chain 3006,0010 -> 0012 -> 0014 -> 0016 ------
    //
    // The chain must NEST, so the four headers appear in ascending order
    // and the innermost class is the INSTANCE class while the study item's
    // is the STUDY class (C.8.8.5.4: "the SOP Class UID ... is that of the
    // Study itself, not of the Instances within the Study").
    let chain = [
        (0x3006u16, 0x0010u16),
        (0x3006, 0x0012),
        (0x3006, 0x0014),
        (0x3006, 0x0016),
    ];
    let mut positions = Vec::new();
    for (group, element) in chain {
        let at = find_subslice(
            object,
            &[group.to_le_bytes(), element.to_le_bytes()].concat(),
        )
        .unwrap_or_else(|| panic!("P4: ({group:04X},{element:04X}) is missing"));
        positions.push(at);
    }
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "P4: the chain headers are in ascending order: {positions:?}"
    );
    // The study item's class is the STUDY class, never CT Image Storage.
    let study_at = positions[1];
    let after_study = &object[study_at..];
    let class_at = find_subslice(after_study, &[0x08, 0x00, 0x50, 0x11])
        .map(|offset| study_at + offset)
        .expect("P4: the study item names a class");
    let study_class = read_ui_at(object, class_at);
    assert_eq!(
        study_class, "1.2.840.10008.3.1.2.3.1",
        "P4: the OUTER reference names the Study SOP Class, which is \
         Detached Study Management - NOT Modality Worklist FIND \
         (1.2.840.10008.3.1.1.1), a query model"
    );
    assert_ne!(
        study_class, "1.2.840.10008.3.1.1.1",
        "P4: Modality Worklist FIND is a query model, not the Study class"
    );
    assert_ne!(
        study_class, CT_IMAGE_STORAGE,
        "P4: CT Image Storage in the outer position is the C.8.8.5.4 error"
    );
    // Every class named at or after the inner sequence is the class the
    // FIXTURE declares in (0008,0016) - the writer copies the frame's own
    // instance class, and CT Image Storage is only the fallback for a frame
    // that declares none.
    let inner = &object[positions[3]..];
    let mut from = 0;
    let mut inner_classes = 0;
    while let Some(at) = find_subslice(&inner[from..], &[0x08, 0x00, 0x50, 0x11]) {
        let at = from + at;
        from = at + 4;
        let class = read_ui_at(inner, at);
        assert_eq!(
            class, FIXTURE_CLASS,
            "P4: the inner items name the instance's own class"
        );
        inner_classes += 1;
    }
    assert!(
        inner_classes >= 2,
        "P4: both source slices are referenced (found {inner_classes})"
    );
    // The referenced slices are grouped by series, and both series appear.
    let text = String::from_utf8_lossy(object);
    assert!(text.contains("1.2.3.4.5.1"), "P4: slice 1 is referenced");
    assert!(text.contains("1.2.3.4.5.2"), "P4: slice 2 is referenced");

    // -- The ROI identity rows ------------------------------------------
    assert_eq!(
        all_values(object, 0x3006, 0x0026),
        vec!["LUNG_L", "LUNG_R"],
        "(3006,0026) ROI Name"
    );
    assert_eq!(
        all_values(object, 0x3006, 0x00A4),
        vec!["ORGAN", "PTV"],
        "(3006,00A4) RT ROI Interpreted Type"
    );
    assert_eq!(
        all_values(object, 0x3006, 0x0022),
        vec!["1", "2"],
        "(3006,0022) ROI Number is 1..N"
    );
    assert_eq!(
        all_values(object, 0x3006, 0x0042),
        vec!["CLOSED_PLANAR", "CLOSED_PLANAR"],
        "(3006,0042) Contour Geometric Type"
    );
    assert_eq!(
        all_values(object, 0x3006, 0x0046),
        vec!["3", "3"],
        "(3006,0046) Number of Contour Points"
    );
    // Each reference back to an ROI resolves to a written ROI number.
    for reference in all_values(object, 0x3006, 0x0084) {
        assert!(
            ["1", "2"].contains(&reference.as_str()),
            "({reference}) is not a written ROI number"
        );
    }
    assert!(
        tag_text(object, 0x3006, 0x0002).is_some_and(|v| v == "proc-3"),
        "(3006,0002) Structure Set Label is derived from the model id and profile"
    );
    // -- (0020,0011) Series Number: the three series in one study differ --
    //
    // A viewer lists a study's series BY NUMBER, so a result series that
    // reuses the source's number is a row a reader cannot tell from the
    // source. The scheme is CT=1, SEG=2, RTSTRUCT=3.
    assert_eq!(
        tag_text(object, 0x0020, 0x0011).as_deref(),
        Some("3"),
        "(0020,0011) Series Number is 3, the RTSTRUCT's own number"
    );
    assert_ne!(
        tag_text(object, 0x0020, 0x0011).as_deref(),
        Some("1"),
        "the structure set must not reuse the source CT's series number"
    );
    assert_ne!(
        "3",
        clinfer::seg::SERIES_NUMBER,
        "the RTSTRUCT series must not collide with the SEG series"
    );
    assert_ne!(
        clinfer::seg::SERIES_NUMBER,
        "1",
        "the SEG series must not collide with the source CT either"
    );
    // The Frame of Reference is the source's, and no (0008,001B).
    assert!(
        find_subslice(object, &[0x08, 0x00, 0x1B, 0x00]).is_none(),
        "(0008,001B) must not be written"
    );
}

#[test]
fn a_known_pixel_pair_lands_at_the_known_patient_point() {
    // The wire pin for the 24.09 transpose finding. The ABI's point is
    // [column, row]; Equation C.7.6.2.1-1 turns it into patient millimetres
    // as P = S + column * dc * row_cosines + row * dr * column_cosines.
    //
    // With a KNOWN pair the equation is arithmetic a reader can do in their
    // head: column 4 at 0.16 mm/column is 0.64 mm of x, row 0 is no y, and
    // the origin carries z. The pair [4, 0] is deliberately asymmetric so a
    // transposed converter - the producer bug the 24.09 run found - stores
    // [0, 0.64] instead and fails on the x value.
    let python = require_python!();

    let scratch = Scratch::new("rtstruct-known-point");
    // Identity orientation, origin at the z below, 0.16 mm in both
    // directions: nothing but the column contribution moves x.
    let source = source_instance_with_oriented_geometry(
        "1.2.3.4.5.1",
        b"0.16\\0.16",
        b"1\\0\\0\\0\\1\\0",
        b"0\\0\\23.84",
        &[1, 2, 3, 4],
    );
    // The ABI and the schema require at least three points for a closed
    // polygon, so the fixture carries three - but each is chosen so the
    // equation is arithmetic a reader can do in their head, and the
    // assertion reads the FIRST point's triple.
    let three_points: [[f64; 2]; 3] = [[4.0, 0.0], [5.0, 0.0], [4.0, 1.0]];
    let script = scratch.script(
        "contour.py",
        &result_script(&rtstruct_body(&[(
            "MANDIBLE",
            "ORGAN",
            &[(1, &three_points)],
        )])),
    );
    let conf = rtstruct_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(Answers::new(&[source], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let sets = objects_of_class(&stored, RTSTRUCT_CLASS);
    assert_eq!(sets.len(), 1, "one structure set per study");
    let data = all_values(sets[0], 0x3006, 0x0050);
    assert_eq!(data.len(), 1, "one contour, so one ContourData");

    // The oracle is the equation itself, evaluated here from the fixture's
    // own numbers rather than read back from the object.
    let expected = patient_polygon(
        &three_points,
        [0.16, 0.16],
        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 23.84],
    );
    assert_eq!(
        data[0], expected,
        "(3006,0050) must be the equation's output for each [column, row] pair"
    );
    // The first point is [column, row] = [4, 0]: column 4 at 0.16 mm/column
    // is x = 0.64, row 0 is y = 0, and z is the origin. A transposed
    // converter stores 0\0.64\23.84 for this point instead, which is what
    // makes this the wire pin for the producer transpose.
    let first: Vec<&str> = data[0].split('\\').take(3).collect();
    assert_eq!(
        first,
        vec!["0.64", "0", "23.84"],
        "the first point's triple: column 4 at 0.16 mm is x=0.64, row 0 is \
         y=0, and z is the origin"
    );
}

#[test]
fn a_structure_set_names_the_model_that_produced_it() {
    // (0008,2111) Derivation Description is the ONE provenance channel the
    // A.19 modules offer: unlike SEG there is no segment-algorithm
    // attribute, so the model id is recorded here or nowhere.
    let python = require_python!();
    let scratch = Scratch::new("rtstruct-derivation");
    let first = source_instance_with_oriented_geometry(
        "1.2.3.4.5.1",
        b"1.0\\1.0",
        b"1\\0\\0\\0\\1\\0",
        b"0\\0\\0",
        &[1, 2, 3, 4],
    );
    let script = scratch.script(
        "contour.py",
        &result_script(&rtstruct_body(&[(
            "LUNG",
            "ORGAN",
            &[(1, &[[1.0, 1.0], [2.0, 1.0], [2.0, 2.0]])],
        )])),
    );
    let conf = rtstruct_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(Answers::new(&[first], &["1.2.3.4.5.1"]).then(success_tail(1)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(matches!(outcome, worker::Processed::Claimed), "{outcome:?}");

    let stored = stored_objects(&fake);
    let object = objects_of_class(&stored, RTSTRUCT_CLASS)
        .first()
        .copied()
        .expect("a structure set was stored")
        .clone();
    assert_eq!(
        tag_text(&object, 0x0008, 0x2111).as_deref(),
        Some("proc"),
        "(0008,2111) Derivation Description names the model that ran"
    );
    // (3006,0004) Structure Set Name is Type 3 and this rule names no series
    // description, so the writer OMITS it rather than inventing a name.
    assert!(
        tag_text(&object, 0x3006, 0x0004).is_none(),
        "(3006,0004) is omitted when no description is configured"
    );
    // (3006,0002) Structure Set Label is Type 1 and derived from the model
    // id and profile version ("proc-3"), NOT from the ABI's own label.
    assert_eq!(
        tag_text(&object, 0x3006, 0x0002).as_deref(),
        Some("proc-3"),
        "(3006,0002) Structure Set Label is the model id plus the profile"
    );
    // (0008,0070) Manufacturer is the fixed producer name.
    assert_eq!(
        tag_text(&object, 0x0008, 0x0070).as_deref(),
        Some("clinfer"),
        "(0008,0070) Manufacturer"
    );
}

#[test]
fn a_contour_on_a_frame_the_study_does_not_have_cancels_the_cycle() {
    // The pre-store gate: a contour naming a frame outside the study is a
    // contract violation and is refused BEFORE anything is stored, so the
    // study cannot end up with a structure set missing the contours that
    // fell outside it.
    let python = require_python!();
    let scratch = Scratch::new("rtstruct-bad-frame");
    let first = source_instance_with_oriented_geometry(
        "1.2.3.4.5.1",
        b"1.0\\1.0",
        b"1\\0\\0\\0\\1\\0",
        b"0\\0\\0",
        &[1, 2, 3, 4],
    );
    // The study has ONE frame; the contour names frame 7.
    let script = scratch.script(
        "contour.py",
        &result_script(&rtstruct_body(&[(
            "LUNG",
            "ORGAN",
            &[(7, &[[1.0, 1.0], [2.0, 1.0], [2.0, 2.0]])],
        )])),
    );
    let conf = rtstruct_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(Answers::new(&[first], &["1.2.3.4.5.1"]).then(failure_tail(0)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let outcome = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    // The cycle is cancelled rather than stored, and nothing of the
    // structure set class reached the origin.
    assert!(
        !matches!(outcome, worker::Processed::Claimed),
        "a bad frame reference cancels the cycle: {outcome:?}"
    );
    assert!(
        objects_of_class(&stored_objects(&fake), RTSTRUCT_CLASS).is_empty(),
        "nothing is stored when the gate refuses"
    );
}

#[test]
fn a_structure_set_with_no_rois_is_not_stored() {
    // `has_rtstruct()` is false for a block whose ROI list is empty, so
    // the cycle stores nothing of this class. The schema refuses the shape
    // outright, which is why this test asserts the EMPTY result rather
    // than a cancellation.
    let python = require_python!();
    let scratch = Scratch::new("rtstruct-empty");
    let first = source_instance_with_oriented_geometry(
        "1.2.3.4.5.1",
        b"1.0\\1.0",
        b"1\\0\\0\\0\\1\\0",
        b"0\\0\\0",
        &[1, 2, 3, 4],
    );
    // A result that carries no rtstruct block at all.
    let script = scratch.script(
        "contour.py",
        &result_script(
            r#"{"protocol": {"name": "clinfer-abi", "version": 1},
                "producer": {"script": "contour.py"}}"#,
        ),
    );
    let conf = rtstruct_conf(scratch.path(), &script, &python);
    let fake = FakeOrigin::start(Answers::new(&[first], &["1.2.3.4.5.1"]).then(success_tail(0)));
    let counters = std::sync::Arc::new(Counters::default());
    let (channels, models) = topology(&origin(&fake), &conf, &counters);
    let _ = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
    for handle in models.join() {
        let _ = handle.join();
    }
    assert!(
        objects_of_class(&stored_objects(&fake), RTSTRUCT_CLASS).is_empty(),
        "no structure set without an rtstruct block"
    );
}

#[test]
fn the_structure_set_uid_is_derived_from_the_study_and_the_model() {
    // The UID is a digest over the study, the model and the profile
    // version, so a rerun reproduces the same object rather than adding a
    // second one. Two runs of the same study must produce the same UID.
    let python = require_python!();
    let scratch = Scratch::new("rtstruct-uid");
    let first = source_instance_with_oriented_geometry(
        "1.2.3.4.5.1",
        b"1.0\\1.0",
        b"1\\0\\0\\0\\1\\0",
        b"0\\0\\0",
        &[1, 2, 3, 4],
    );
    let script = scratch.script(
        "contour.py",
        &result_script(&rtstruct_body(&[(
            "LUNG",
            "ORGAN",
            &[(1, &[[1.0, 1.0], [2.0, 1.0], [2.0, 2.0]])],
        )])),
    );
    let conf = rtstruct_conf(scratch.path(), &script, &python);

    let mut uids = Vec::new();
    for _ in 0..2 {
        let fake = FakeOrigin::start(
            Answers::new(std::slice::from_ref(&first), &["1.2.3.4.5.1"]).then(success_tail(1)),
        );
        let counters = std::sync::Arc::new(Counters::default());
        let (channels, models) = topology(&origin(&fake), &conf, &counters);
        let _ = worker::process(&origin(&fake), &conf, "1.2.3", &counters, &channels);
        for handle in models.join() {
            let _ = handle.join();
        }
        let stored = stored_objects(&fake);
        let object = objects_of_class(&stored, RTSTRUCT_CLASS)
            .first()
            .copied()
            .expect("a structure set was stored")
            .clone();
        uids.push(sop_instance_uid(&object).expect("(0008,0018)"));
    }
    assert_eq!(uids[0], uids[1], "the UID is stable across reruns");
    // The UID is the crate's 128-bit digest rendered under the registered 2.25
    // root - the same convention the SEG, SR and STL writers use. A bare
    // decimal digest is syntactically a UID but sits in no org root.
    assert!(
        uids[0]
            .strip_prefix("2.25.")
            .is_some_and(|digits| digits.chars().all(|c| c.is_ascii_digit())),
        "the UID is a 2.25-rooted decimal digest: {}",
        uids[0]
    );
    assert!(
        (1..=64).contains(&uids[0].len()),
        "a UID is 1..=64 characters: {}",
        uids[0].len()
    );
}

/// `(0008,1150)` of a reference item, read at a known offset.
///
/// The VR is UI and the value is a UID, so this reads the short-form
/// header the way the fixture writes it.
fn read_ui_at(object: &[u8], at: usize) -> String {
    let vr = &object[at + 4..at + 6];
    assert_eq!(vr, b"UI", "a referenced SOP Class UID is UI");
    let length = u16::from_le_bytes([object[at + 6], object[at + 7]]) as usize;
    String::from_utf8_lossy(&object[at + 8..at + 8 + length])
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

/// Every value of one tag, in document order.
///
/// Unlike `tag_text`, this scans the WHOLE object for repeated tags: a
/// sequence writes the same tag once per item, and a test for uniqueness
/// has to see all of them.
fn all_values(object: &[u8], group: u16, element: u16) -> Vec<String> {
    let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
    let mut values = Vec::new();
    let mut from = 0;
    while let Some(at) = find_subslice(&object[from..], &needle) {
        let at = from + at;
        from = at + 4;
        let vr = &object[at + 4..at + 6];
        let (length, head) = if matches!(vr, b"OB" | b"OW" | b"SQ" | b"UT" | b"UN") {
            (
                u32::from_le_bytes([
                    object[at + 8],
                    object[at + 9],
                    object[at + 10],
                    object[at + 11],
                ]) as usize,
                12,
            )
        } else {
            (
                u16::from_le_bytes([object[at + 6], object[at + 7]]) as usize,
                8,
            )
        };
        // A sequence header has an undefined length and no inline value;
        // skip it rather than reading past the object.
        if length == 0xFFFF_FFFF || length > 4096 {
            continue;
        }
        if let Some(value) = object.get(at + head..at + head + length) {
            values.push(
                String::from_utf8_lossy(value)
                    .trim_end_matches('\0')
                    .trim()
                    .to_string(),
            );
        }
    }
    values
}

/// CT Image Storage, the class every contour's source slice carries.
const CT_IMAGE_STORAGE: &str = "1.2.840.10008.5.1.4.1.1.2";

/// The SOP Class the `source_instance` fixture declares in (0008,0016).
///
/// Digital X-Ray Image Storage - For Presentation. The writer copies the
/// frame's OWN class into the inner reference items, so this is what the
/// pins must expect; CT Image Storage is only the writer's fallback for a
/// frame that declares no class at all.
const FIXTURE_CLASS: &str = "1.2.840.10008.5.1.4.1.1.1.1";

/// Equation C.7.6.2.1-1 applied here, independently of the crate.
///
///   P = S + column * dc * row_cosines + row * dr * column_cosines
///
/// where `dc` is the COLUMN resolution (`Spacing[1]`) and `dr` the ROW
/// resolution (`Spacing[0]`). Written out separately from the crate's own
/// `to_patient` so that a bug in the crate cannot make this oracle agree
/// with it.
fn patient_polygon(
    points: &[[f64; 2]],
    spacing: [f64; 2],
    orientation: [f64; 6],
    position: [f64; 3],
) -> String {
    let dc = spacing[1];
    let dr = spacing[0];
    let mut values = Vec::new();
    for point in points {
        let (column, row) = (point[0], point[1]);
        for axis in 0..3 {
            let value =
                position[axis] + column * dc * orientation[axis] + row * dr * orientation[axis + 3];
            // The writer states a value exactly when its shortest
            // round-trip spelling fits the VR, and every fixture point is
            // chosen to land on such a value, so the oracle's `{}` matches
            // the stored text byte for byte. A point that did not fit would
            // fall back to three decimals in the writer, and the fixture
            // would need the same fallback here.
            values.push(format!("{value}"));
        }
    }
    values.join("\\")
}
