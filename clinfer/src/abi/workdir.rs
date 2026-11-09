// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
//! The work directory an external processor is handed.
//!
//! One directory per run, holding one raw file per frame plus the manifest
//! that describes them. The directory is the unit of work: an external
//! processor may read the frames, write whatever it likes beside them, and
//! has no need to know anything about clinfer, DICOMweb or the origin. The
//! whole run is a directory a person can reproduce by hand with the same
//! interpreter, which is what makes a processor debuggable at 03:00.
//!
//! Everything written here is written once and never modified: the frames
//! and the manifest are the run's INPUT, and a processor that edited them
//! would be editing its own question.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::abi::manifest::{
    FRAMES_DIR, FrameEntry, MANIFEST_FILE, Manifest, ManifestParameter, StudyContext,
};

/// A frame on its way into a work directory.
///
/// The caller decides the order (series order, then instance order within
/// the series) and this module preserves it: the frames are the study's
/// images in the order the pipeline sees them, and a processor that returns
/// one derived image per frame relies on that order being stable.
pub struct Frame<'a> {
    /// The instance's SOP Instance UID: the link back to the source.
    pub sop_uid: &'a str,
    /// The pixel bytes, exactly as fetched.
    pub bytes: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// Bits allocated, which is also the raw file's element size.
    pub bits: u32,
    /// (0028,0103) Pixel Representation: `true` when the stored samples are
    /// two's complement.
    ///
    /// Travels with the frame for the same reason the geometry does - it is
    /// a property OF the frame. A producer that reads the bytes unsiged
    /// when the instance declared them signed gets inverted contrast, which
    /// on a CT-like source is the difference between a segmentable volume
    /// and noise.
    pub signed: bool,
    pub photometric: &'a str,
    /// (0028,0030) Pixel Spacing of the instance, when it declared one.
    ///
    /// Geometry travels with the frame because it is a property OF the
    /// frame, not of the run: a series may mix instances that declare
    /// spacing with instances that do not, and a manifest that hoisted the
    /// value to the top would have to pick one and be wrong.
    pub pixel_spacing: Option<[f64; 2]>,
    /// (0018,0088) Spacing Between Slices, when declared.
    pub slice_spacing: Option<f64>,
    /// (0020,0032) Image Position Patient, when declared.
    pub position: Option<[f64; 3]>,
    /// (0020,0037) Image Orientation (Patient), when declared.
    pub orientation: Option<[f64; 6]>,
}

/// A work directory that was built, and what is in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workdir {
    /// The directory itself, for the processor's argument vector.
    pub path: PathBuf,
    /// The frames, in the order they were written.
    pub frames: usize,
    /// The total number of pixel bytes written.
    pub bytes: u64,
    /// A canonical string over the frames as written: each frame's SOP
    /// Instance UID and the digest of its bytes, in order.
    ///
    /// This is the SEED the audit line hashes with the crate's existing
    /// `digest128`, rather than a hash of its own: the crate keeps one
    /// mixer, and the caller already has it. It is a statement about the
    /// bytes that went to disk rather than the bytes that were fetched -
    /// the two agree by construction, and recording what was written is
    /// what makes the line reconcilable with the directory afterwards.
    pub seed: String,
}

/// Errors that stop a run before the processor is even started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkdirError {
    /// The directory could not be created, or a file could not be written.
    Io(String),
    /// The caller handed in no frames: there is nothing to process.
    NoFrames,
    /// Two frames claim the same SOP Instance UID, which would give the
    /// processor two answers for one question and make the derived images
    /// ambiguous about which source they replace.
    DuplicateFrame(String),
}

impl std::fmt::Display for WorkdirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "work directory: {message}"),
            Self::NoFrames => f.write_str("the study has no frames to process"),
            Self::DuplicateFrame(uid) => {
                write!(f, "frame {uid} appears twice in the work directory")
            }
        }
    }
}

impl std::error::Error for WorkdirError {}

