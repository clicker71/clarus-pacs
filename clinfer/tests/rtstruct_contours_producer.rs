//! The threshold demo producer `RTSTRUCT-contours.py`: coordinate order.
//!
//! The 24.09 Weasis run found that BOTH contour producers emitted the OpenCV
//! pixel pair transposed - `[row, column]` where the ABI contract is
//! `[column, row]` - so every contour landed mirrored across the main
//! diagonal of the image. The TotalSegmentator producer has its own red fact
//! in `totalseg_producer.rs`; this file pins the DEMO producer, which the
//! discipline is copied FROM: fixing only the file the live run happened to
//! hit would leave the source of the copy still wrong.
//!
//! The fixture is deliberately ASYMMETRIC. Every earlier contour fixture was a
//! square patch, and a square is transpose-invariant: swapping column and row
//! maps the patch onto itself, so no coordinate assertion against a square
//! could ever have failed. That is exactly why the bug shipped.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use clinfer::abi::external::{self, Outcome, Processor};
use clinfer::abi::schema::DEFAULT_MAX_RESULT_BYTES;
use clinfer::abi::workdir::{self, Frame};
use clinfer::receiver::Counters;

/// The side of the fixture's frames, in pixels.
const SIDE: u64 = 64;

/// A temporary directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "clinfer-contours-{label}-{}-{}",
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

/// The demo producer's real body, staged from its source tree.
///
/// The file lives beside this crate rather than in it, so it is read at test
/// time: a copy checked into the test would drift from the producer the site
/// installs. The shared parameter reader beside it is staged too - an
/// interpreter resolves `import params` against the SCRIPT's directory, so a
/// script staged alone could not import the reader it now uses.
fn real_producer(scratch: &Scratch) -> PathBuf {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("abi");
    for name in ["params.py", "RTSTRUCT-contours.py"] {
        let body = std::fs::read_to_string(directory.join(name))
            .unwrap_or_else(|error| panic!("reading {name}: {error}"));
        scratch.script(name, &body);
    }
    scratch.path().join("RTSTRUCT-contours.py")
}

impl Scratch {
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, body).expect("script");
        path
    }
}

/// An interpreter that imports the producer's dependencies, or `None`.
///
/// The seam clears the child's environment down to PATH and PYTHONUNBUFFERED,
/// so an interpreter whose packages arrive through PYTHONPATH or a user site
/// imports them in a shell and fails under the seam. Checking with
/// `python --version` would skip on a machine where the test could pass and
/// run on one where it cannot: the check imports the modules under an
/// environment as bare as the seam's.
fn producer_interpreter() -> Option<String> {
    if let Some(explicit) = std::env::var_os("CLINFER_TEST_PYTHON") {
        let path = PathBuf::from(explicit);
        if carries_producer_deps(&path) {
            return Some(path.display().to_string());
        }
        eprintln!(
            "CLINFER_TEST_PYTHON={} cannot import numpy and cv2 with the seam's \
             cleared environment",
            path.display()
        );
        return None;
    }
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
    std::process::Command::new(interpreter)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("PYTHONUNBUFFERED", "1")
        .arg("-c")
        .arg("import numpy, cv2")
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
                    "skipping: no interpreter here imports numpy and cv2 with the \
                     seam's cleared environment. Set CLINFER_TEST_PYTHON to the \
                     site's venv python to run this."
                );
                return;
            }
        }
    };
}

/// A one-frame workdir holding a RECTANGULAR bone blob that is not square.
///
/// The blob is 8 rows by 20 columns in the ROW-MAJOR frame the demo reads, so
/// the column extent and the row extent differ and a transposed emit is
/// visible: the first coordinate must fall in the column range and the second
/// in the row range. 20 x 8 = 160 px is above the producer's `MIN_AREA_PX`
/// (100), so the blob is not discarded as dust.
///
/// The pixel value is inside the producer's BONE window (300..=1100), so the
/// bone threshold finds it; every other pixel is 0 (air), which no threshold
/// matches.
fn blob_workdir(parent: &Path) -> workdir::Workdir {
    let mut pixels = vec![0u8; (SIDE * SIDE * 2) as usize];
    // Rows 8..16, columns 30..50, little-endian u16 with the value 500.
    for row in 8..16u64 {
        for column in 30..50u64 {
            let at = ((row * SIDE + column) * 2) as usize;
            pixels[at] = 500u16.to_le_bytes()[0];
            pixels[at + 1] = 500u16.to_le_bytes()[1];
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
        pixel_spacing: Some([0.16, 0.16]),
        slice_spacing: Some(1.0),
        position: Some([0.0, 0.0, 0.0]),
        orientation: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
    }];

    workdir::build(
        parent,
        "blob-study",
        clinfer::abi::StudyContext {
            uid: "1.2.840.113857.2409.1".to_string(),
            description: Some("BLOB".to_string()),
            modalities: vec!["CT".to_string()],
            body_part: Some("MANDIBLE".to_string()),
        },
        &frames,
        "0.3.0-alpha",
        1,
    )
    .expect("workdir")
}

fn processor(interpreter: &str, script: &Path) -> Processor {
    Processor {
        id: "rtstruct_contours".to_string(),
        interpreter: PathBuf::from(interpreter),
        script: script.to_path_buf(),
        timeout_secs: 60,
        max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
    }
}

#[test]
fn the_demo_producer_emits_column_then_row_not_transposed() {
    // T2 of the 24.09 prompt: the SAME assertion T1 makes for the
    // TotalSegmentator producer, run against the threshold demo. The demo is
    // the discipline source every other contour producer copies, so it has to
    // be pinned, not merely fixed.
    //
    // The blob is rows 8..16 by columns 30..50. Every emitted point must have
    // its first coordinate (the ABI's column) in 30..=49 and its second (the
    // row) in 8..=15. A transposed emit puts a row in the first slot and
    // fails on the first point.
    let python = require_python!();

    let scratch = Scratch::new("column-row-order");
    let built = blob_workdir(&scratch.path().join("state"));
    let script = real_producer(&scratch);

    let counters = Counters::default();
    let report = external::run(
        &processor(&python, &script),
        &built.path,
        &counters,
        &mut external::ChildMetrics::new(&counters, "contours-test"),
        |line| eprintln!("[producer] {line}"),
    )
    .expect("the seam runs the producer");
    assert_eq!(
        report.outcome,
        Outcome::Success,
        "the demo producer must succeed: {:?}",
        report.diagnosis()
    );

    let result = report.result.expect("a parsed result");
    let rs = result
        .rtstruct
        .expect("the BONE blob produces an rtstruct block");
    let bone = rs
        .rois
        .iter()
        .find(|roi| roi.name == "BONE")
        .expect("the bone window finds the blob");

    let mut checked = 0;
    for contour in &bone.contours {
        for point in &contour.points {
            assert!(
                (30.0..=49.0).contains(&point[0]),
                "point {point:?} has its first coordinate {} outside the \
                 column range 30..=49: the pair is (column, row), so a value \
                 in 8..=15 here means the OpenCV pair was emitted transposed",
                point[0]
            );
            assert!(
                (8.0..=15.0).contains(&point[1]),
                "point {point:?} has its second coordinate {} outside the row \
                 range 8..=15: the pair is (column, row)",
                point[1]
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "the rectangular blob must produce at least one contour"
    );
}
