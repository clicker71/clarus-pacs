//! The TotalSegmentator producer: a synthetic mini-run against a stub engine.
//!
//! What this covers, and why it needs its own file rather than a case in
//! `abi_seam.rs`: the producer's job is not the spawn contract but the TWO
//! RESULT BLOCKS it derives from one inference pass, and the geometry it had
//! to read out of the manifest to get there. So the tests here
//!
//!   1. build a workdir whose frames DECLARE geometry (the newly published
//!      manifest fields) and check the producer uses it;
//!   2. run the real script against a stub engine that stands in for
//!      TotalSegmentator's CLI, returning a multilabel volume of a known
//!      shape and a run report of a known class map;
//!   3. read the result back through clinfer's own schema parser, which is
//!      the reader that decides whether the producer speaks the contract.
//!
//! The stub is a Python package named `totalsegmentator` planted beside the
//! producer script, so `-m totalsegmentator.bin.TotalSegmentator` resolves to
//! it. That keeps the producer's argument vector VERBATIM: nothing about the
//! real command line is stubbed out, only the program at the end of it.
//!
//! The planted package also shadows a real installation, which is deliberate:
//! these tests must never spend minutes running a real model on a real CT.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use clinfer::abi::external::{self, Outcome, Processor};
use clinfer::abi::schema::DEFAULT_MAX_RESULT_BYTES;
use clinfer::abi::workdir::{self, Frame};
use clinfer::receiver::Counters;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The side of the fixture's frames, in pixels.
///
/// 32 rather than a token 8: the contour discipline drops components below
/// `MIN_AREA_PX` (100 px, lifted from `RTSTRUCT-contours.py`), so on a
/// smaller grid every blob is dust and the rtstruct block comes out empty -
/// which would make "the contours are traced" untestable rather than true.
const SIDE: u64 = 32;

/// How many frames the fixture writes.
const FRAMES: usize = 4;

/// A directory that removes itself, so a failed assertion leaves nothing.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let unique = format!(
            "clinfer-totalseg-{label}-{}-{}",
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An interpreter that can ACTUALLY run the producer, or `None`.
///
/// Not the same question as "is there a python". The producer needs numpy,
/// nibabel and cv2, and the seam clears the child's environment down to PATH
/// and PYTHONUNBUFFERED - so an interpreter whose packages live in a user
/// site-packages reached through PYTHONPATH or PYTHONUSERBASE will import
/// them under an ordinary shell and FAIL under the seam. Checking with
/// `python --version` would therefore skip the test on a machine where it
/// cannot pass and, worse, RUN it on a machine where it cannot pass either.
///
/// The check imports the three modules with an environment as bare as the
/// seam's, so only an interpreter that genuinely carries them counts. A site
/// venv - the documented deployment - passes.
///
/// `CLINFER_TEST_PYTHON` names the interpreter explicitly, which is what a
/// real deployment should set: the site's venv is the intended value, and
/// guessing between `python` and `python3` is a convenience for a developer
/// box rather than a deployment rule.
fn producer_interpreter() -> Option<String> {
    if let Some(explicit) = std::env::var_os("CLINFER_TEST_PYTHON") {
        let path = PathBuf::from(explicit);
        if carries_producer_deps(&path) {
            return Some(path.display().to_string());
        }
        eprintln!(
            "CLINFER_TEST_PYTHON={} cannot import numpy, nibabel and cv2 with the \
             seam's cleared environment",
            path.display()
        );
        return None;
    }

    // A venv is the deployment shape, so it is tried before the system
    // interpreter: on Windows that is `.venv/Scripts/python.exe`, elsewhere
    // `.venv/bin/python`.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let root = Path::new(&manifest);
        for relative in [
            ".venv/Scripts/python.exe",
            ".venv/bin/python",
            "tmp/totalseg_venv/Scripts/python.exe",
            "tmp/totalseg_venv/bin/python",
        ] {
            candidates.push(root.join(relative));
            // The crate sits one level down from the workspace root.
            candidates.push(root.join("..").join(relative));
        }
    }
    for name in ["python", "python3"] {
        candidates.push(PathBuf::from(name));
    }

    for candidate in candidates {
        if carries_producer_deps(&candidate) {
            return Some(candidate.display().to_string());
        }
    }
    None
}

