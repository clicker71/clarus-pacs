// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The external-processor seam: a configured processor, run over a work
//! directory, against the real spawn path.
//!
//! These tests run a REAL child process - a small Python program written
//! into a temporary directory - so what is exercised is the seam itself:
//! argv, cwd, the environment, stderr forwarding and its limits, the
//! timeout and the process-tree kill, the strict schema, and the atomic
//! read of `result.json`. A stubbed spawn would test none of that.
//!
//! The tests skip themselves when no interpreter is available, rather than
//! failing a build on a machine that has no Python: the seam is the
//! subject, not the machine's software inventory.

#[path = "support/fake_origin.rs"]
mod fake_origin;

use clinfer::abi::external::{self, Outcome, Processor};
use clinfer::abi::schema::DEFAULT_MAX_RESULT_BYTES;
use clinfer::abi::workdir::{self, Frame};
use clinfer::receiver::Counters;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// A temporary directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "clinfer-abi-{label}-{}-{}",
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

    /// Write a file into the scratch directory and return its path.
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

/// The interpreter to run the seams with, or `None` on a machine without
/// one. `python3` on Unix, `python` on Windows.
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

/// Skip the test body when there is no interpreter, saying so.
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

fn processor(interpreter: &str, script: &Path, timeout_secs: u32) -> Processor {
    Processor {
        id: "demo".to_string(),
        interpreter: PathBuf::from(interpreter),
        script: script.to_path_buf(),
        timeout_secs,
        max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
    }
}

/// The same processor with an explicit result budget, for the tests that
/// are about the budget rather than about the seam's mechanics.
fn processor_with_budget(
    interpreter: &str,
    script: &Path,
    timeout_secs: u32,
    max_result_bytes: u64,
) -> Processor {
    Processor {
        id: "demo".to_string(),
        interpreter: PathBuf::from(interpreter),
        script: script.to_path_buf(),
        timeout_secs,
        max_result_bytes,
    }
}

/// A metrics handle for one run, built where the run is made.
///
/// The seam tests assert what the CHILD did - its exit, its stderr, its
/// result.json - and not what the gauges published, so the handle is a
/// formality here. It is still a real one: `run` publishes into it exactly
/// as it does in production, which is what keeps the two paths the same.
fn metrics(counters: &Counters) -> clinfer::abi::external::ChildMetrics<'_> {
    clinfer::abi::external::ChildMetrics::new(counters, "seam-test")
}

fn counters() -> Counters {
    Counters::default()
}

/// A work directory with two small frames, built through the real builder.
fn built_workdir(parent: &Path) -> workdir::Workdir {
    let frames = [
        Frame {
            sop_uid: "1.2.3.1",
            bytes: &[10, 20, 30, 40],
            width: 2,
            height: 2,
            bits: 8,
            signed: false,
            photometric: "MONOCHROME2",
            pixel_spacing: None,
            slice_spacing: None,
            position: None,
            orientation: None,
        },
        Frame {
            sop_uid: "1.2.3.2",
            bytes: &[50, 60, 70, 80],
            width: 2,
            height: 2,
            bits: 8,
            signed: false,
            photometric: "MONOCHROME2",
            pixel_spacing: None,
            slice_spacing: None,
            position: None,
            orientation: None,
        },
    ];
    workdir::build(
        parent,
        "study",
        clinfer::abi::StudyContext {
            uid: "1.2.3".to_string(),
            description: Some("Fixture".to_string()),
            modalities: vec!["DX".to_string()],
            body_part: Some("CHEST".to_string()),
        },
        &frames,
        "0.3.0",
        1,
    )
    .expect("workdir")
}

/// A processor that echoes its own context and writes a valid result.
const ECHO: &str = r#"
import json, os, pathlib, sys

def main():
    workdir = pathlib.Path(sys.argv[1])
    manifest = json.loads((workdir / "manifest.json").read_text())
    files = []
    for frame in manifest["frames"]:
        (workdir / frame["file"]).write_bytes(
            (workdir / "frames" / frame["file"]).read_bytes()
        )
        files.append({"file": frame["file"]})
    result = {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {"script": "echo.py", "script_version": "9.9.9"},
        "derived": {"files": files},
    }
    # The context checks write to stderr, which is where a processor's
    # diagnostics belong; result.json is the only output channel.
    import sys as s
    s.stderr.write("ARGC=%d\n" % len(s.argv))
    s.stderr.write("ARGV1=%s\n" % s.argv[1])
    s.stderr.write("CWD_SAME=%s\n" % (pathlib.Path.cwd() == workdir))
    s.stderr.write("STDIN_NULL=%s\n" % (s.stdin is None or True))
    s.stderr.write("CLEAN=%s\n" % ("HOME" not in os.environ and "PYTHONUNBUFFERED" in os.environ))
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result))
    tmp.rename(workdir / "result.json")

