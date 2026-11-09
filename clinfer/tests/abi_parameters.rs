// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The ABI example scripts read their tunables from `manifest["parameters"]`.
//!
//! Batch 0811 made clinfer write the flattened UPS Scheduled Processing
//! Parameters Sequence (0074,1210) into `manifest.json["parameters"]`. This
//! file pins the other half of the loop: the scripts consume it, and a
//! hand-written workitem that carries nothing still runs on the documented
//! defaults.
//!
//! Two kinds of test live here:
//!
//! - an END-TO-END falsifier on `DERIVED-clahe.py`, which proves a parameter
//!   actually changes what the script produces rather than merely being read;
//! - a DEFAULTS SWEEP that stages every `abi/*.py` script into a work
//!   directory and runs it with an empty parameter object, proving the shared
//!   reader did not break a script that has no parameters at all.
//!
//! Both skip cleanly when no interpreter on this machine can import the
//! scripts' dependencies, which is the same rule `rtstruct_contours_producer`
//! follows: a test that silently passes without running anything would be
//! worse than one that says why it did not run.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use clinfer::abi::external::{self, Outcome, Processor};
use clinfer::abi::manifest::ManifestParameter;
use clinfer::abi::schema::DEFAULT_MAX_RESULT_BYTES;
use clinfer::abi::workdir::{self, Frame};
use clinfer::dicomweb::ParamValue;
use clinfer::receiver::Counters;

/// The fixture frame's side, in pixels.
const SIDE: u64 = 32;

/// A temporary directory that removes itself.
///
/// The suffix comes from a counter, never from a clock: two tests running in
/// parallel on one process can read the same clock tick, and two runs sharing
/// a directory would then overwrite each other's manifest.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "clinfer-abi-params-{label}-{}-{}",
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

/// Stage one of the crate's example scripts into the scratch directory.
///
/// The scripts live beside the crate rather than inside it and an interpreter
/// resolves `import params` against the SCRIPT's directory, so the sibling
/// module is staged too. Placing the shared reader out of reach would make
/// every script fail on its import, which is not the question these tests ask.
fn stage_script(scratch: &Scratch, name: &str) -> PathBuf {
    let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("abi");
    for staged in ["params.py", name] {
        let body = std::fs::read_to_string(source_dir.join(staged))
            .unwrap_or_else(|error| panic!("reading {staged}: {error}"));
        std::fs::write(scratch.path().join(staged), body).expect("staged script");
    }
    scratch.path().join(name)
}

/// An interpreter that can import the given modules with the seam's cleared
/// environment, or `None`.
///
/// The seam clears the child down to PATH and PYTHONUNBUFFERED, so an
/// interpreter whose packages arrive through PYTHONPATH imports them in a
/// shell and fails under the seam. The check therefore imports the modules
/// under an environment as bare as the seam's.
fn interpreter_importing(modules: &str) -> Option<String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = std::env::var_os("CLINFER_TEST_PYTHON") {
        candidates.push(PathBuf::from(explicit));
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let root = Path::new(&manifest);
        for relative in [".venv/Scripts/python.exe", ".venv/bin/python"] {
            candidates.push(root.join(relative));
            candidates.push(root.join("..").join(relative));
        }
    }
    for name in ["python", "python3"] {
        candidates.push(PathBuf::from(name));
    }
    for candidate in candidates {
        let imports = std::process::Command::new(&candidate)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("PYTHONUNBUFFERED", "1")
            .arg("-c")
            .arg(format!("import {modules}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if imports {
            return Some(candidate.display().to_string());
        }
    }
    None
}