/// Whether this interpreter imports the producer's dependencies as the seam
/// would see them.
fn carries_producer_deps(interpreter: &Path) -> bool {
    Command::new(interpreter)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("PYTHONUNBUFFERED", "1")
        .arg("-c")
        .arg("import numpy, nibabel, cv2")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

macro_rules! require_python {
    () => {
        match producer_interpreter() {
            Some(found) => found,
            None => {
                eprintln!(
                    "skipping: no interpreter here imports numpy, nibabel and cv2 with \
                     the seam's cleared environment (a site venv does; a bare CI \
                     interpreter whose packages arrive via PYTHONPATH does not, \
                     because the seam does not pass PYTHONPATH on and should not). \
                     Set CLINFER_TEST_PYTHON to the site's venv python to run this."
                );
                return;
            }
        }
    };
}
/// A CT workdir whose frames declare the geometry the producer needs.
///
/// The geometry is not decoration: it is what lets the producer build an
/// honest affine, and "the producer could use it" is the point of publishing
/// these fields at all.
///
/// The frames are 32x32 rather than a token 8x8 because the contour
/// discipline drops components below `MIN_AREA_PX` (100 px, lifted from
/// `RTSTRUCT-contours.py`). On a smaller grid every blob is under the
/// threshold and the rtstruct block would come out empty - which would make
/// "the contours are traced" untestable rather than true.
fn ct_workdir(parent: &Path, modalities: &[&str]) -> workdir::Workdir {
    // 16 bits per sample, so two bytes each.
    let pixel = vec![0u8; (SIDE * SIDE * 2) as usize];
    let owned: Vec<Vec<u8>> = (0..FRAMES).map(|_| pixel.clone()).collect();
    let uids = ["1.2.3.1", "1.2.3.2", "1.2.3.3", "1.2.3.4"];
    let frames: Vec<Frame<'_>> = owned
        .iter()
        .enumerate()
        .map(|(at, bytes)| Frame {
            sop_uid: uids[at],
            bytes: bytes.as_slice(),
            width: SIDE as u32,
            height: SIDE as u32,
            bits: 16,
            signed: false,
            photometric: "MONOCHROME2",
            pixel_spacing: Some([0.5, 0.5]),
            // Deliberately CONSISTENT with the positions below (12.0 mm).
            // The drift case - a declared spacing that disagrees with the
            // stack actually sent - has its own fixture, because a fixture
            // that shipped the drift would make every other test depend on
            // which of the two the producer trusted.
            slice_spacing: Some(12.0),
            position: Some([-12.4, -102.0, at as f64 * 12.0]),
            orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
        })
        .collect();

    workdir::build(
        parent,
        "ct-study",
        clinfer::abi::StudyContext {
            uid: "1.2.840.113857.2209.1".to_string(),
            description: Some("JAW CT".to_string()),
            modalities: modalities.iter().map(|m| (*m).to_string()).collect(),
            body_part: Some("MANDIBLE".to_string()),
        },
        &frames,
        "0.3.0-alpha",
        1,
    )
    .expect("workdir")
}

/// A stub for TotalSegmentator's CLI module.
///
/// It parses the producer's own argument vector, writes a multilabel nifti on
/// the input grid, and writes the run report whose `classes` map is the ONLY
/// source of organ ids.
///
/// The mask layout is fixed and known: class 9 down the first column of every
/// frame, class 5 as a 3x3 block on frame 0 only, class 12 absent entirely -
/// so "the third organ has no contours" is an assertion about the produced
/// result rather than about the stub's mood.
///
/// The class map mirrors the REAL `craniofacial_structures` task, which
/// carries all three organs the site declares. A map missing one would be a
/// different test (the producer refuses that case, which
/// `a_task_missing_a_declared_organ_is_refused` covers).
///
/// The class IDS are deliberately not the obvious 1/2/3: a producer that
/// hard-coded ids instead of reading this map would fail these tests.
const STUB_ENGINE: &str = r#"
import json, pathlib, sys

import nibabel as nib
import numpy as np

argv = sys.argv[1:]
options = {}
at = 0
takes_value = ("-i", "--input", "-o", "--output", "-ta", "--task",
               "-d", "--device", "--report")
while at < len(argv):
    token = argv[at]
    if token in takes_value:
        options[token] = argv[at + 1]
        at += 2
    elif token.startswith("-"):
        options[token] = True
        at += 1
    else:
        at += 1

source = nib.load(options["-i"])
volume = np.asanyarray(source.dataobj)
shape = volume.shape

# The affine's z column and the volume's value range are what the geometry
# tests read: the first says which slice spacing the producer trusted, the
# second says whether it read the pixels in the source's own signedness.
affine = np.asanyarray(source.affine)
sys.stderr.write("STUB_AFFINE_Z=%r\n" % float(affine[2, 2]))
sys.stderr.write("STUB_VOXEL_MIN=%d\n" % int(volume.min()))
sys.stderr.write("STUB_VOXEL_MAX=%d\n" % int(volume.max()))

classes = {"5": "mandible", "9": "teeth_lower", "12": "skull",
           "7": "teeth_upper"}

predicted = np.zeros(shape, dtype=np.uint8)
predicted[:, 0:8, :] = 9
# An in-plane 12x12 patch at the first slice, painted in the x-y-z
# layout so it stays a per-frame patch under any axis arrangement.
predicted[10:22, 10:22, 0] = 5

# The CLI's own flags, echoed for the assertions about the command line.
sys.stderr.write("STUB_CLASSES=%s\n" % json.dumps(classes, sort_keys=True))
sys.stderr.write("STUB_INPUT_SHAPE=%s\n" % json.dumps(list(shape)))
sys.stderr.write("STUB_TASK=%s\n" % options.get("-ta"))
sys.stderr.write("STUB_ML=%s\n" % ("--ml" in options))
sys.stderr.write("STUB_DEVICE=%s\n" % options.get("-d"))
sys.stderr.write("STUB_FAST=%s\n" % ("--fast" in options))

nib.save(nib.Nifti1Image(predicted, source.affine), options["-o"])
pathlib.Path(options["--report"]).write_text(json.dumps({
    "task": options.get("-ta"),
    "classes": classes,
    "totalsegmentator_version": "2.18.0-stub",
    "device": options.get("-d", "cpu"),
    "output_files": {"segmentation": options["-o"]},
}))
"#;

/// A checking engine: it refuses any input that is not LAS-oriented with
/// the slice stack on the z axis. The engine's DICOM reader reorients to
/// LAS, and the 24.09 live run proved the model silently returns all-zero
/// masks on any other layout - while every unit test stayed green, because
/// the stub engine accepts any grid. This stub is the wire pin for that
/// contract: the producer must hand over exactly the reader's layout.
const STUB_ENGINE_LAS_CHECK: &str = r#"
import json
import pathlib
import sys

import nibabel as nib
import numpy as np

argv = sys.argv[1:]
opts = {}
takes = ("-i", "-o", "-ta", "-d", "--report")
at = 0
while at < len(argv):
    token = argv[at]
    if token in takes:
        opts[token] = argv[at + 1]
        at += 2
    else:
        at += 1

img = nib.load(opts["-i"])
codes = tuple(nib.orientations.aff2axcodes(img.affine))
sys.stderr.write("STUB_AXCODES=%s\n" % (codes,))
shape = np.asanyarray(img.dataobj).shape
sys.stderr.write("STUB_SHAPE=%s\n" % (shape,))
if codes != ("L", "A", "S"):
    sys.stderr.write("STUB_REFUSED: not LAS\n")
    sys.exit(3)
if len(shape) != 3 or shape[2] != 4 or shape[0] != shape[1]:
    sys.stderr.write("STUB_REFUSED: the slices are not on the z axis\n")
    sys.exit(3)

classes = {"5": "mandible", "9": "teeth_lower", "12": "skull", "7": "teeth_upper"}
predicted = np.zeros(shape, dtype=np.uint8)
predicted[:, 0:8, :] = 9
predicted[10:22, 10:22, 0] = 5
nib.save(nib.Nifti1Image(predicted, img.affine), opts["-o"])
pathlib.Path(opts["--report"]).write_text(json.dumps({
    "task": opts.get("-ta"),
    "classes": classes,
    "totalsegmentator_version": "2.18.0-las-stub",
    "device": opts.get("-d", "cpu"),
    "output_files": {"segmentation": opts["-o"]},
}))
"#;

/// Plant the stub package beside the producer and return the script's path.
///
/// The seam clears the child's environment down to PATH and
/// PYTHONUNBUFFERED, so `PYTHONPATH` cannot be injected from here. Python
/// puts the SCRIPT's own directory on `sys.path`, so a `totalsegmentator`
/// package sitting next to `RTSTRUCT-CT-totalseg.py` is what resolves the `-m`.
///
/// This also SHADOWS a real installed TotalSegmentator, which is the point:
/// a real 0.5 mm run on a real CT would take minutes and prove nothing about
/// the result blocks.
/// Stage the crate's shared parameter reader into the scratch directory.
///
/// The producer imports it by name, and an interpreter resolves that import
/// against the SCRIPT's own directory, so a producer staged alone could not
/// reach the reader it uses.
fn stage_reader(scratch: &Scratch) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("abi")
        .join("params.py");
    let body = std::fs::read_to_string(&source).expect("the shared parameter reader");
    std::fs::write(scratch.path().join("params.py"), body).expect("staged reader");
}