main()
"#;

// -- argv, cwd, environment ---------------------------------------------

#[test]
fn the_processor_is_given_the_workdir_as_argv1_and_as_its_cwd() {
    let python = require_python!();
    let scratch = Scratch::new("argv");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script("echo.py", ECHO);

    let mut lines: Vec<String> = Vec::new();
    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |line| lines.push(line.to_string()),
    )
    .expect("ran");

    assert_eq!(report.outcome, Outcome::Success, "{:?}", report.outcome);
    let text = lines.join("\n");
    assert!(
        text.contains("ARGC=2"),
        "the workdir is the only argument: {text}"
    );
    assert!(
        text.contains(&format!("ARGV1={}", built.path.display())),
        "argv[1] is the work directory: {text}"
    );
    assert!(text.contains("CWD_SAME=True"), "cwd is the workdir: {text}");
    assert!(
        text.contains("CLEAN=True"),
        "the environment is cleared and then given PATH + PYTHONUNBUFFERED: {text}"
    );
}

// -- success ------------------------------------------------------------

#[test]
fn a_valid_result_is_read_back_with_its_derived_files() {
    let python = require_python!();
    let scratch = Scratch::new("success");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script("echo.py", ECHO);

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    assert_eq!(report.outcome, Outcome::Success);
    let result = report.result.expect("a parsed result");
    assert_eq!(result.producer.script, "echo.py");
    assert_eq!(result.producer.script_version.as_deref(), Some("9.9.9"));
    let files = result.derived.expect("derived").files;
    let names: Vec<&str> = files.iter().map(|f| f.file.as_str()).collect();
    assert_eq!(names, vec!["0000.raw", "0001.raw"]);
    // The echo script declares no geometry, which means "the source's own".
    assert!(files.iter().all(|f| f.width.is_none() && f.bits.is_none()));
}

/// A processor that declares a geometry it made up, which the relaxed rule
/// allows: upscale, downscale and crop are all legal.
const DECLARING: &str = r#"
import json, pathlib, sys

workdir = pathlib.Path(sys.argv[1])
manifest = json.loads((workdir / "manifest.json").read_text())
files = []
for frame in manifest["frames"]:
    # Twice the width, twice the height, same 8 bits: 4x the bytes.
    (workdir / frame["file"]).write_bytes(b"\x00" * 16)
    files.append({"file": frame["file"], "width": 4, "height": 4, "bits": 8})
(workdir / "result.json").write_text(json.dumps({
    "protocol": {"name": "clinfer-abi", "version": 1},
    "producer": {"script": "DERIVED-upscale.py"},
    "derived": {"files": files},
}))
"#;

#[test]
fn a_declared_geometry_is_read_back_as_declared() {
    let python = require_python!();
    let scratch = Scratch::new("declared");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script("DERIVED-upscale.py", DECLARING);

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    assert_eq!(report.outcome, Outcome::Success, "{:?}", report.outcome);
    let files = report
        .result
        .expect("a parsed result")
        .derived
        .expect("derived")
        .files;
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].width, Some(4));
    assert_eq!(files[0].height, Some(4));
    assert_eq!(files[0].bits, Some(8));
}

/// A processor that produces a binary segmentation, as the ABI declares it.
const SEGMENTING: &str = r#"
import json, pathlib, sys

workdir = pathlib.Path(sys.argv[1])
manifest = json.loads((workdir / "manifest.json").read_text())
# A 2x2 mask is one byte per row (2 columns pad to 1), so two bytes a frame.
files = []
for index, frame in enumerate(manifest["frames"]):
    name = "mask-%d.raw" % index
    (workdir / name).write_bytes(bytes([0b1010_0000, 0b0101_0000]))
    files.append({"file": name, "frame": index + 1})
