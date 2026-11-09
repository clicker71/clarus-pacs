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

use memmap2::Mmap;

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
    /// The mapped bytes. Read-only; the underlying file must never be
    /// truncated or replaced while a mapping is alive.
    mmap: Mmap,
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
/// each `YYYYMMDD.idx` name, keeping the newest `n`, and mapping them
/// read-only. Returns the mappings sorted newest-first.
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
    days.truncate(n);
    if days.is_empty() {
        return Err(PreloadError::NothingToPreload);
    }

    let mut mapped = Vec::with_capacity(days.len());
    for day in days {
        let path = dir.join(format!(
            "{}.idx",
            std::str::from_utf8(&day).unwrap_or("00000000")
        ));
        let file = fs::File::open(&path).map_err(PreloadError::Io)?;
        // The mmap itself lives in the audited OS boundary (crate::ffi).
        // Preload maps only immutable (rolled-over) days, so no concurrent
        // writer extends or truncates the file for the lifetime of the
        // mapping; `file` stays alive for the mapping's scope.
        let mmap = crate::ffi::map_readonly(&file).map_err(PreloadError::Io)?;
        mapped.push(MappedIndex { path, day, mmap });
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
}