fn staged_producer(scratch: &Scratch, body: &str) -> PathBuf {
    let package = scratch.path().join("totalsegmentator");
    std::fs::create_dir_all(package.join("bin")).expect("package");
    std::fs::write(package.join("__init__.py"), b"").expect("init");
    std::fs::write(package.join("bin").join("__init__.py"), b"").expect("init");
    std::fs::write(package.join("bin").join("TotalSegmentator.py"), STUB_ENGINE)
        .expect("stub engine");
    stage_reader(scratch);

    let script = scratch.path().join("RTSTRUCT-CT-totalseg.py");
    std::fs::write(&script, body).expect("stage producer");
    script
}

/// The producer's real body, staged beside the stub.
fn real_producer(scratch: &Scratch) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("abi")
        .join("RTSTRUCT-CT-totalseg.py");
    let body = std::fs::read_to_string(&source).expect("the producer script");
    staged_producer(scratch, &body)
}

/// Like `staged_producer`, but the engine planted beside the producer is
/// the caller's own body rather than the permissive one. Stages the real
/// producer DIRECTLY: going through `real_producer` would call
/// `staged_producer` back and overwrite the custom engine.
fn staged_producer_with_engine(scratch: &Scratch, engine_body: &str) -> PathBuf {
    let package = scratch.path().join("totalsegmentator");
    std::fs::create_dir_all(package.join("bin")).expect("package");
    std::fs::write(package.join("__init__.py"), b"").expect("init");
    std::fs::write(package.join("bin").join("__init__.py"), b"").expect("init");
    std::fs::write(package.join("bin").join("TotalSegmentator.py"), engine_body)
        .expect("stub engine");
    stage_reader(scratch);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("abi")
        .join("RTSTRUCT-CT-totalseg.py");
    let body = std::fs::read_to_string(&source).expect("the producer script");
    let script = scratch.path().join("RTSTRUCT-CT-totalseg.py");
    std::fs::write(&script, body).expect("stage producer");
    script
}

fn processor(interpreter: &str, script: &Path) -> Processor {
    Processor {
        id: "totalseg".to_string(),
        interpreter: PathBuf::from(interpreter),
        script: script.to_path_buf(),
        // The producer spawns the engine itself, so the seam's timeout has to
        // cover that child too. 120 s is far more than the stub needs and far
        // less than a real run, which is what this test is NOT about.
        timeout_secs: 120,
        max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
    }
}

fn counters() -> Counters {
    Counters::default()
}

/// One producer run's report plus the stderr the seam handed the callback.
///
/// The report is the seam's verdict; the stderr is the CHILD's own voice.
/// The engine stub echoes what it parsed there, so a test that wants to know
/// which affine or dtype the producer actually chose has to read that, not
/// the report - a report says "success" and nothing about the volume it
/// produced.
struct RunAndStderr {
    report: external::RunReport,
    stderr: String,
}

impl RunAndStderr {
    /// The seam's verdict.
    fn outcome(&self) -> &Outcome {
        &self.report.outcome
    }