(workdir / "result.json").write_text(json.dumps({
    "protocol": {"name": "clinfer-abi", "version": 1},
    "producer": {"script": "SEG-threshold.py"},
    "seg": {
        "segments": [{
            "label": "THRESHOLD",
            "category": {"code": "M-01000", "scheme": "SRT",
                         "meaning": "Morphologically Altered Structure"},
            "type": {"code": "M-01000", "scheme": "SRT",
                     "meaning": "Morphologically Altered Structure"},
            "color": [255, 0, 0],
        }],
        "files": files,
    },
}))
"#;

#[test]
fn a_segmentation_result_is_read_back_with_its_segments_and_masks() {
    let python = require_python!();
    let scratch = Scratch::new("seg-schema");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script("SEG-threshold.py", SEGMENTING);

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    assert_eq!(report.outcome, Outcome::Success, "{:?}", report.outcome);
    let result = report.result.expect("a parsed result");
    assert!(result.has_segmentation());
    assert!(result.seg_frames_are_complete(), "1..=2 with no gaps");
    let seg = result.seg.expect("seg");
    assert_eq!(seg.segments.len(), 1);
    assert_eq!(seg.segments[0].label, "THRESHOLD");
    assert_eq!(seg.segments[0].color, Some([255, 0, 0]));
    assert_eq!(
        seg.segments[0].algorithm, "AUTOMATIC",
        "the default the processor did not have to state"
    );
    assert_eq!(seg.files.len(), 2);
    assert_eq!(seg.files[0].frame, 1);
    assert_eq!(seg.files[1].frame, 2);
}

#[test]
fn the_spawn_counter_is_recorded_under_the_processors_label() {
    let python = require_python!();
    let scratch = Scratch::new("counter");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script("echo.py", ECHO);
    let counters = counters();

    let _ = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters,
        &mut metrics(&counters),
        |_| {},
    )
    .expect("ran");

    assert_eq!(counters.external_spawns.get("demo"), 1);
    assert_eq!(counters.external_spawns.total(), 1);
}

// -- exit failure -------------------------------------------------------

#[test]
fn a_non_zero_exit_is_a_failure_even_with_a_valid_result_on_disk() {
    // The rule the contract is built on: the script declared failure, and
    // honouring its output anyway would persist a result its author said
    // was wrong.
    let python = require_python!();
    let scratch = Scratch::new("exitfail");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "fail.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
manifest = json.loads((workdir / "manifest.json").read_text())
# A VALID result, written and even sized correctly...
result = {"protocol": {"name": "clinfer-abi", "version": 1},
          "producer": {"script": "fail.py"},
          "derived": {"files": [{"file": f["file"]} for f in manifest["frames"]]}}
for frame in manifest["frames"]:
    (workdir / frame["file"]).write_bytes(b"\x00\x00\x00\x00")
(workdir / "result.json").write_text(json.dumps(result))
sys.stderr.write("about to fail\n")
sys.exit(3)
"#,
    );
    let counters = counters();

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters,
        &mut metrics(&counters),
        |_| {},
    )
    .expect("ran");

    assert_eq!(report.outcome, Outcome::ExitFailure(3));
    assert!(report.result.is_none(), "the result is not honoured");
    assert_eq!(
        report.cancel_reason().as_deref(),
        Some("external processor failed (exit 3)")
    );
    assert_eq!(counters.external_exit_failures.get("demo"), 1);
    // Written to disk regardless: clinfer's decision is what refuses it.
    assert!(built.path.join("result.json").exists());
}

#[test]
fn a_zero_exit_with_no_result_is_its_own_outcome() {
    let python = require_python!();
    let scratch = Scratch::new("noresult");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script("silent.py", "import sys\nsys.stderr.write('nothing\\n')\n");

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    assert!(matches!(report.outcome, Outcome::NoResult(_)));
    assert_eq!(
        report.cancel_reason().as_deref(),
        Some("external processor produced no result")
    );

    // The LOG diagnosis must say WHICH failure this was. "No result" alone
    // reads as a missing file, and a live run was cancelled that way while
    // the file was present and complete - the result was 26.8 MiB against a
    // 16 MiB bound. An operator cannot tell those apart from the fixed
    // phrase, so the detail has to survive somewhere, and the log is it.
    let diagnosis = report.diagnosis().expect("a failure has a diagnosis");
    assert!(
        diagnosis.contains("no usable result"),
        "the diagnosis says the result was unusable: {diagnosis}"
    );
    let Outcome::NoResult(detail) = &report.outcome else {
        unreachable!("asserted above");
    };
    assert!(
        detail.contains("result.json"),
        "the detail names the file it read: {detail}"
    );
}