/// Build a work directory with no scheduled processing parameters.
///
/// The parameter list is optional to the builder because a workitem need not
/// carry (0074,1210); every existing caller of a parameterless build keeps
/// its call shape.
pub fn build(
    parent: &Path,
    label: &str,
    study: StudyContext,
    frames: &[Frame<'_>],
    clinfer_version: &str,
    profile_version: u32,
) -> Result<Workdir, WorkdirError> {
    build_with_parameters(
        parent,
        label,
        study,
        frames,
        clinfer_version,
        profile_version,
        &[],
    )
}

/// Build a work directory whose manifest carries the requester's parameters.
///
/// `label` is part of the directory name, so an operator looking at the
/// directory listing can tell which study and which model produced it
/// without opening the manifest. Nothing is reused and nothing is
/// overwritten: a name that is taken means an earlier run left it behind,
/// and this run gets its own directory rather than inheriting someone
/// else's leftovers.
pub fn build_with_parameters(
    parent: &Path,
    label: &str,
    study: StudyContext,
    frames: &[Frame<'_>],
    clinfer_version: &str,
    profile_version: u32,
    parameters: &[ManifestParameter],
) -> Result<Workdir, WorkdirError> {
    if frames.is_empty() {
        return Err(WorkdirError::NoFrames);
    }
    let mut seen: Vec<&str> = Vec::with_capacity(frames.len());
    for frame in frames {
        if seen.contains(&frame.sop_uid) {
            return Err(WorkdirError::DuplicateFrame(frame.sop_uid.to_string()));
        }
        seen.push(frame.sop_uid);
    }

    let path = unique_dir(parent, label)?;
    let frames_dir = path.join(FRAMES_DIR);
    fs::create_dir_all(&frames_dir).map_err(|error| WorkdirError::Io(error.to_string()))?;

    let mut entries: Vec<FrameEntry> = Vec::with_capacity(frames.len());
    let mut total: u64 = 0;
    // The seed is built from the bytes that were WRITTEN, so it is
    // computed from the same slice the write consumes, in the same loop,
    // in the same order.
    let mut seed = String::new();
    for (position, frame) in frames.iter().enumerate() {
        // 1-based, which is what the contract calls the frame's identity:
        // the index and the file name's number are the same number, so a
        // processor and an operator are looking at the same arithmetic.
        let index = position + 1;
        let file = Manifest::frame_file(position as u32);
        let target = frames_dir.join(&file);
        fs::write(&target, frame.bytes).map_err(|error| WorkdirError::Io(error.to_string()))?;
        if !seed.is_empty() {
            seed.push('|');
        }
        // The UID is what makes the seed a statement about WHICH frames,
        // not only how many bytes: two runs of the same study and the same
        // model must digest the same, and a run over a different study
        // must not.
        seed.push_str(frame.sop_uid);
        seed.push(':');
        seed.push_str(&frame_digest(frame.bytes));
        total += frame.bytes.len() as u64;
        entries.push(FrameEntry {
            index: index as u32,
            file,
            sop_uid: frame.sop_uid.to_string(),
            width: frame.width,
            height: frame.height,
            bits: frame.bits,
            photometric: frame.photometric.to_string(),
            bytes: frame.bytes.len() as u64,
            signed: frame.signed,
            // Copied through unchanged, `None` included: the builder does
            // not know what the geometry means and must not substitute a
            // value for an attribute the instance did not declare.
            pixel_spacing: frame.pixel_spacing,
            slice_spacing: frame.slice_spacing,
            position: frame.position,
            orientation: frame.orientation,
        });
    }

    let manifest = Manifest {
        clinfer_version: clinfer_version.to_string(),
        study,
        profile_version,
        parameters: parameters.to_vec(),
        frames: entries,
    };
    let text = manifest.to_json();
    fs::write(path.join(MANIFEST_FILE), text.as_bytes())
        .map_err(|error| WorkdirError::Io(error.to_string()))?;

    Ok(Workdir {
        path,
        frames: frames.len(),
        bytes: total,
        seed,
    })
}

/// A directory name that is not taken.
///
/// The name is derived from the label so it is readable, and a suffix is
/// added only when the base is already there. Nothing is ever removed to
/// make room: a leftover directory is either an interrupted run an operator
/// wants to look at, or another run's live work.
fn unique_dir(parent: &Path, label: &str) -> Result<PathBuf, WorkdirError> {
    fs::create_dir_all(parent).map_err(|error| WorkdirError::Io(error.to_string()))?;
    let safe = sanitize(label);
    for suffix in 0..1000u32 {
        let name = if suffix == 0 {
            safe.clone()
        } else {
            format!("{safe}-{suffix}")
        };
        let candidate = parent.join(name);
        // `create_dir` is the check: an `exists()` probe followed by a create
        // is a window two concurrent runs of the same study fit through, and
        // the loser then writes its frames into the winner's directory.
        // AlreadyExists is the ordinary "name taken" answer.
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(WorkdirError::Io(error.to_string())),
        }
    }
    Err(WorkdirError::Io(format!(
        "no free directory name for '{label}' under {}",
        parent.display()
    )))
}

/// A filesystem-safe form of a label: UIDs and model ids, nothing else.
///
/// A UID is already safe, but a model id comes from the configuration and a
/// configuration is a file a person edits, so the unsafe characters are
/// replaced rather than trusted.
fn sanitize(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "work".to_string()
    } else {
        out
    }
}