    /// The parsed result, taken by value: a `ResultFile` is not `Copy` and
    /// a test that needs it is the last reader.
    fn take_result(&mut self) -> clinfer::abi::schema::ResultFile {
        self.report.result.take().expect("a parsed result")
    }

    /// Why the run ended as it did, for the log.
    fn diagnosis(&self) -> Option<String> {
        self.report.diagnosis()
    }

    /// A value the child echoed on its stderr.
    ///
    /// This is the CHILD's voice rather than the seam's verdict: the stub
    /// reports the affine and the voxel range it was handed, which is what
    /// proves the producer chose them rather than merely accepting them.
    fn echoed(&self, prefix: &str) -> String {
        let line = self
            .stderr
            .lines()
            .find(|line| line.contains(prefix))
            .unwrap_or_else(|| panic!("the stub must echo {prefix}: {}", self.stderr));
        line.split_once(prefix)
            .expect("the prefix is present")
            .1
            .trim()
            .to_string()
    }
}

fn run_producer(interpreter: &str, script: &Path, built: &workdir::Workdir) -> RunAndStderr {
    let mut captured = String::new();
    let report = external::run(
        &processor(interpreter, script),
        &built.path,
        &counters(),
        &mut external::ChildMetrics::new(&counters(), "totalseg-test"),
        |line| {
            // The seam hands every stderr line here. Echoing it is what makes
            // a failure diagnosable from the test log alone: the producer's
            // own message names the missing dependency, the bad task, or the
            // malformed manifest, and without this the log would say only
            // "exit 1".
            eprintln!("[producer] {line}");
            captured.push_str(line);
            captured.push('\n');
        },
    )
    .expect("the seam runs the producer");
    RunAndStderr {
        report,
        stderr: captured,
    }
}

#[test]
fn one_run_emits_an_rtstruct_block_and_no_seg_block() {
    let python = require_python!();

    let scratch = Scratch::new("rtstruct-only");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = real_producer(&scratch);

    let mut run = run_producer(&python, &script, &built);
    assert_eq!(
        *run.outcome(),
        Outcome::Success,
        "the producer must succeed: {:?}",
        run.diagnosis()
    );
    let result = run.take_result();

    // The producer block names the model and its version, which is what
    // makes a stored result reproducible.
    assert_eq!(result.producer.model.as_deref(), Some("TotalSegmentator"));
    assert_eq!(
        result.producer.model_version.as_deref(),
        Some("2.18.0-stub")
    );

    // -- no seg block ----------------------------------------------------
    //
    // The seg half was dropped after the 24.09 Weasis finding: a conformant,
    // geometrically correct SEG rendered as garbage in Weasis v4.7.2, and an
    // independently built reference object rendered identically, so the
    // defect is viewer-side. This producer now emits the rtstruct block
    // alone, which is what makes the whole SEG half a dead weight to drop.
    assert!(
        result.seg.is_none(),
        "the producer emits no seg block: the seg half was dropped"
    );

    // -- the rtstruct block ----------------------------------------------
    let rs = result.rtstruct.expect("an rtstruct block");
    assert_eq!(
        rs.rois.len(),
        2,
        "teeth_upper has no pixels anywhere in the stub's volume, so it is \
         dropped from rois"
    );
    assert_eq!(rs.rois[0].name, "MANDIBLE");
    assert_eq!(rs.rois[0].interpreted_type, "ORGAN");
    assert_eq!(rs.rois[0].generation_algorithm, "AUTOMATIC");
    // The mandible is a 12x12 block on frame 0 only, so exactly one contour.
    assert_eq!(rs.rois[0].contours.len(), 1);
    assert_eq!(rs.rois[0].contours[0].frame, 1);
    // The teeth column runs down every frame, so four contours.
    assert_eq!(rs.rois[1].name, "TEETH_LOWER");
    assert_eq!(rs.rois[1].contours.len(), 4);
    assert!(
        !rs.rois.iter().any(|roi| roi.name == "TEETH_UPPER"),
        "an organ with no contours anywhere is dropped from rois"
    );
}

#[test]
fn the_dropped_seg_half_leaves_no_files_in_the_workdir() {
    // The seg half was not only a result block: it also WROTE one
    // `seg-NNNN.raw` mask file per frame into the workdir. Dropping the
    // block without dropping the files would leave the worker's workdir
    // holding bytes nothing claims, and a later reader that globbed the
    // directory would find masks for a block that no longer exists.
    let python = require_python!();

    let scratch = Scratch::new("no-seg-files");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = real_producer(&scratch);

    let mut run = run_producer(&python, &script, &built);
    assert_eq!(*run.outcome(), Outcome::Success, "{:?}", run.diagnosis());
    let result = run.take_result();
    assert!(result.seg.is_none(), "no seg block");
    assert!(
        result.rtstruct.is_some(),
        "the rtstruct block is still there"
    );

    // The producer's own directory listing must carry no seg-*.raw file.
    let leftovers: Vec<String> = std::fs::read_dir(built.path)
        .expect("the workdir listing")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("seg-") && name.ends_with(".raw"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the dropped seg half must leave no mask files behind: {leftovers:?}"
    );
}