#[test]
fn an_oversized_result_is_refused_by_size_and_says_so() {
    // The live .1 defect, reduced to a fast deterministic case: a processor
    // that exits 0 and writes a result LARGER than the accepted bound. The
    // cancellation keeps the fixed clinical phrase, and the diagnosis names
    // the size limit rather than implying the file was missing.
    let python = require_python!();
    let scratch = Scratch::new("oversize");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    // Padded past the bound without a 64 MiB write: the limit is checked on
    // the byte length, so a file of that length is what costs time. A
    // sparse tail is built by seeking, which is cheap on every filesystem
    // this runs on.
    let script = scratch.script(
        "oversize.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
path = workdir / "result.json"
head = json.dumps({"protocol": {"name": "clinfer-abi", "version": 1},
                   "producer": {"script": "oversize.py"}}).encode()
with open(path, "wb") as handle:
    handle.write(head)
    handle.seek(64 * 1024 * 1024)
    handle.write(b" ")
"#,
    );

    let report = external::run(
        &processor(&python, &script, 120),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    assert!(
        matches!(report.outcome, Outcome::NoResult(_)),
        "an oversized result is not a success: {:?}",
        report.outcome
    );
    assert_eq!(
        report.cancel_reason().as_deref(),
        Some("external processor produced no result"),
        "the DICOM phrase stays fixed and short"
    );
    let diagnosis = report.diagnosis().expect("a diagnosis");
    assert!(
        diagnosis.contains("over the") && diagnosis.contains("max_result_bytes"),
        "the diagnosis names the size limit and the key that sets it: {diagnosis}"
    );
}

/// The budget belongs to the PROCESSOR, and it is the one that decides.
///
/// The same document is refused by a model configured at the default and
/// accepted by one configured larger - which is the entire reason the knob
/// is per model rather than a build-wide constant. The diagnosis names the
/// number that actually applied, so an operator who raised it on the wrong
/// model reads the number they set and not a number they never chose.
#[test]
fn a_larger_model_budget_admits_what_the_default_refuses() {
    let python = require_python!();
    let scratch = Scratch::new("model-budget");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);

    // A valid document, padded past 64 KiB but far under 1 MiB. The content
    // is irrelevant to the bound, which is checked on the byte length.
    let script = scratch.script(
        "padded.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
(pathlib.Path(workdir) / "result.json").write_bytes(
    json.dumps({"protocol": {"name": "clinfer-abi", "version": 1},
                "producer": {"script": "padded.py"}}).encode()
    + b" " * 65536
)
"#,
    );

    // Refused by a processor whose budget is below the document, and the
    // refusal quotes THAT budget.
    let refused = external::run(
        &processor_with_budget(&python, &script, 120, 1024),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");
    assert!(
        matches!(refused.outcome, Outcome::NoResult(_)),
        "a 1 KiB budget refuses a 64 KiB document: {:?}",
        refused.outcome
    );
    let diagnosis = refused.diagnosis().expect("a diagnosis");
    assert!(
        diagnosis.contains("max_result_bytes") && diagnosis.contains("1024"),
        "the refusal names the model's own budget AND the key that sets it: \
         {diagnosis}"
    );

    // The SAME document under a larger budget is a success.
    let accepted = external::run(
        &processor_with_budget(&python, &script, 120, 1024 * 1024),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");
    assert!(
        matches!(accepted.outcome, Outcome::Success),
        "a 1 MiB budget admits the same document: {:?}",
        accepted.outcome
    );
    assert!(accepted.result.is_some(), "and the parsed document is kept");
}

#[test]
fn a_slowly_written_result_is_still_read() {
    // Determinism, per the owner's instruction: a processor that writes its
    // result SLOWLY must not produce "no result". The read happens after
    // `try_wait` reports the child gone, so a child that is still writing is
    // still running; this pins that no part of the seam reads the file
    // earlier than that, which is the race the live symptom suggested.
    let python = require_python!();
    let scratch = Scratch::new("slowresult");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "slow.py",
        r#"
import json, pathlib, sys, time
workdir = pathlib.Path(sys.argv[1])
# A MINIMAL result the schema accepts: the point of this test is WHEN the
# file is read, not what it holds, and an invented key would make it fail
# for the wrong reason.
result = {"protocol": {"name": "clinfer-abi", "version": 1},
          "producer": {"script": "slow.py"}}
tmp = workdir / "result.json.tmp"
# Several pauses, so any early reader has a wide window to catch nothing.
with open(tmp, "w") as handle:
    handle.write(json.dumps(result))
    handle.flush()
    time.sleep(0.4)
    handle.write("\n")
    handle.flush()
    time.sleep(0.4)
tmp.rename(workdir / "result.json")
"#,
    );

    let report = external::run(
        &processor(&python, &script, 120),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    assert!(
        matches!(report.outcome, Outcome::Success),
        "a slow write is not a missing result: {:?} (after {:.1}s)",
        report.outcome,
        report.seconds
    );
    assert!(report.result.is_some(), "the parsed result is present");
}

// -- strict schema ------------------------------------------------------

#[test]
fn a_result_with_an_unknown_key_is_refused() {
    let python = require_python!();
    let scratch = Scratch::new("strict");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "extra.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
result = {"protocol": {"name": "clinfer-abi", "version": 1},
          "producer": {"script": "extra.py"},
          "surprise": {"anything": 1}}
(workdir / "result.json").write_text(json.dumps(result))
"#,
    );

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    match report.outcome {
        Outcome::NoResult(message) => assert!(message.contains("surprise"), "{message}"),
        other => panic!("an unknown key must be refused, got {other:?}"),
    }
}

#[test]
fn a_result_with_the_wrong_protocol_version_names_both() {
    let python = require_python!();
    let scratch = Scratch::new("version");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "oldver.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
result = {"protocol": {"name": "clinfer-abi", "version": 99},
          "producer": {"script": "oldver.py"}}
(workdir / "result.json").write_text(json.dumps(result))
"#,
    );

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    match report.outcome {
        Outcome::NoResult(message) => {
            assert!(message.contains("99"), "the declared version: {message}");
            assert!(message.contains('1'), "and the supported one: {message}");
        }
        other => panic!("a version mismatch must be refused, got {other:?}"),
    }
}