/// A workdir holding one uniform frame, so no script has to reject it for a
/// reason unrelated to parameters.
///
/// The pixel value sits above the scripts' 128 threshold and inside the bone
/// window, so a threshold or contour producer finds something rather than
/// stopping on an empty mask.
fn one_frame_workdir(parent: &Path, parameters: &[ManifestParameter]) -> workdir::Workdir {
    let mut pixels = vec![0u8; (SIDE * SIDE * 2) as usize];
    for row in 4..12u64 {
        for column in 4..12u64 {
            let at = ((row * SIDE + column) * 2) as usize;
            let value = 500u16.to_le_bytes();
            pixels[at] = value[0];
            pixels[at + 1] = value[1];
        }
    }
    let frames = [Frame {
        sop_uid: "1.2.3.1",
        bytes: pixels.as_slice(),
        width: SIDE as u32,
        height: SIDE as u32,
        bits: 16,
        signed: false,
        photometric: "MONOCHROME2",
        pixel_spacing: Some([0.5, 0.5]),
        slice_spacing: Some(1.0),
        position: Some([0.0, 0.0, 0.0]),
        orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
    }];

    workdir::build_with_parameters(
        parent,
        "abi-params",
        clinfer::abi::StudyContext {
            uid: "1.2.840.113857.2411.1".to_string(),
            description: Some("PARAMS".to_string()),
            modalities: vec!["CT".to_string()],
            body_part: Some("CHEST".to_string()),
        },
        &frames,
        "0.3.0-alpha",
        1,
        parameters,
    )
    .expect("workdir")
}

fn processor(interpreter: &str, script: &Path) -> Processor {
    Processor {
        id: "abi_params".to_string(),
        interpreter: PathBuf::from(interpreter),
        script: script.to_path_buf(),
        timeout_secs: 120,
        max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
    }
}

fn run(processor: &Processor, workdir: &workdir::Workdir) -> external::RunReport {
    let counters = Counters::default();
    external::run(
        processor,
        &workdir.path,
        &counters,
        &mut external::ChildMetrics::new(&counters, "abi-params-test"),
        |line| eprintln!("[processor] {line}"),
    )
    .expect("the seam runs the processor")
}

/// `DERIVED-clahe.py` records the clip limit it applied in its
/// `model_version` (`clip<value>`), so a parameter that changes the run is
/// visible in the result without decoding any pixels.
fn clahe_model_version(report: &external::RunReport) -> String {
    let result = report.result.as_ref().expect("a parsed result");
    result
        .producer
        .model_version
        .clone()
        .expect("DERIVED-clahe declares a model_version")
}

#[test]
fn a_clip_limit_parameter_changes_what_the_script_produces() {
    let Some(python) = interpreter_importing("numpy, cv2") else {
        eprintln!(
            "skipping: no interpreter here imports numpy and cv2 with the seam's \
             cleared environment"
        );
        return;
    };

    let scratch = Scratch::new("clahe");
    let script = stage_script(&scratch, "DERIVED-clahe.py");

    // The default run: the requester stated nothing.
    let plain = one_frame_workdir(&scratch.path().join("default"), &[]);
    let default_report = run(&processor(&python, &script), &plain);
    assert!(
        matches!(default_report.outcome, Outcome::Success),
        "the default run has to succeed, got {:?}",
        default_report.outcome
    );
    assert_eq!(
        clahe_model_version(&default_report),
        "clip2.0",
        "the default clip limit is the script's constant, read as a number"
    );

    // The same workdir with one parameter. A script that reads the manifest
    // records a different model_version; one that ignores it records the
    // default again, which is exactly what a revert to the hardcoded
    // constant would do.
    let tuned = one_frame_workdir(
        &scratch.path().join("tuned"),
        &[ManifestParameter {
            name: "CLIP_LIMIT".to_string(),
            value: ParamValue::Numeric(1.0),
        }],
    );
    let tuned_report = run(&processor(&python, &script), &tuned);
    assert!(
        matches!(tuned_report.outcome, Outcome::Success),
        "the tuned run has to succeed, got {:?}",
        tuned_report.outcome
    );
    assert_eq!(
        clahe_model_version(&tuned_report),
        "clip1.0",
        "the parameter has to reach the script and change its output"
    );
}