#[test]
fn the_engine_is_invoked_with_the_task_multilabel_and_no_fast() {
    let python = require_python!();

    let scratch = Scratch::new("engine-argv");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = real_producer(&scratch);

    // The real producer swallows the engine's stderr, so the argv is checked
    // by running the SAME staged script under a wrapper that tees the stub's
    // output. A cheaper alternative - grepping the producer's source - would
    // assert the text rather than the call.
    //
    // The placeholders are positional and the `!r` conversion is Python's,
    // so the two languages' syntaxes do not have to agree.
    let probe = format!(
        r#"
import subprocess, sys

import importlib.util
spec = importlib.util.spec_from_file_location("totalseg_probe", {0})
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

real_run = subprocess.run
def spy(argv, **kwargs):
    sys.stderr.write("ENGINE_ARGV=%s\n" % " ".join(argv))
    return real_run(argv, **kwargs)

subprocess.run = spy
module.main({1})
"#,
        python_literal(&script.display().to_string()),
        python_literal(&built.path.display().to_string()),
    );
    let probe_path = scratch.path().join("probe.py");
    std::fs::write(&probe_path, probe).expect("probe");

    let output = Command::new(&python)
        .arg(&probe_path)
        .current_dir(&built.path)
        .output()
        .expect("the probe runs");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "the probe failed: {text}\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The stub echoes what it parsed, so these are the flags the engine
    // actually received.
    assert!(text.contains("STUB_TASK=craniofacial_structures"), "{text}");
    assert!(text.contains("STUB_ML=True"), "--ml must be passed: {text}");
    assert!(
        text.contains("STUB_FAST=False"),
        "--fast must NOT be passed: the task refuses it: {text}"
    );
    assert!(
        text.contains("STUB_DEVICE=cpu"),
        "cpu is the default: {text}"
    );
    // And the input really was a nifti built from all four frames, in the
    // LAS x-y-z layout the engine's DICOM reader produces: 32 columns,
    // 32 rows, 4 slices on the z axis.
    assert!(text.contains("STUB_INPUT_SHAPE=[32, 32, 4]"), "{text}");
    assert!(
        text.contains("ENGINE_ARGV=") && text.contains("totalsegmentator.bin.TotalSegmentator"),
        "the engine is reached with -m: {text}"
    );
}

#[test]
fn a_non_ct_study_is_refused_with_the_reason() {
    let python = require_python!();

    let scratch = Scratch::new("not-ct");
    // An MR study: the model reads Hounsfield units, so its output would be
    // confidently wrong rather than merely poor.
    let built = ct_workdir(&scratch.path().join("state"), &["MR"]);
    let script = real_producer(&scratch);

    let run = run_producer(&python, &script, &built);
    assert!(
        matches!(*run.outcome(), Outcome::ExitFailure(1)),
        "the producer must exit rather than emit a block: {:?}",
        *run.outcome()
    );
    // And it must not have written a result: a refused run that leaves a
    // result behind is worse than one that fails, because the caller would
    // store it.
    assert!(
        !built.path.join("result.json").exists(),
        "a refused run must leave no result"
    );
}

#[test]
fn a_manifest_without_geometry_is_refused_rather_than_guessed() {
    let python = require_python!();

    let scratch = Scratch::new("no-geometry");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = real_producer(&scratch);

    // Strip the geometry out of the manifest, which is the shape every
    // pre-extension manifest has. The producer must STOP: a fabricated 1 mm
    // z would compress this 12 mm stack twelvefold and the masks would be
    // wrong on a grid that is native in shape.
    let manifest_path = built.path.join("manifest.json");
    let text = std::fs::read_to_string(&manifest_path).expect("manifest");
    let stripped = strip_geometry(&text);
    std::fs::write(&manifest_path, stripped).expect("rewrite");

    let run = run_producer(&python, &script, &built);
    assert!(
        matches!(*run.outcome(), Outcome::ExitFailure(1)),
        "the producer must refuse rather than assume 1 mm: {:?}",
        *run.outcome()
    );
    assert!(
        !built.path.join("result.json").exists(),
        "a refused run must leave no result"
    );
}