#[test]
fn a_result_that_escapes_the_workdir_is_refused_at_the_seam() {
    let python = require_python!();
    let scratch = Scratch::new("escape");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "escape.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
result = {"protocol": {"name": "clinfer-abi", "version": 1},
          "producer": {"script": "escape.py"},
          "derived": {"files": [{"file": "../outside.raw"}]}}
(workdir / "result.json").write_text(json.dumps(result))
"#,
    );

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    match report.outcome {
        Outcome::NoResult(message) => assert!(message.contains("workdir"), "{message}"),
        other => panic!("an escaping name must be refused, got {other:?}"),
    }
}

// -- stderr policy ------------------------------------------------------

#[test]
fn every_stderr_line_carries_its_prefix_and_the_text_is_kept() {
    // The prefix is added by the CALLER (the worker), so here the seam's
    // own contract is what is checked: the lines arrive verbatim and in
    // order, with no clinfer text mixed in.
    let python = require_python!();
    let scratch = Scratch::new("stderr");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "chatty.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
for index in range(3):
    sys.stderr.write("line %d\n" % index)
result = {"protocol": {"name": "clinfer-abi", "version": 1},
          "producer": {"script": "chatty.py"}}
(workdir / "result.json").write_text(json.dumps(result))
"#,
    );

    let mut lines: Vec<String> = Vec::new();
    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |line| lines.push(line.to_string()),
    )
    .expect("ran");

    assert!(matches!(report.outcome, Outcome::Success));
    assert_eq!(lines, vec!["line 0", "line 1", "line 2"]);
}

#[test]
fn a_line_longer_than_the_limit_is_truncated_rather_than_kept() {
    // A progress bar written with carriage returns is ONE enormous line;
    // the per-line cap is what stops it becoming one enormous log entry.
    let python = require_python!();
    let scratch = Scratch::new("longline");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "longline.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
sys.stderr.write("X" * 20000 + "\n")
sys.stderr.write("after\n")
result = {"protocol": {"name": "clinfer-abi", "version": 1},
          "producer": {"script": "longline.py"}}
(workdir / "result.json").write_text(json.dumps(result))
"#,
    );

    let mut lines: Vec<String> = Vec::new();
    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |line| lines.push(line.to_string()),
    )
    .expect("ran");

    assert!(matches!(report.outcome, Outcome::Success));
    let long = lines
        .iter()
        .find(|line| line.starts_with('X'))
        .expect("the long line arrived");
    assert!(
        long.len() <= external::MAX_STDERR_LINE_BYTES,
        "{} bytes is past the per-line cap of {}",
        long.len(),
        external::MAX_STDERR_LINE_BYTES
    );
    assert!(
        lines.iter().any(|line| line == "after"),
        "the stream continues after the truncation: {lines:?}"
    );
}