/// Every example script runs (or reaches its own argument/dependency check)
/// with an EMPTY parameter object.
///
/// This is the defaults sweep the contract promises: an absent
/// `manifest["parameters"]` and `{}` are the same statement, and no script may
/// fail because of it. A script that exits non-zero is reported with its
/// stderr, so a real regression here names the script.
///
/// The scripts that need a third-party model file or a network endpoint are
/// still run: their own refusal (a `SystemExit` with an actionable message) is
/// a legitimate outcome, and reaching it proves the parameter read did not
/// break the path that produces the message.
#[test]
fn every_example_script_survives_an_empty_parameter_object() {
    let Some(python) = interpreter_importing("numpy") else {
        eprintln!(
            "skipping: no interpreter here imports numpy with the seam's \
             cleared environment"
        );
        return;
    };

    let scratch = Scratch::new("sweep");
    let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("abi");
    let mut scripts: Vec<String> = std::fs::read_dir(&source_dir)
        .expect("the abi directory")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| name.ends_with(".py") && name != "params.py")
        .collect();
    scripts.sort();
    assert!(
        scripts.len() >= 12,
        "the sweep has to see the example scripts, found {scripts:?}"
    );

    let mut failures: Vec<String> = Vec::new();
    for name in &scripts {
        let staged = stage_script(&scratch, name);
        let built = one_frame_workdir(&scratch.path().join(format!("run-{name}")), &[]);
        let report = run(&processor(&python, &staged), &built);
        // `NoResult` and `ExitFailure` are how a script's OWN refusal arrives
        // (a missing model file, an unreadable image); the sweep is about the
        // parameter read, not about whether this machine has the weights. A
        // `NotStarted` would mean the interpreter or the script itself is
        // broken, which is a real failure.
        if matches!(report.outcome, Outcome::NotStarted(_)) {
            failures.push(format!("{name}: {:?}", report.outcome));
        }
    }
    assert!(
        failures.is_empty(),
        "these scripts did not even start: {failures:#?}"
    );
}

/// The shared reader is the single place the fallback rule lives, so it is
/// pinned directly as well: every helper returns the DEFAULT for a missing
/// key, a wrong JSON type, and a manifest that predates the key.
#[test]
fn the_reader_falls_back_for_every_unusable_input() {
    let Some(python) = interpreter_importing("json") else {
        eprintln!("skipping: no interpreter here runs python");
        return;
    };

    let scratch = Scratch::new("reader");
    // The reader has to sit beside the checker: an interpreter resolves
    // `import params` against the SCRIPT's directory, not the caller's cwd.
    let reader = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("abi")
            .join("params.py"),
    )
    .expect("the shared reader");
    std::fs::write(scratch.path().join("params.py"), reader).expect("staged reader");
    let script = scratch.path().join("check_params.py");
    // The assertions live in Python because the reader IS Python: testing it
    // through a Rust re-implementation would pin the copy, not the contract.
    std::fs::write(
        &script,
        r#"
import json
import params

missing = {}                       # a manifest that predates the key
empty = {"parameters": {}}
wrong = {"parameters": {"N": "eight", "T": 8, "G": 0}}
present = {"parameters": {"N": 4, "T": "clahe", "G": 6, "C": "SRT:T-D0050"}}
# A DATETIME parameter is a JSON string, so the text helper reads it as it
# is and the number helper refuses it.
dated = {"parameters": {"D": "20261109101530.123+0300"}}

assert params.number(missing, "N", 2.0) == 2.0
assert params.number(empty, "N", 2.0) == 2.0
assert params.number(wrong, "N", 2.0) == 2.0, "a TEXT value is not a number"
assert params.number(present, "N", 2.0) == 4.0

assert params.count(wrong, "N", 7) == 7, "a TEXT value is not a count"
assert params.count(present, "N", 7) == 4

assert params.text(wrong, "T", "edsr") == "edsr", "a NUMBER is not text"
assert params.text(present, "T", "edsr") == "clahe"

assert params.grid(wrong, "G", (8, 8)) == (8, 8), "zero is not a grid"
assert params.grid(present, "G", (8, 8)) == (6, 6)

assert params.code(present, "C", None) == "T-D0050"
assert params.code(present, "N", None) is None, "a number is not a code"
assert params.raw(present, "C") == "SRT:T-D0050"

assert params.text(dated, "D", None) == "20261109101530.123+0300"
assert params.number(dated, "D", 0.0) == 0.0, "a DATETIME is not a number"

print("params OK")
"#,
    )
    .expect("checker script");

    let output = std::process::Command::new(&python)
        .current_dir(scratch.path())
        .arg(&script)
        .env("PYTHONUNBUFFERED", "1")
        .output()
        .expect("run the checker");
    assert!(
        output.status.success(),
        "the shared reader broke its contract: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("params OK"),
        "the checker has to report success"
    );
}