/// FNV-1a over one frame's bytes, as sixteen hexadecimal digits.
///
/// The same mixer the crate's `digest128` uses, kept local so the builder
/// stays a leaf module with no dependency on the worker. The result is a
/// field of the seed rather than the run's digest: the caller hashes the
/// whole seed with `digest128`, so the crate still has ONE published hash.
fn frame_digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// How long a work directory is kept before the age sweep removes it.
///
/// A constant rather than a configuration key, per the owner's direction: a
/// workdir is either an abandoned run or a forensic artefact, and neither
/// case wants a per-site number. 24 hours is long enough to look at a
/// failure the next morning and short enough that a study's frames do not
/// sit on the disk indefinitely.
pub const SWEEP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Age the sweep uses for a directory it cannot stat.
///
/// Reported rather than guessed at: an entry whose age cannot be read is
/// left in place, because deleting something on the strength of a failed
/// `stat` is how a sweep removes the directory an operator is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swept {
    /// Removed: it was older than [`SWEEP_AGE`].
    Removed,
    /// Kept: younger than the threshold.
    Kept,
    /// Kept: its age could not be determined.
    Unknown,
}

/// Remove work directories under `parent` older than [`SWEEP_AGE`].
///
/// Runs at startup rather than on a timer: a sidecar that is up is busy
/// processing, and a sidecar that restarts is exactly the moment nobody is
/// using the directories it left behind. Returns one entry per directory
/// that was considered, so the caller can report what it did.
///
/// The AGE comes from the manifest inside each directory, not from the
/// directory's own timestamp. Two reasons, and both matter:
///
/// - the manifest is written ONCE, when the run starts, so its timestamp is
///   the run's age. A directory's own mtime moves whenever anything is
///   written into it, so a processor that touches its scratch files would
///   keep an abandoned run alive indefinitely;
/// - the age of a FILE can be set, which is what makes this rule pinnable
///   without waiting a day.
///
/// A directory with no manifest is not a run clinfer started (or its
/// manifest was removed) and is left alone rather than guessed at. Only
/// DIRECTORIES are considered: `cache_dir` is shared with the PR markers,
/// and a sweep that deleted those would turn every study into a duplicate
/// store.
pub fn sweep(parent: &Path, now: SystemTime) -> Vec<(PathBuf, Swept)> {
    let mut outcome = Vec::new();
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        // A missing directory means nothing was ever written: not an error.
        Err(_) => return outcome,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest = path.join(MANIFEST_FILE);
        let created = match fs::metadata(&manifest).and_then(|meta| meta.modified()) {
            Ok(modified) => modified,
            // No manifest, or an unreadable one: kept, and NOT reported as
            // removable, because the alternative is deleting a directory on
            // the strength of a failed `stat`.
            Err(_) => {
                outcome.push((path, Swept::Unknown));
                continue;
            }
        };
        let verdict = match now.duration_since(created) {
            Ok(age) if age >= SWEEP_AGE => {
                if fs::remove_dir_all(&path).is_ok() {
                    Swept::Removed
                } else {
                    // A removal that fails (a file held open by a live
                    // processor) is kept and named, not retried forever.
                    Swept::Unknown
                }
            }
            Ok(_) => Swept::Kept,
            // A timestamp in the future: kept, and named, because a clock
            // that moved backwards is a fact an operator wants to see
            // rather than have acted upon.
            Err(_) => Swept::Unknown,
        };
        outcome.push((path, verdict));
    }
    outcome
}