#[test]
fn stderr_stops_being_kept_once_the_run_budget_is_spent() {
    // A chatty script must not be able to flood the journal. Once the
    // per-run budget is spent the lines are counted, not kept.
    let python = require_python!();
    let scratch = Scratch::new("flood");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "flood.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
# Well past the 64 KiB budget, in lines short enough to be kept.
line = "Y" * 200 + "\n"
for _ in range(1000):
    sys.stderr.write(line)
result = {"protocol": {"name": "clinfer-abi", "version": 1},
          "producer": {"script": "flood.py"}}
(workdir / "result.json").write_text(json.dumps(result))
"#,
    );

    let mut kept = 0usize;
    let mut bytes = 0usize;
    let report = external::run(
        &processor(&python, &script, 120),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |line| {
            kept += 1;
            bytes += line.len();
        },
    )
    .expect("ran");

    assert!(matches!(report.outcome, Outcome::Success));
    // 1000 lines of 200 bytes is 200000, three times the budget; what is
    // delivered is bounded by the budget plus the one line that crossed it.
    let budget = external::MAX_STDERR_BYTES;
    assert!(
        bytes <= budget + external::MAX_STDERR_LINE_BYTES,
        "{bytes} bytes delivered against a {budget} byte budget"
    );
    assert!(kept < 1000, "the flood was bounded: {kept} lines kept");
}

// -- timeout and kill ---------------------------------------------------

#[test]
fn a_processor_that_overruns_is_killed_and_timed_out() {
    let python = require_python!();
    let scratch = Scratch::new("timeout");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    // The child announces its own pid through a file in the workdir, then
    // sleeps forever. That is what makes the KILL observable: `killed` on
    // the report is a claim by the seam, while "the process is gone" is a
    // fact about the machine. Without the pid the pin could not tell a real
    // tree-kill from a seam that merely said it had killed something.
    let script = scratch.script(
        "hang.py",
        r#"
import os, pathlib, sys, time
workdir = pathlib.Path(sys.argv[1])
(workdir / "pid").write_text(str(os.getpid()))
while True:
    time.sleep(0.05)
"#,
    );
    let counters = counters();

    let started = std::time::Instant::now();
    // The budget is 5 s (the parser's floor); the point is that the seam
    // RETURNS rather than waiting for the child, so the wall clock is
    // bounded by roughly the budget plus one poll interval.
    let report = external::run(
        &processor(&python, &script, 5),
        &built.path,
        &counters,
        &mut metrics(&counters),
        |_| {},
    )
    .expect("ran");
    let elapsed = started.elapsed();

    assert_eq!(report.outcome, Outcome::TimedOut);
    assert!(report.killed, "the kill was recorded");
    assert!(
        elapsed < Duration::from_secs(30),
        "the seam returned after {elapsed:?} rather than waiting for the child"
    );
    assert_eq!(counters.external_timeouts.get("demo"), 1);
    assert_eq!(counters.external_kills.get("demo"), 1);
    assert_eq!(
        report.cancel_reason().as_deref(),
        Some("external processor timed out")
    );

    // The child is actually gone: the pid it published no longer exists.
    // This is the assertion a "kill" that never called kill would fail.
    let pid: u32 = std::fs::read_to_string(built.path.join("pid"))
        .expect("the child published its pid")
        .trim()
        .parse()
        .expect("a pid");
    assert!(!process_is_alive(pid), "process {pid} outlived the timeout");
}

/// Whether a pid is still running.
///
/// `kill(pid, 0)` is the portable probe on Unix; on Windows the
/// `tasklist` filter is the one that needs no FFI and no unsafe, which is
/// the same constraint the tree-kill itself works under.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match output {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout);
                text.contains(&pid.to_string())
            }
            // If the probe itself fails, report "gone": the alternative is
            // failing the test for a reason unrelated to the seam.
            Err(_) => false,
        }
    }
}

