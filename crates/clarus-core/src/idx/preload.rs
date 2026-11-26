// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! RAM preload of recent date + modality `.idx` files (B-010).
//!
//! At startup the server maps the most recent N day files for the date index
//! and each modality index read-only. Mapping (not reading) leaves the pages
//! in the OS page cache, so the first QIDO query over a hot day pays zero
//! page faults. The plan budgets ~4 MB for this - a few hundred KB per
//! mapped file plus the virtual-address overhead.
//!
//! # Immutability contract
//!
//! Preload only maps *immutable* (rolled-over) files. Mapping today's mutable
//! `O_APPEND` file would race with concurrent writers (growing file -> SIGBUS
//! on past-the-end access). The rollover guarantees a file is never written
//! again before it is exposed to mmap consumers, so preload runs for the
//! `N-1` most recent days, excluding today.

// The mmap constructor lives in `crate::ffi::map_sealed_day`: soundness
// depends on the file being immutable, which only the PUBLICATION PROTOCOL
// establishes. That wrapper accepts a [`SealedIdxRoot`] - the rollover
// writer's own root - so the entry points here take one rather than any path
// a caller happens to hold. Excluding `today` is a rule about WHICH days are
// sealed, not a proof about a caller-chosen directory.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Index files preload could not inspect or map, by day.
///
/// Preloading is best-effort: one unreadable day must not cost the cache the
/// other days it could still hold, so a failure is skipped rather than
/// propagated. Skipping SILENTLY was the other half of that trade - an
/// unreadable directory looks exactly like an empty one, and an unmappable day
/// looks like a day that was never sealed, so an operator saw a cold cache with
/// no cause. The count makes the difference visible without changing the
/// best-effort policy.
static SKIPPED: AtomicU64 = AtomicU64::new(0);

/// How many index files preload has skipped so far.
#[cfg(test)]
pub(crate) fn skipped_preloads() -> u64 {
    SKIPPED.load(Ordering::Relaxed)
}

/// Record one skip.
fn note_skipped() {
    SKIPPED.fetch_add(1, Ordering::Relaxed);
}

/// Errors produced during preload.
#[derive(Debug)]
pub enum PreloadError {
    /// A directory walk or open failed.
    Io(std::io::Error),
    /// No candidate day files to preload (nothing to warm).
    NothingToPreload,
}

impl std::fmt::Display for PreloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreloadError::Io(e) => write!(f, "preload I/O error: {e}"),
            PreloadError::NothingToPreload => write!(f, "no index files to preload"),
        }
    }
}

impl std::error::Error for PreloadError {}

impl From<std::io::Error> for PreloadError {
    fn from(e: std::io::Error) -> Self {
        PreloadError::Io(e)
    }
}

/// A read-only memory-mapped immutable index file.
pub struct MappedIndex {
    /// Full path of the mapped file.
    pub path: PathBuf,
    /// The day key (`YYYYMMDD`) this file covers.
    pub day: [u8; 8],
    /// The mapped bytes, plus the descriptor they came from. Read-only; the
    /// underlying file must never be truncated or replaced while a mapping is
    /// alive - `MappedFile` pins the handle so closing it cannot happen.
    mmap: crate::ffi::MappedFile,
}

impl MappedIndex {
    /// Access the mapped row bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.mmap
    }

    /// Number of mapped bytes.
    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }
}

/// An index directory holding sealed, rolled-over days.
///
/// The immutable file class preload maps is defined by the WRITER's protocol:
/// it only appends to today's file and moves to a new name on rollover, so
/// every day it has rolled PAST is complete and is never written again. An
/// arbitrary `&Path` cannot express that - a caller could name a directory of
/// files some other program rewrites - so the safe preload entry points take
/// this type instead. It is obtained from the configured `idx/` root, which is
/// the only place the rollover writer publishes into, and it carries the
/// writer's actual sealing snapshot (`sealed` days + the current day), not a
/// wall-clock date: a day is mappable only if the writer has sealed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedIdxRoot {
    root: PathBuf,
    /// Days the rollover writer has sealed (rolled over and never written
    /// again). Only these days are immutable; every other day - including the
    /// writer's current day - is still appendable and must not be mapped.
    sealed: HashSet<[u8; 8]>,
    /// The day the rollover writer currently appends to (never sealed).
    current_day: [u8; 8],
}

