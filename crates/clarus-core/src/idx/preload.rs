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

use std::fs;
use std::path::{Path, PathBuf};

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

/// Map the most recent `n` *immutable* day files under `dir`, excluding today.
///
/// `today` is excluded (it is still mutable). Files are selected by parsing
/// each `YYYYMMDD.idx` name, walking newest-first, and mapping them read-only;
/// a day that cannot be mapped is skipped and the next-oldest takes its slot,
/// so the result holds up to `n` mappings. Returns the mappings sorted
/// newest-first.
pub fn preload_recent<P: AsRef<Path>>(
    dir: P,
    today: [u8; 8],
    n: usize,
) -> Result<Vec<MappedIndex>, PreloadError> {
    let dir = dir.as_ref();
    let mut days: Vec<[u8; 8]> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            parse_day(&name.to_string_lossy())
        })
        .filter(|d| *d != today)
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
        let path = dir.join(format!(
            "{}.idx",
            std::str::from_utf8(&day).unwrap_or("00000000")
        ));
        // One missing or unreadable day is a SKIP, not an abort: preloading
        // warms pages and a single torn file must not cost the cache the other
        // days it could still hold. `preload_recent` used to propagate the
        // first open/mmap failure, so a bad day wiped the whole warm set.
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        // Preload maps only immutable (rolled-over) days, so no concurrent
        // writer extends or truncates the file for the lifetime of the
        // mapping, and `MappedFile` keeps the descriptor pinned besides.
        let Ok(mapped_file) = crate::ffi::MappedFile::from_file(file) else {
            continue;
        };
        mapped.push(MappedIndex {
            path,
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
pub fn preload_all(head: &Path, root: &Path, today: [u8; 8], n: usize) -> Vec<MappedIndex> {
    let mut out = Vec::new();
    // Date index: idx/dates/
    let dates_dir = root.join("dates");
    if let Ok(m) = preload_recent(&dates_dir, today, n) {
        out.extend(m);
    }
    let _ = head;
    // Modality index: idx/modality/{CT,MR,...}/
    let modality_root = root.join("modality");
    if let Ok(entries) = fs::read_dir(&modality_root) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if let Ok(m) = preload_recent(entry.path(), today, n) {
                out.extend(m);
            }
        }
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

    #[test]
    fn parse_day_accepts_valid_names() {
        assert_eq!(parse_day("20240115.idx"), Some(*b"20240115"));
        assert_eq!(parse_day("notes.idx"), None);
        assert_eq!(parse_day("2024111.idx"), None); // 7 digits
    }

    #[test]
    fn preload_excludes_today_and_keeps_newest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let today = *b"20240116";
        // Older days are immutable and become warm.
        write_file(&dir.path().join("20240114.idx"), b"1111\n");
        write_file(&dir.path().join("20240115.idx"), b"2222\n");
        // Today is mutable and must be excluded.
        write_file(&dir.path().join("20240116.idx"), b"3333\n");

        let maps = preload_recent(dir.path(), today, 10).expect("preload");
        assert_eq!(maps.len(), 2, "today excluded");
        // Newest first: 15, then 14.
        assert_eq!(maps[0].day, *b"20240115");
        assert_eq!(maps[1].day, *b"20240114");
        assert_eq!(maps[0].as_slice(), b"2222\n");
    }

    #[test]
    fn preload_respects_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let today = *b"20240116";
        write_file(&dir.path().join("20240112.idx"), b"a\n");
        write_file(&dir.path().join("20240113.idx"), b"b\n");
        write_file(&dir.path().join("20240114.idx"), b"c\n");
        write_file(&dir.path().join("20240115.idx"), b"d\n");

        let maps = preload_recent(dir.path(), today, 2).expect("preload");
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
        let today = *b"20240104";
        write_file(&dir.path().join("20240101.idx"), b"one\n");
        write_file(&dir.path().join("20240102.idx"), b"two\n");
        std::fs::create_dir(dir.path().join("20240103.idx")).expect("mkdir");

        let maps = preload_recent(dir.path(), today, 2).expect("preload");
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
        let today = *b"20240104";
        write_file(&dir.path().join("20240101.idx"), b"one\n");
        write_file(&dir.path().join("20240102.idx"), b"two\n");
        write_file(&dir.path().join("20240103.idx"), b"three\n");

        let maps = preload_recent(dir.path(), today, 0).expect("preload");
        assert!(
            maps.is_empty(),
            "n = 0 must map nothing, got {} mapping(s)",
            maps.len()
        );
    }
}