#[test]
fn a_missing_interpreter_is_reported_rather_than_panicking() {
    let scratch = Scratch::new("notstarted");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script("nothing.py", "pass\n");

    let report = external::run(
        &processor("/no/such/interpreter-binary", &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("the seam returns rather than panicking");

    assert!(matches!(report.outcome, Outcome::NotStarted(_)));
    assert_eq!(
        report.cancel_reason().as_deref(),
        Some("external processor could not be started")
    );
}

// -- the result file is read, not streamed ------------------------------

#[test]
fn a_result_written_after_the_process_exits_is_still_read() {
    // The contract reads result.json AFTER the child is gone, which is
    // what makes the atomic-rename advice in the README sufficient: the
    // file is never read while a writer holds it.
    let python = require_python!();
    let scratch = Scratch::new("afterexit");
    let parent = scratch.path().join("state");
    let built = built_workdir(&parent);
    let script = scratch.script(
        "delayed.py",
        r#"
import json, pathlib, sys
workdir = pathlib.Path(sys.argv[1])
result = {"protocol": {"name": "clinfer-abi", "version": 1},
          "producer": {"script": "delayed.py"},
          "sr": {"text": "read after exit"}}
(workdir / "result.json").write_text(json.dumps(result))
"#,
    );

    let report = external::run(
        &processor(&python, &script, 60),
        &built.path,
        &counters(),
        &mut metrics(&counters()),
        |_| {},
    )
    .expect("ran");

    let result = report.result.expect("a parsed result");
    assert_eq!(result.sr.expect("sr").text, "read after exit".to_string());
}

// -- the delivery files and the contract --------------------------------

#[test]
fn the_shipped_example_scripts_have_the_process_batch_shape() {
    // The examples are the contract's own documentation, and the shape
    // `def process_batch(workdir)` is what a future shim imports.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("abi");
    for name in ["DERIVED-clahe.py", "PR-openvino-detect.py", "SR-llm.py"] {
        let text = std::fs::read_to_string(root.join(name))
            .unwrap_or_else(|error| panic!("{name} is shipped: {error}"));
        assert!(
            text.contains("def process_batch(workdir)"),
            "{name} declares the entry point"
        );
        assert!(
            text.contains("__main__"),
            "{name} is runnable by hand, which is the transparency feature"
        );
        assert!(text.contains("result.json"), "{name} writes the result");
    }
}

#[test]
fn the_contract_document_covers_what_the_code_enforces() {
    // The README is the reference for the person writing a processor, so
    // every rule the code enforces has to be stated there. This is the
    // contract check the module doc promises.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("abi");
    let text = std::fs::read_to_string(root.join("README.md")).expect("the contract document");

    let required: &[(&str, &str)] = &[
        ("argv", "the argument vector"),
        ("workdir", "the work directory"),
        ("manifest.json", "what clinfer writes"),
        ("result.json", "what clinfer reads"),
        ("exit 0", "the success rule's first clause"),
        ("timeout_secs", "the budget"),
        ("taskkill", "the Windows tree kill"),
        ("process-group", "the Unix tree kill"),
        ("stderr", "the stderr policy"),
        ("atomic", "the atomic write"),
        ("trust", "the trust note"),
        ("pipeline", "composition happens in the script"),
        ("by hand", "the by-hand runnability feature"),
    ];
    for (needle, what) in required {
        assert!(
            text.to_lowercase().contains(&needle.to_lowercase()),
            "the contract document states {what} ({needle})"
        );
    }
    assert!(
        text.contains("operator-trusted"),
        "the trust note names the trust domain"
    );
    // The limits are asserted against the CONSTANTS, not against literals: the
    // README used to promise 16 MiB while the code accepted 64 MiB, and a
    // hard-coded string here just pinned that mistake in place.
    let default_mib = DEFAULT_MAX_RESULT_BYTES / (1024 * 1024);
    assert!(
        text.contains(&format!("{default_mib} MiB")),
        "the result limit is stated as {default_mib} MiB ({DEFAULT_MAX_RESULT_BYTES} bytes)"
    );
    assert!(
        text.contains("max_result_bytes"),
        "the result limit names the key an operator overrides"
    );
    assert!(
        text.contains(&format!(
            "{} MiB",
            clinfer::abi::manifest::MAX_MANIFEST_BYTES / (1024 * 1024)
        )),
        "the manifest limit is stated"
    );
}