/// The directory work directories are created under, given the cache root.
///
/// One place, so the builder and the sweep cannot disagree about where the
/// runs live - a sweep looking in the wrong directory is silent.
#[must_use]
pub fn root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("abi")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn study() -> StudyContext {
        StudyContext {
            uid: "1.2.3.4".to_string(),
            description: Some("Fixture".to_string()),
            modalities: vec!["DX".to_string()],
            body_part: Some("CHEST".to_string()),
        }
    }

    fn frame<'a>(uid: &'a str, bytes: &'a [u8]) -> Frame<'a> {
        Frame {
            sop_uid: uid,
            bytes,
            width: 4,
            height: 3,
            bits: 8,
            signed: false,
            photometric: "MONOCHROME2",
            // No geometry: the fixture's instances declare none, which is
            // the `None` case the manifest has to publish as an ABSENT key.
            pixel_spacing: None,
            slice_spacing: None,
            position: None,
            orientation: None,
        }
    }
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("clinfer-workdir-{name}"));
        let _ = fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn the_frames_are_written_in_order_with_their_manifest() {
        let parent = scratch("ordered");
        let frames = [frame("1.2.3.1", &[1, 2]), frame("1.2.3.2", &[3, 4, 5])];
        let built = build(&parent, "study-A", study(), &frames, "0.3.0", 2).expect("built");
        assert_eq!(built.frames, 2);
        assert_eq!(built.bytes, 5);
        let first = fs::read(built.path.join(FRAMES_DIR).join("0000.raw")).expect("0000");
        let second = fs::read(built.path.join(FRAMES_DIR).join("0001.raw")).expect("0001");
        assert_eq!(first, vec![1, 2]);
        assert_eq!(second, vec![3, 4, 5]);
        let manifest = fs::read_to_string(built.path.join(MANIFEST_FILE)).expect("manifest");
        assert!(manifest.contains("\"1.2.3.1\""), "{manifest}");
        assert!(manifest.contains("\"0001.raw\""), "{manifest}");
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    #[test]
    fn declared_geometry_reaches_the_written_manifest() {
        // The builder is the last place the geometry exists in Rust before
        // it becomes the document a producer reads. A value dropped here
        // would be invisible: the manifest would parse, the script would
        // run, and the volume would be placed wrongly.
        let parent = scratch("geometry");
        let mut first = frame("1.2.3.1", &[1, 2]);
        first.pixel_spacing = Some([0.5, 0.5]);
        first.slice_spacing = Some(12.0);
        first.position = Some([-12.4, -102.0, 6.0]);
        first.orientation = Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        // The second frame declares NOTHING, so the document has to show
        // both cases in one run.
        let second = frame("1.2.3.2", &[3, 4]);
        let built = build(
            &parent,
            "study-G",
            study(),
            &[first, second],
            "0.3.0-alpha",
            1,
        )
        .expect("built");

        let manifest = fs::read_to_string(built.path.join(MANIFEST_FILE)).expect("manifest");
        let parsed = crate::json::parse(manifest.as_bytes()).expect("the manifest parses");
        let frames = parsed
            .get("frames")
            .and_then(|value| value.as_array())
            .expect("frames array");

        assert_eq!(
            frames[0]
                .get("pixel_spacing")
                .and_then(|value| value.as_array())
                .map(|values| values.len()),
            Some(2)
        );
        assert_eq!(
            frames[0]
                .get("slice_spacing")
                .and_then(|value| value.as_f64()),
            Some(12.0)
        );
        assert_eq!(
            frames[0]
                .get("orientation")
                .and_then(|value| value.as_array())
                .map(|values| values.len()),
            Some(6)
        );
        assert!(
            frames[1].get("pixel_spacing").is_none(),
            "a frame that declared no geometry must not gain one in transit"
        );
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    #[test]
    fn a_geometry_free_fixture_writes_no_geometry_keys() {
        // The shape every existing producer already reads. If this ever
        // starts emitting keys, the document stopped being additive and
        // somebody's parser is about to see a field it has no case for.
        let parent = scratch("nogeom");
        let frames = [frame("1.2.3.1", &[1, 2])];
        let built = build(&parent, "study-H", study(), &frames, "0.3.0", 1).expect("built");
        let manifest = fs::read_to_string(built.path.join(MANIFEST_FILE)).expect("manifest");
        for key in ["pixel_spacing", "slice_spacing", "position", "orientation"] {
            assert!(!manifest.contains(key), "{key} must be absent: {manifest}");
        }
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    #[test]
    fn an_empty_run_is_refused_rather_than_started() {
        // Starting a processor with nothing to process would produce a
        // result that names no derived files, which the chain would then
        // have to treat as a failure - better to refuse before the spawn.
        let parent = scratch("empty");
        let err = build(&parent, "study-B", study(), &[], "0.3.0", 1).expect_err("refused");
        assert_eq!(err, WorkdirError::NoFrames);
        fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn the_same_frame_twice_is_refused() {
        // Two files for one SOP Instance UID would make the derived images
        // ambiguous about which source instance they replace.
        let parent = scratch("duplicate");
        let frames = [frame("1.2.3.1", &[1]), frame("1.2.3.1", &[2])];
        let err = build(&parent, "study-C", study(), &frames, "0.3.0", 1).expect_err("refused");
        assert_eq!(err, WorkdirError::DuplicateFrame("1.2.3.1".to_string()));
        fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn an_existing_directory_is_not_reused() {
        // A leftover directory is either an interrupted run an operator
        // wants to inspect or another run's live work. Neither is something
        // this run may write into.
        let parent = scratch("unique");
        let frames = [frame("1.2.3.1", &[1])];
        let first = build(&parent, "study-D", study(), &frames, "0.3.0", 1).expect("first");
        fs::write(first.path.join("leftover.txt"), b"keep me").expect("marker");
        let second = build(&parent, "study-D", study(), &frames, "0.3.0", 1).expect("second");
        assert_ne!(first.path, second.path, "a fresh directory");
        assert!(
            first.path.join("leftover.txt").exists(),
            "the earlier run is left alone"
        );
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    #[test]
    fn the_seed_covers_the_bytes_and_their_order() {
        // The seed is what an operator's digest is taken over, so it must
        // change when the bytes do and when the order does - otherwise the
        // audit line records nothing the directory listing does not.
        let parent = scratch("digest");
        let one = build(
            &parent,
            "study-E",
            study(),
            &[frame("1.2.3.1", &[1, 2]), frame("1.2.3.2", &[3])],
            "0.3.0",
            1,
        )
        .expect("one");
        let other = build(
            &parent,
            "study-E",
            study(),
            &[frame("1.2.3.1", &[1, 3]), frame("1.2.3.2", &[3])],
            "0.3.0",
            1,
        )
        .expect("other");
        assert_ne!(one.seed, other.seed, "a changed byte changes the seed");
        let reordered = build(
            &parent,
            "study-E",
            study(),
            &[frame("1.2.3.2", &[3]), frame("1.2.3.1", &[1, 2])],
            "0.3.0",
            1,
        )
        .expect("reordered");
        assert_ne!(
            one.seed, reordered.seed,
            "the frame order is part of what is recorded"
        );
        // And the UID is in it, so a run over a different study differs
        // even when the byte counts happen to match.
        let renamed = build(
            &parent,
            "study-E",
            study(),
            &[frame("9.9.9.1", &[1, 2]), frame("1.2.3.2", &[3])],
            "0.3.0",
            1,
        )
        .expect("renamed");
        assert_ne!(one.seed, renamed.seed, "the SOP UID is in the seed");
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    #[test]
    fn a_label_with_a_separator_does_not_escape_the_parent() {
        // The label carries a study UID and a model id, and a model id is
        // read from a configuration file. Nothing in it may move the
        // directory out of the parent it was asked for.
        let parent = scratch("sanitize");
        let frames = [frame("1.2.3.1", &[1])];
        let built = build(&parent, "../../etc", study(), &frames, "0.3.0", 1).expect("built");
        let parent = fs::canonicalize(&parent).expect("parent exists");
        let path = fs::canonicalize(&built.path).expect("built exists");
        assert!(
            path.starts_with(&parent),
            "{} stayed under {}",
            path.display(),
            parent.display()
        );
        // The separators are gone rather than escaped, so what is left is a
        // single directory NAME: the ".." survives as text but has nothing
        // to traverse, which is why the check is on the NAME's components
        // and not on the text.
        let name = built
            .path
            .file_name()
            .expect("a directory name")
            .to_string_lossy()
            .to_string();
        assert_eq!(name, ".._.._etc");
        assert_eq!(
            path.components().count(),
            parent.components().count() + 1,
            "{} is one level under {}",
            path.display(),
            parent.display()
        );
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    /// Age a path by writing its modification time into the past.
    ///
    /// The sweep decides on the manifest's own timestamp, so that is what
    /// the test moves. Injecting the age rather than waiting 24 hours is
    /// the only way to pin a time-based rule.
    fn age_by(path: &Path, seconds: u64) {
        let when = SystemTime::now() - Duration::from_secs(seconds);
        // Write access, not `File::open`: the read-only handle Windows
        // gives a plain open cannot change a timestamp.
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for aging");
        file.set_modified(when).expect("set mtime");
    }

    #[test]
    fn the_sweep_removes_only_directories_older_than_the_threshold() {
        // The rule the owner set: an abandoned run goes, a fresh one stays.
        // The threshold is the ONLY thing that decides, which is what keeps
        // a failed run (kept for forensics) from being swept the moment it
        // fails.
        let parent = scratch("sweep");
        let frames = [frame("1.2.3.1", &[1])];
        let old = build(&parent, "studies-old", study(), &frames, "0.3.0", 1).expect("old");
        let fresh = build(&parent, "studies-fresh", study(), &frames, "0.3.0", 1).expect("fresh");
        // One second past the threshold, and one second short of it. The
        // age is the MANIFEST's: a directory whose scratch files were
        // touched during a long run must not be kept alive by that.
        age_by(&old.path.join(MANIFEST_FILE), SWEEP_AGE.as_secs() + 1);
        age_by(&fresh.path.join(MANIFEST_FILE), SWEEP_AGE.as_secs() - 1);

        let outcome = sweep(&parent, SystemTime::now());
        assert!(!old.path.exists(), "the old workdir is gone");
        assert!(fresh.path.exists(), "the fresh workdir is still there");
        assert_eq!(outcome.len(), 2);
        assert!(
            outcome
                .iter()
                .any(|(path, verdict)| path == &old.path && *verdict == Swept::Removed)
        );
        assert!(
            outcome
                .iter()
                .any(|(path, verdict)| path == &fresh.path && *verdict == Swept::Kept)
        );
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    #[test]
    fn a_directory_with_no_manifest_is_kept_rather_than_guessed_at() {
        // An unreadable or absent manifest means the age is unknown, and
        // deleting on a failed `stat` is how a sweep removes the directory
        // an operator is looking at.
        let parent = scratch("sweep-nomanifest");
        fs::create_dir_all(&parent).expect("parent");
        let orphan = parent.join("not-a-run");
        fs::create_dir_all(&orphan).expect("orphan");

        let outcome = sweep(&parent, SystemTime::now());
        assert!(orphan.exists(), "a directory with no manifest is kept");
        assert_eq!(outcome, vec![(orphan, Swept::Unknown)]);
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    #[test]
    fn the_sweep_leaves_marker_files_alone() {
        // `cache_dir` is shared with the PR markers (`pr-<study>-v<n>.done`)
        // and a marker is what stops a second store of the same result.
        // Deleting one would silently turn every study into a duplicate.
        let parent = scratch("sweep-markers");
        fs::create_dir_all(&parent).expect("parent");
        let marker = parent.join("pr-1.2.3-v1.done");
        fs::write(&marker, "study=1.2.3").expect("marker");
        age_by(&marker, SWEEP_AGE.as_secs() + 1);

        let outcome = sweep(&parent, SystemTime::now());
        assert!(marker.exists(), "the marker survived an old timestamp");
        assert!(outcome.is_empty(), "a file is not a workdir: {outcome:?}");
        fs::remove_dir_all(&parent).expect("cleanup");
    }

    #[test]
    fn the_sweep_of_a_directory_that_does_not_exist_is_not_an_error() {
        // On a fresh deployment nothing has ever been written, and a sweep
        // that failed here would stop the sidecar from starting.
        let parent = scratch("sweep-missing");
        let missing = parent.join("never-created");
        assert!(sweep(&missing, SystemTime::now()).is_empty());
        assert!(!missing.exists(), "the sweep did not create it");
    }

    #[test]
    fn the_workdir_root_is_the_one_the_sweep_looks_in() {
        // A sweep and a builder that disagree about the location is silent:
        // the runs pile up forever and nothing reports it. This pins the
        // one function both use.
        let cache = Path::new("/var/lib/clinfer/cache");
        assert_eq!(root(cache), cache.join("abi"));
    }
}