impl SealedIdxRoot {
    /// Construct from the rollover writer's registry.
    ///
    /// Safe because the writer owns the root and is the only protocol that
    /// seals past days (it appends to today's file only and rolls over to a
    /// new name). The snapshot of the writer's sealing state is taken here, so
    /// the returned root maps exactly the days the writer has sealed and
    /// nothing else - the exact file class
    /// [`crate::ffi::map_sealed_day`] may map.
    ///
    /// The caller must hold the lock that serialises the writer's rollover
    /// (the shadow tier's inner mutex does) so the snapshot cannot race a
    /// concurrent `rollover_if_needed`.
    pub fn from_writer(set: &crate::idx::rollover::DailyIndexSet) -> Self {
        Self {
            root: set.root().clone(),
            sealed: set.immutable_days().clone(),
            current_day: *set.current_day(),
        }
    }

    /// Whether `day` is sealed (rolled over and never written again).
    ///
    /// A day is mappable only if it is in the writer's sealed set AND is not
    /// the day the writer currently appends to. The second test is redundant
    /// with the set (the current day is never inserted into `immutable_days`),
    /// but it keeps the exclusion explicit and the safety argument local.
    #[must_use]
    pub fn is_sealed(&self, day: &[u8; 8]) -> bool {
        self.sealed.contains(day) && day != &self.current_day
    }

    /// The `idx/` directory this root names.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `dates/` day directory.
    #[must_use]
    pub fn dates_dir(&self) -> PathBuf {
        self.root.join("dates")
    }

    /// The `modality/` parent directory (one subdirectory per modality).
    #[must_use]
    pub fn modality_root(&self) -> PathBuf {
        self.root.join("modality")
    }
}

/// Parse a `YYYYMMDD.idx` file name into its `[u8; 8]` day key (or `None`).
fn parse_day(name: &str) -> Option<[u8; 8]> {
    if !name.ends_with(".idx") {
        return None;
    }
    let stem = &name[..name.len() - 4];
    if stem.len() != 8 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut day = [b'0'; 8];
    day.copy_from_slice(stem.as_bytes());
    Some(day)
}