/// A path as a Python string literal.
///
/// Windows paths contain backslashes, which are escape characters inside a
/// Python literal: `D:\temp\run` would become a tab and a newline. The
/// generated probe has to name real paths, so they are quoted here where the
/// escaping rule is explicit rather than assembled by hand.
fn python_literal(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// A stub whose class map is missing an organ the site declares.
///
/// The only difference from [`STUB_ENGINE`] is the missing `teeth_upper`: the
/// point is that the producer notices, not that the engine failed.
const STUB_ENGINE_MISSING_ORGAN: &str = r#"
import json, pathlib, sys

import nibabel as nib
import numpy as np

argv = sys.argv[1:]
options = {}
at = 0
takes_value = ("-i", "-o", "-ta", "-d", "--report")
while at < len(argv):
    token = argv[at]
    if token in takes_value:
        options[token] = argv[at + 1]
        at += 2
    else:
        at += 1

source = nib.load(options["-i"])
volume = np.asanyarray(source.dataobj)
predicted = np.zeros(volume.shape, dtype=np.uint8)
predicted[:, 0:1, :] = 9
predicted[0, 1:4, 1:4] = 5

# teeth_upper is absent, which is what the test is about.
classes = {"5": "mandible", "9": "teeth_lower", "12": "skull"}
nib.save(nib.Nifti1Image(predicted, source.affine), options["-o"])
pathlib.Path(options["--report"]).write_text(json.dumps({
    "task": options.get("-ta"),
    "classes": classes,
    "totalsegmentator_version": "2.18.0-stub",
}))
"#;

#[test]
fn a_task_missing_a_declared_organ_is_refused_rather_than_trimmed() {
    let python = require_python!();

    let scratch = Scratch::new("missing-organ");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    // Stage a different stub body: same harness, one organ short.
    let package = scratch.path().join("totalsegmentator");
    std::fs::create_dir_all(package.join("bin")).expect("package");
    std::fs::write(package.join("__init__.py"), b"").expect("init");
    std::fs::write(package.join("bin").join("__init__.py"), b"").expect("init");
    std::fs::write(
        package.join("bin").join("TotalSegmentator.py"),
        STUB_ENGINE_MISSING_ORGAN,
    )
    .expect("stub");
    stage_reader(&scratch);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("abi")
        .join("RTSTRUCT-CT-totalseg.py");
    let body = std::fs::read_to_string(&source).expect("the producer script");
    let script = scratch.path().join("RTSTRUCT-CT-totalseg.py");
    std::fs::write(&script, body).expect("stage producer");

    let run = run_producer(&python, &script, &built);
    assert!(
        matches!(*run.outcome(), Outcome::ExitFailure(1)),
        "a declared organ the task does not carry must stop the run, not be \
         trimmed from the result: {:?}",
        *run.outcome()
    );
    assert!(
        !built.path.join("result.json").exists(),
        "a refused run must leave no result: a segmentation with one segment \
         fewer than the site declared looks complete and is not"
    );
}

/// Remove the four geometry keys from every frame of a manifest document.
///
/// A textual edit rather than a JSON round trip, because the point is to
/// produce the OLD DOCUMENT SHAPE - the one an earlier writer emitted - and a
/// re-serialised document is not that shape.
///
/// `to_json` writes one frame per line and appends the geometry as a
/// CONTIGUOUS RUN at the end of the object, each key introduced by its own
/// comma: `..., "photometric": "X", "pixel_spacing": [..], "slice_spacing":
/// N, "position": [..], "orientation": [..]}`. So the run is cut from the
/// first `, "pixel_spacing"` to the frame's own closing brace, which is
/// whatever follows the last `]` of the run.
///
/// A plain line filter cannot do this: it would leave a dangling comma where
/// the run began, and the stripped document would then fail to parse for a
/// reason that has nothing to do with the code under test - which is exactly
/// the false failure the guard test below is here to catch.
fn strip_geometry(text: &str) -> String {
    const FIRST_GEOMETRY_KEY: &str = ", \"pixel_spacing\"";
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let Some(at) = line.find(FIRST_GEOMETRY_KEY) else {
            out.push_str(line);
            continue;
        };
        // The frame's closing brace is everything after the run's last `]`.
        // When the line carries no `]` the run is malformed and the guard
        // test below is what says so, rather than a silent truncation here.
        match line.rfind(']') {
            Some(last) => {
                out.push_str(&line[..at]);
                out.push_str(&line[last + 1..]);
            }
            None => out.push_str(line),
        }
    }
    out.push('\n');
    out
}

#[test]
fn a_geometry_free_manifest_genuinely_loses_the_geometry() {
    // The guard on the guard: if `strip_geometry` stopped removing the keys,
    // the refusal test above would pass for the wrong reason.
    let scratch = Scratch::new("strip-check");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let text = std::fs::read_to_string(built.path.join("manifest.json")).expect("manifest");
    assert!(
        text.contains("pixel_spacing"),
        "the fixture has geometry to lose"
    );
    let stripped = strip_geometry(&text);
    for key in ["pixel_spacing", "slice_spacing", "position", "orientation"] {
        assert!(!stripped.contains(key), "{key} survived the strip");
    }
    // And the result is still a document the crate can parse.
    let parsed = clinfer::json::parse(stripped.as_bytes());
    assert!(
        parsed.is_ok(),
        "the stripped manifest must still parse: {parsed:?}"
    );
}

/// Rewrite a workdir manifest, replacing one frame's keys.
///
/// A textual edit for the same reason [`strip_geometry`] is one: these tests
/// are about the DOCUMENT SHAPE a producer receives, and a re-serialised
/// document is not the shape clinfer writes.
fn patch_manifest(built: &workdir::Workdir, from: &str, to: &str) {
    let path = built.path.join("manifest.json");
    let text = std::fs::read_to_string(&path).expect("manifest");
    assert!(
        text.contains(from),
        "the fixture must contain the text being replaced: {from}"
    );
    std::fs::write(&path, text.replace(from, to)).expect("rewrite");
}

/// The affine z the stub was handed, as a float.
fn affine_z(run: &RunAndStderr) -> f64 {
    run.echoed("STUB_AFFINE_Z=").parse().expect("an affine z")
}

#[test]
fn positions_win_over_a_declared_slice_spacing_that_disagrees() {
    let python = require_python!();

    let scratch = Scratch::new("z-drift");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = real_producer(&scratch);

    // The live finding, reproduced: the instances declare the ORIGINAL
    // acquisition step (0.16 mm) while the stack actually sent keeps every
    // twelfth slice, so the positions are 1.92 mm apart. A producer that
    // trusts the declared value compresses the volume twelvefold in z and
    // the model returns masks on a grid that is wrong in a way no later
    // check catches.
    //
    // The anchors are the text `to_json` emits, not a re-serialised
    // document. Rust's `Display` for `f64` drops the fraction of a whole
    // number, so the fixture's `[-12.4, -102.0, 0.0]` is written
    // `[-12.4, -102, 0]` - `-102`, not `-102.0`. The anchors below spell
    // the z values through a float, so they follow the writer rather than
    // guessing at it.
    patch_manifest(&built, "\"slice_spacing\": 12", "\"slice_spacing\": 0.16");
    // Re-space the positions at 1.92 mm, which is the stack that was sent.
    for at in 0..FRAMES {
        let from = format!("\"position\": [-12.4, -102, {}]", at as f64 * 12.0);
        let to = format!("\"position\": [-12.4, -102, {}]", at as f64 * 1.92);
        patch_manifest(&built, &from, &to);
    }

    let run = run_producer(&python, &script, &built);
    assert_eq!(
        *run.outcome(),
        Outcome::Success,
        "the producer must succeed: {:?}",
        run.diagnosis()
    );
    // The affine is a float32 after the nifti round-trip, so the comparison
    // is to the float32 of 1.92 rather than to the double: an exact
    // comparison would fail on a conversion that is not a defect.
    let wanted = f64::from(1.92f32);
    assert!(
        (affine_z(&run) - wanted).abs() < 1e-6,
        "the affine must carry the POSITIONS' step (1.92), not the declared \
         0.16: got {}",
        affine_z(&run)
    );
}