/// Map the most recent `n` *immutable* day files under `dir`.
///
/// A day is mappable only if the root's sealing snapshot says it is sealed
/// ([`SealedIdxRoot::is_sealed`]) AND it is not the caller-supplied `today`
/// (a belt-and-suspenders exclusion; the writer's current day is already
/// excluded by the snapshot). Files are selected by parsing each
/// `YYYYMMDD.idx` name, walking newest-first, and mapping them read-only; a
/// day that cannot be mapped is skipped and the next-oldest takes its slot, so
/// the result holds up to `n` mappings. Returns the mappings sorted
/// newest-first.
///
/// Sealing is the WRITER's state, not the wall clock: if the writer still
/// targets an older day while the clock has advanced, that older day is its
/// current day (not sealed) and must not be mapped. The caller's `today` alone
/// cannot establish that - hence the snapshot in the root.
///
/// `dir` is a day directory of a [`SealedIdxRoot`]. The rollover writer is the
/// reason a day other than its current day is immutable, and that writer only
/// publishes under the configured root; naming the directory through the root
/// type is what makes the mapping's soundness argument reachable from the
/// signature.
pub fn preload_recent(
    root: &SealedIdxRoot,
    dir: &Path,
    today: [u8; 8],
    n: usize,
) -> Result<Vec<MappedIndex>, PreloadError> {
    // A `..` component would make the prefix check below unsound: it compares
    // path COMPONENTS, not resolved paths, so `/idx/../scratch` shares its
    // leading components with `/idx` while resolving outside the sealed tree.
    // Reject the component before any directory is opened, or `map_sealed_day`
    // below would map a file the rollover writer never sealed.
    if dir
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(PreloadError::Io(std::io::Error::from(
            std::io::ErrorKind::InvalidInput,
        )));
    }
    // The tree is claimed sealed by `root`; a directory outside it is not part
    // of that claim, so it is refused rather than mapped on trust.
    if !dir.starts_with(root.root()) {
        return Err(PreloadError::Io(std::io::Error::from(
            std::io::ErrorKind::InvalidInput,
        )));
    }
    let mut days: Vec<[u8; 8]> = fs::read_dir(dir)?
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            // An unreadable ENTRY is counted rather than dropped: the day it
            // names is not known to be absent, only unknown.
            Err(_) => {
                note_skipped();
                None
            }
        })
        .filter_map(|e| {
            let name = e.file_name();
            parse_day(&name.to_string_lossy())
        })
        .filter(|d| root.is_sealed(d) && *d != today)
        .collect();
    // Highest day value == most recent (8-digit zero-padded YYYYMMDD).
    days.sort_unstable_by(|a, b| b.cmp(a));
    // Do NOT truncate to `n` here: a day that fails to open or map must be
    // replaced by the next-oldest day, not silently dropped from the warm set.
    //
    // `n == 0` is answered before the loop, not by it: the loop's bound is
    // checked AFTER a push, so with `n == 0` the first day was mapped and kept
    // and the "holds up to n mappings" contract was broken by one. Callers pass
    // a config value bounded to 1..=3650, but this is a public function.
    if n == 0 {
        return Ok(Vec::new());
    }
    if days.is_empty() {
        return Err(PreloadError::NothingToPreload);
    }

    // Capacity is bounded by the request, not by the directory: a store with
    // thousands of day files must not reserve a slot per file.
    let mut mapped = Vec::with_capacity(n.min(days.len()));
    for day in days {
        let name = format!("{}.idx", std::str::from_utf8(&day).unwrap_or("00000000"));
        // One missing or unreadable day is a SKIP, not an abort: preloading
        // warms pages and a single torn file must not cost the cache the other
        // days it could still hold. `preload_recent` used to propagate the
        // first open/mmap failure, so a bad day wiped the whole warm set.
        //
        // Preload maps only immutable (rolled-over) days, so no concurrent
        // writer extends or truncates the file for the lifetime of the
        // mapping, and `MappedFile` keeps the descriptor pinned besides: only
        // sealed days reach this loop, and a sealed day is never appended to
        // or truncated again. That is the file class `map_sealed_day` names.
        let opened = crate::ffi::map_sealed_day(root, dir, &day);
        let Ok(mapped_file) = opened else {
            // Counted, not just skipped: the next-oldest day still fills the
            // slot, but an operator can tell "fewer sealed days than I asked
            // for" from "some of them would not map".
            note_skipped();
            continue;
        };
        mapped.push(MappedIndex {
            path: dir.join(name),
            day,
            mmap: mapped_file,
        });
        if mapped.len() == n {
            break;
        }
    }
    Ok(mapped)
}

/// Preload both the date index and every modality index found under `root`.
///
/// `root` is the `idx/` directory. Returns a list of all mappings; the caller
/// keeps them alive (or drops them) to control how long the pages stay warm.
pub fn preload_all(root: &SealedIdxRoot, today: [u8; 8], n: usize) -> Vec<MappedIndex> {
    let mut out = Vec::new();
    // Date index: idx/dates/
    let dates_dir = root.dates_dir();
    if let Ok(m) = preload_recent(root, &dates_dir, today, n) {
        out.extend(m);
    }
    // Modality index: idx/modality/{CT,MR,...}/
    let modality_root = root.modality_root();
    match fs::read_dir(&modality_root) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => {
                        // An unreadable entry is a modality directory whose
                        // days are NOT warmed, which is not the same as a
                        // modality with no days.
                        note_skipped();
                        continue;
                    }
                };
                if !entry.path().is_dir() {
                    continue;
                }
                match preload_recent(root, &entry.path(), today, n) {
                    Ok(m) => out.extend(m),
                    // An unreadable modality directory used to look like an
                    // empty one: `Ok` yields nothing, and this arm is the only
                    // place the difference can be recorded.
                    Err(PreloadError::NothingToPreload) => {}
                    Err(_) => note_skipped(),
                }
            }
        }
        // The whole modality tree is unreadable rather than absent.
        Err(_) => note_skipped(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, data: &[u8]) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).expect("mkdir");
        }
        fs::write(path, data).expect("write");
    }

    /// Test-only root: a writer that has already rolled over to `current` and
    /// sealed every day in `sealed`. The tests write `.idx` files once and
    /// never append or truncate them after mapping, matching the writer's
    /// sealing contract.
    fn test_root(dir: &Path, sealed: &[[u8; 8]], current: [u8; 8]) -> SealedIdxRoot {
        SealedIdxRoot {
            root: dir.to_path_buf(),
            sealed: sealed.iter().copied().collect(),
            current_day: current,
        }
    }

    #[test]
    fn parse_day_accepts_valid_names() {
        assert_eq!(parse_day("20240115.idx"), Some(*b"20240115"));
        assert_eq!(parse_day("notes.idx"), None);
        assert_eq!(parse_day("2024111.idx"), None); // 7 digits
    }

    #[test]
    fn preload_excludes_today_and_keeps_newest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = test_root(dir.path(), &[*b"20240114", *b"20240115"], *b"20240116");
        let today = *b"20240116";
        let dates = dir.path().join("dates");
        // Older days are sealed and become warm.
        write_file(&dates.join("20240114.idx"), b"1111\n");
        write_file(&dates.join("20240115.idx"), b"2222\n");
        // Today is the writer's current day (mutable) and must be excluded.
        write_file(&dates.join("20240116.idx"), b"3333\n");

        let maps = preload_recent(&root, &dates, today, 10).expect("preload");
        assert_eq!(maps.len(), 2, "current day excluded");
        // Newest first: 15, then 14.
        assert_eq!(maps[0].day, *b"20240115");
        assert_eq!(maps[1].day, *b"20240114");
        assert_eq!(maps[0].as_slice(), b"2222\n");
    }

    #[test]
    fn preload_respects_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = test_root(
            dir.path(),
            &[*b"20240112", *b"20240113", *b"20240114", *b"20240115"],
            *b"20240116",
        );
        let today = *b"20240116";
        let dates = dir.path().join("dates");
        write_file(&dates.join("20240112.idx"), b"a\n");
        write_file(&dates.join("20240113.idx"), b"b\n");
        write_file(&dates.join("20240114.idx"), b"c\n");
        write_file(&dates.join("20240115.idx"), b"d\n");

        let maps = preload_recent(&root, &dates, today, 2).expect("preload");
        assert_eq!(maps.len(), 2);
        assert_eq!(maps[0].day, *b"20240115");
        assert_eq!(maps[1].day, *b"20240114");
    }

    /// A day that cannot be opened must be replaced by the next-oldest one,
    /// not silently turn into a short warm set.
    ///
    /// The newest day here is a DIRECTORY named `20240103.idx`: `File::open`
    /// succeeds on a directory on Windows and fails on Unix, so the test uses
    /// the mmap step to make the failure platform-independent - mapping a
    /// directory fails on both. (An empty regular file would NOT work as the
    /// fixture: `map(0)` succeeds on both platforms.)
    #[test]
    fn a_failed_day_is_backfilled_from_the_next_oldest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = test_root(
            dir.path(),
            &[*b"20240101", *b"20240102", *b"20240103"],
            *b"20240104",
        );
        let today = *b"20240104";
        let dates = dir.path().join("dates");
        write_file(&dates.join("20240101.idx"), b"one\n");
        write_file(&dates.join("20240102.idx"), b"two\n");
        std::fs::create_dir(dates.join("20240103.idx")).expect("mkdir");

        let maps = preload_recent(&root, &dates, today, 2).expect("preload");
        assert_eq!(
            maps.iter().map(|m| m.day).collect::<Vec<_>>(),
            vec![*b"20240102", *b"20240101"],
            "the unopenable newest day must free its slot for 20240101"
        );
    }

    /// `n == 0` means "map nothing". The loop's bound is checked after a push,
    /// so it could not express that: with `n == 0` the first day was pushed and
    /// the loop ran on, returning EVERY day. `preload_recent` is public and
    /// documents "holds up to n mappings".
    #[test]
    fn zero_days_requested_maps_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = test_root(
            dir.path(),
            &[*b"20240101", *b"20240102", *b"20240103"],
            *b"20240104",
        );
        let today = *b"20240104";
        write_file(&dir.path().join("20240101.idx"), b"one\n");
        write_file(&dir.path().join("20240102.idx"), b"two\n");
        write_file(&dir.path().join("20240103.idx"), b"three\n");

        let maps = preload_recent(&root, dir.path(), today, 0).expect("preload");
        assert!(
            maps.is_empty(),
            "n = 0 must map nothing, got {} mapping(s)",
            maps.len()
        );
    }

    /// A `..` component in the directory is refused BEFORE any directory is
    /// opened.
    ///
    /// The prefix check compares path COMPONENTS, not resolved paths, so
    /// `/idx/../scratch` lexically starts with `/idx` while resolving outside
    /// the sealed tree. Without the component check `map_sealed_day` would map
    /// a file the rollover writer never sealed.
    #[test]
    fn a_parent_component_escape_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = test_root(dir.path(), &[*b"20240115"], *b"20240116");
        let today = *b"20240116";
        // `dates/../scratch` resolves to `<tempdir>/scratch`, outside the
        // dates tree, while its leading components still start with the root.
        let escape = dir.path().join("dates").join("..").join("scratch");
        write_file(&escape.join("20240115.idx"), b"escape\n");

        match preload_recent(&root, &escape, today, 10) {
            Err(PreloadError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            Ok(_) => panic!("a `..` escape must be refused"),
            Err(other) => panic!("expected an invalid-input I/O error, got {other:?}"),
        }
    }

    /// A writer that has NOT yet rolled over must not have its current day
    /// mapped as sealed, even when the wall clock has advanced past it.
    ///
    /// If the writer still targets 20240115 while the clock reads 20240116,
    /// 20240115 is the writer's CURRENT day (mutable) - the old wall-clock
    /// exclusion (`today = 20240116`) would have mapped it, and a later
    /// rollover-then-append would append to a mapped file.
    #[test]
    fn a_writers_current_day_is_not_mapped_when_the_clock_is_ahead() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The writer still targets 20240115 (startup, before the first
        // rollover); its sealed set is empty.
        let set = crate::idx::rollover::DailyIndexSet::new(dir.path().to_path_buf(), *b"20240115");
        let root = SealedIdxRoot::from_writer(&set);
        // The file the writer would still append to exists on disk.
        write_file(&dir.path().join("20240115.idx"), b"2222\n");

        // The wall clock reads one day AHEAD of the writer.
        let maps = match preload_recent(&root, dir.path(), *b"20240116", 10) {
            Ok(m) => m,
            Err(PreloadError::NothingToPreload) => Vec::new(),
            Err(e) => panic!("unexpected preload error: {e:?}"),
        };
        assert!(
            maps.is_empty(),
            "the writer's current day must not be mapped as sealed"
        );
    }

    /// An unreadable sealed day is COUNTED, not silently skipped.
    ///
    /// The skip itself is correct - one bad file must not cost the cache the
    /// other days - but with no signal a day that failed to map is
    /// indistinguishable from a day that was never sealed, so a cold cache has
    /// no diagnosable cause.
    #[test]
    fn an_unmappable_sealed_day_is_counted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = test_root(dir.path(), &[*b"20240114", *b"20240115"], *b"20240116");
        let today = *b"20240116";
        let dates = dir.path().join("dates");
        write_file(&dates.join("20240115.idx"), b"2222\n");
        // A DIRECTORY where a day file is expected: it cannot be mapped.
        fs::create_dir_all(dates.join("20240114.idx")).expect("plant dir");

        let before = skipped_preloads();
        let maps = preload_recent(&root, &dates, today, 10).expect("preload");
        assert_eq!(maps.len(), 1, "the healthy day is still warmed");
        assert_eq!(maps[0].day, *b"20240115");
        assert!(
            skipped_preloads() > before,
            "the unmappable day must be reported"
        );
    }
}