#[test]
fn the_declared_slice_spacing_is_used_when_no_positions_exist() {
    let python = require_python!();

    let scratch = Scratch::new("z-declared");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = real_producer(&scratch);

    // No positions at all: the declared spacing is the only honest answer
    // left, and it must be used rather than refused.
    let manifest = std::fs::read_to_string(built.path.join("manifest.json")).expect("manifest");
    let stripped: String = manifest
        .lines()
        .map(|line| match line.find(", \"position\": [") {
            Some(at) => match line.rfind(']') {
                Some(last) => format!("{}{}", &line[..at], &line[last + 1..]),
                None => line.to_string(),
            },
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(
        !stripped.contains("\"position\""),
        "the fixture must lose its positions for this test to mean anything"
    );
    std::fs::write(built.path.join("manifest.json"), stripped).expect("rewrite");

    let run = run_producer(&python, &script, &built);
    assert_eq!(
        *run.outcome(),
        Outcome::Success,
        "a declared spacing with no positions is usable: {:?}",
        run.diagnosis()
    );
    assert!(
        (affine_z(&run) - 12.0).abs() < 1e-9,
        "without positions the declared 12.0 must be used: got {}",
        affine_z(&run)
    );
}

#[test]
fn a_signed_source_is_read_as_signed_and_an_unsigned_one_is_not() {
    let python = require_python!();

    // `read_frame` is the unit under test, and it has to be exercised
    // DIRECTLY rather than through the nifti: `volume_and_affine` casts every
    // plane to int16 for the model, so the two readings collapse to the same
    // array by the time the engine sees them. The dtype is what differs, and
    // the dtype is what the engine stub is asked to report.
    //
    // 0xFFF6 is -10 as int16 and 65526 as uint16, so it distinguishes the two
    // unambiguously.
    let probe = r#"
import importlib.util, json, sys, pathlib

spec = importlib.util.spec_from_file_location("totalseg_probe", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

workdir = pathlib.Path(sys.argv[2])
frame = {"file": "0001.raw", "bits": 16, "height": 32, "width": 32}

unsigned = module.read_frame(workdir, dict(frame))
signed = module.read_frame(workdir, dict(frame, signed=True))

print("PROBE_UNSIGNED_DTYPE=%s" % unsigned.dtype)
print("PROBE_UNSIGNED_MAX=%d" % int(unsigned.max()))
print("PROBE_SIGNED_DTYPE=%s" % signed.dtype)
print("PROBE_SIGNED_MIN=%d" % int(signed.min()))

# The affine the real volume ends up with, for the same manifest.
manifest = json.loads((workdir / "manifest.json").read_text())
_, affine, _, _ = module.volume_and_affine(workdir, manifest)
print("PROBE_AFFINE_Z=%r" % float(affine[2, 2]))
"#;

    let scratch = Scratch::new("signed-probe");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = real_producer(&scratch);

    // One frame of two's complement, then zeros: a small exact pattern whose
    // two readings differ. Files are named by 1-based position, so the first
    // frame is `0001.raw`.
    let mut pattern = Vec::with_capacity((SIDE * SIDE * 2) as usize);
    for index in 0..(SIDE * SIDE) {
        let value: u16 = if index == 0 { 0xFFF6 } else { 0x0000 };
        pattern.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(built.path.join("frames").join("0001.raw"), &pattern).expect("frame");

    let probe_path = scratch.path().join("probe_signed.py");
    std::fs::write(&probe_path, probe).expect("probe");

    let output = Command::new(&python)
        .arg(&probe_path)
        .arg(&script)
        .arg(&built.path)
        .current_dir(&built.path)
        .output()
        .expect("the probe runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "the probe failed: {text}");

    assert!(
        text.contains("PROBE_UNSIGNED_DTYPE=uint16"),
        "an absent `signed` key must read unsigned, which is what every \
         producer assumed before the key existed: {text}"
    );
    assert!(
        text.contains("PROBE_UNSIGNED_MAX=65526"),
        "the unsigned reading of 0xFFF6 is 65526, an air value, and that \
         reading must reach the engine instead of being folded to a \
         negative: {text}"
    );
    assert!(
        text.contains("PROBE_SIGNED_DTYPE=int16"),
        "a signed source must be read signed: {text}"
    );
    assert!(
        text.contains("PROBE_SIGNED_MIN=-10"),
        "the signed reading of 0xFFF6 is -10, which is the air of a CBCT \
         stack rather than 64536: {text}"
    );
    // And the geometry sits beside it in the same run, so the manifest the
    // probe read is the one the producer also builds an affine from.
    assert!(
        text.contains("PROBE_AFFINE_Z=12.0"),
        "the fixture's declared spacing and positions agree at 12.0: {text}"
    );
}

#[test]
fn a_successful_run_reports_no_diagnosis_and_the_engine_version() {
    let python = require_python!();

    let scratch = Scratch::new("no-stderr-noise");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = real_producer(&scratch);

    let mut run = run_producer(&python, &script, &built);
    assert_eq!(*run.outcome(), Outcome::Success, "{:?}", run.diagnosis());
    // A successful run has nothing to diagnose, so `diagnosis` is `None`.
    // This is the same call the worker logs, so a regression that started
    // reporting a fault on success would surface here first.
    assert!(
        run.diagnosis().is_none(),
        "a successful run has no diagnosis: {:?}",
        run.diagnosis()
    );
    assert!(
        run.report.cancel_reason().is_none(),
        "and nothing to cancel"
    );
    // The version is the visible proof that the run report was READ rather
    // than defaulted: a producer that never opened it could not know this.
    let result = run.take_result();
    assert_eq!(
        result.producer.model_version.as_deref(),
        Some("2.18.0-stub")
    );
}

#[test]
fn the_volume_handed_to_the_engine_is_las_with_slices_on_the_z_axis() {
    let python = require_python!();

    let scratch = Scratch::new("las-orientation");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = staged_producer_with_engine(&scratch, STUB_ENGINE_LAS_CHECK);

    let run = run_producer(&python, &script, &built);
    assert_eq!(
        *run.outcome(),
        Outcome::Success,
        "the LAS-checking engine must accept the producer's volume: {:?}",
        run.diagnosis()
    );
    assert_eq!(run.echoed("STUB_AXCODES="), "('L', 'A', 'S')");
    assert_eq!(run.echoed("STUB_SHAPE="), "(32, 32, 4)");
}

/// An engine whose mask has ONE rectangular blob that is TALLER THAN WIDE in
/// rows, which is what makes the OpenCV (column, row) order observable.
///
/// Every other stub here paints a SQUARE patch (the 12x12 block, the 32-wide
/// column band), and a square is transpose-invariant: swapping column and row
/// maps the patch onto itself, so no coordinate assertion against a square can
/// fail. The 24.09 live finding was exactly that blind spot - both contour
/// producers emitted the OpenCV pair transposed and every test stayed green.
///
/// The blob is rows 4..16 by columns 20..31 (12 rows x 11 columns), which is
/// both asymmetric and well above MIN_AREA_PX.
const STUB_ENGINE_ASYMMETRIC: &str = r#"
import json
import pathlib
import sys

import nibabel as nib
import numpy as np

argv = sys.argv[1:]
opts = {}
takes = ("-i", "-o", "-ta", "-d", "--report")
at = 0
while at < len(argv):
    token = argv[at]
    if token in takes:
        opts[token] = argv[at + 1]
        at += 2
    else:
        at += 1

img = nib.load(opts["-i"])
shape = np.asanyarray(img.dataobj).shape
classes = {"5": "mandible", "9": "teeth_lower", "12": "skull", "7": "teeth_upper"}
predicted = np.zeros(shape, dtype=np.uint8)
# The nifti is indexed (x, y, z) and the producer transposes it back to
# (slices, rows, cols), so this block of 12 x-values by 11 y-values lands as
# rows 1..11 by columns 4..15 in the array contours_of reads. It is
# ASYMMETRIC either way, which is the point: a square patch is
# transpose-invariant and would pass whatever the producer did with the pair.
predicted[4:16, 20:31, :] = 5
nib.save(nib.Nifti1Image(predicted, img.affine), opts["-o"])
pathlib.Path(opts["--report"]).write_text(json.dumps({
    "task": opts.get("-ta"),
    "classes": classes,
    "totalsegmentator_version": "2.18.0-asym-stub",
    "device": opts.get("-d", "cpu"),
    "output_files": {"segmentation": opts["-o"]},
}))
"#;

#[test]
fn the_emitted_points_are_column_then_row_not_transposed() {
    // The red fact for the 24.09 live finding: BOTH contour producers emitted
    // the OpenCV pair as [row, column] while the ABI contract is
    // [column, row], so every contour landed transposed on the anatomy.
    //
    // OpenCV hands over (x=column, y=row) natively, so the fix is to emit the
    // pair AS-IS. The stub paints an asymmetric block, which becomes rows
    // 1..11 by columns 4..15 in the array the producer contours. Every
    // emitted point must therefore have its FIRST coordinate (the ABI's
    // column) in 4..=15 and its SECOND (the row) in 1..=11; a transposed emit
    // puts a row in the first slot and fails on the first point.
    let python = require_python!();

    let scratch = Scratch::new("column-row-order");
    let built = ct_workdir(&scratch.path().join("state"), &["CT"]);
    let script = staged_producer_with_engine(&scratch, STUB_ENGINE_ASYMMETRIC);

    let mut run = run_producer(&python, &script, &built);
    assert_eq!(
        *run.outcome(),
        Outcome::Success,
        "the producer must succeed: {:?}",
        run.diagnosis()
    );
    let result = run.take_result();
    let rs = result.rtstruct.expect("an rtstruct block");
    let mandible = rs
        .rois
        .iter()
        .find(|roi| roi.name == "MANDIBLE")
        .expect("the mandible blob is the only non-empty organ");

    // The blob is rows 1..11 by columns 4..15 in the producer's own grid.
    let mut checked = 0;
    for contour in &mandible.contours {
        for point in &contour.points {
            assert!(
                (4.0..=15.0).contains(&point[0]),
                "point {point:?} has its first coordinate {} outside the \
                 column range 4..=15: the pair is (column, row), so a value \
                 in 1..=11 here means the OpenCV pair was emitted transposed",
                point[0]
            );
            assert!(
                (1.0..=11.0).contains(&point[1]),
                "point {point:?} has its second coordinate {} outside the row \
                 range 1..=11: the pair is (column, row)",
                point[1]
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "the mandible blob must produce at least one contour"
    );
}
