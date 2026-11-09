// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! SOP-owner index (`idx/sop_owner/YYYYMMDD.idx`) - the 2.3 conflict
//! oracle (A1).
//!
//! Maps `SOPInstanceUID -> StudyInstanceUID` so the STOW duplicate/conflict
//! decision (409 / reason 272) survives the SQLite excision. Each row is a
//! FIXED-size record - the family contract in
//! `idx/mod.rs` ("fixed-size record sorted ... enabling binary search").
//!
//! # Record format (fixed 129 bytes)
//!
//! ```text
//! SOP  (64 bytes, space-padded) | Study (64 bytes, space-padded) | '\n'
//! ```
//!
//! The SOP field is the fixed-width search key. Records within a day are
//! appended in arrival order (concurrent STOW finalizes interleave), so the
//! segment is NOT globally sorted on disk; the reader sorts its in-memory
//! snapshot before binary search. A day is the immutable
//! segment unit (consistent with `dates.idx` / `modality.idx` and the midnight
//! rollover in `idx/rollover.rs`).
//!
//! # Degradation
//!
//! An incompletely-written tail segment degrades to a lookup miss (the torn
//! tail is skipped), exactly the `dates.idx` class - a missed conflict is
//! acceptable and healed by a rebuild.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Row width: SOP (64) + Study (64) + '\n' (1).
pub const ROW_LEN: usize = 129;
/// UID field width (PS3.5 caps UIDs at 64 chars).
pub const UID_LEN: usize = 64;
/// The 8-byte `YYYYMMDD` day key shared by every `.idx` writer.
pub type DayKey = [u8; 8];

/// Errors produced by [`SopOwnerIndex`].
#[derive(Debug)]
pub enum SopOwnerError {
    /// A UID exceeds the fixed field width.
    UidTooLong,
    /// Underlying filesystem I/O failure.
    Io(io::Error),
}

impl std::fmt::Display for SopOwnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SopOwnerError::UidTooLong => write!(f, "sop_owner: UID exceeds 64 chars"),
            SopOwnerError::Io(e) => write!(f, "sop_owner I/O error: {e}"),
        }
    }
}

impl std::error::Error for SopOwnerError {}

impl From<io::Error> for SopOwnerError {
    fn from(e: io::Error) -> Self {
        SopOwnerError::Io(e)
    }
}

/// Encode one `(sop, study)` pair into a fixed 129-byte row.
///
/// UIDs are space-padded to [`UID_LEN`]; the SOP occupies the leading 64 bytes
/// so the row sorts by SOP (the search key). Returns `Err` if either UID
/// exceeds 64 chars.
pub fn encode_row<'a>(
    sop: &[u8],
    study: &[u8],
    row: &'a mut [u8; ROW_LEN],
) -> Result<&'a [u8], SopOwnerError> {
    if sop.len() > UID_LEN || study.len() > UID_LEN {
        return Err(SopOwnerError::UidTooLong);
    }
    let mut i = 0;
    for &b in sop {
        row[i] = b;
        i += 1;
    }
    while i < UID_LEN {
        row[i] = b' ';
        i += 1;
    }
    for &b in study {
        row[i] = b;
        i += 1;
    }
    while i < UID_LEN * 2 {
        row[i] = b' ';
        i += 1;
    }
    row[ROW_LEN - 1] = b'\n';
    Ok(&row[..ROW_LEN])
}

/// WARM variant of [`encode_row`]: NO panic, NO bounds check, NO allocation.
///
/// Fills the whole `[u8; ROW_LEN]` space-padded row via `copy_from_slice` and
/// `iter_mut` (never `row[i]`), so the linked body has no
/// `panic_bounds_check`. Returns `Err(UidTooLong)` when a UID exceeds
/// [`UID_LEN`]; the row is then unspecified.
pub fn encode_row_warm<'a>(
    sop: &[u8],
    study: &[u8],
    row: &'a mut [u8; ROW_LEN],
) -> Result<&'a [u8], SopOwnerError> {
    if sop.len() > UID_LEN || study.len() > UID_LEN {
        return Err(SopOwnerError::UidTooLong);
    }
    // Space-fill first (one pass, no indexing), then overwrite the two UIDs.
    for slot in row.iter_mut() {
        *slot = b' ';
    }
    let sop_dst = row.get_mut(..sop.len()).ok_or(SopOwnerError::UidTooLong)?;
    sop_dst.copy_from_slice(sop);
    let study_dst = row
        .get_mut(UID_LEN..UID_LEN + study.len())
        .ok_or(SopOwnerError::UidTooLong)?;
    study_dst.copy_from_slice(study);
    // Trailing newline terminates the last row.
    *row.last_mut().ok_or(SopOwnerError::UidTooLong)? = b'\n';
    Ok(&row[..])
}

/// Append-only writer for one day's `sop_owner` segment.
pub struct SopOwnerIndex {
    day: DayKey,
    file: File,
}

/// Open (or create) the append-only handle for a day.
pub fn open_appender(dir: &Path, day: &DayKey) -> Result<SopOwnerIndex, SopOwnerError> {
    let sub = dir.join("sop_owner");
    std::fs::create_dir_all(&sub).map_err(SopOwnerError::Io)?;
    let path = file_path(dir, day);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(SopOwnerError::Io)?;
    Ok(SopOwnerIndex { day: *day, file })
}

/// Full path of a day's `sop_owner.idx` file.
pub fn file_path(dir: &Path, day: &DayKey) -> PathBuf {
    dir.join("sop_owner").join(day_filename(day))
}

/// Build `YYYYMMDD.idx` from an 8-byte day key.
fn day_filename(day: &DayKey) -> String {
    let mut s = String::with_capacity(13);
    s.push_str(std::str::from_utf8(day).unwrap_or("00000000"));
    s.push_str(".idx");
    s
}

impl SopOwnerIndex {
    /// The day this writer targets.
    pub fn day(&self) -> &DayKey {
        &self.day
    }

    /// Append one `(sop, study)` row with a single write.
    ///
    /// Rows are appended in arrival order: a day segment is NOT globally
    /// sorted on disk. Lookups sort their read snapshot before binary search
    /// (`lookup_owner`).
    pub fn append(&mut self, row: &[u8]) -> Result<(), SopOwnerError> {
        self.file.write_all(row).map_err(SopOwnerError::Io)?;
        Ok(())
    }

    /// WARM append: like [`append`], but on failure it does NOT propagate the
    /// `io::Error` value.
    ///
    /// An `io::Error` owns a heap box in its custom variant, so returning it
    /// (or dropping it) emits `free` drop glue in the linked body - which the
    /// static no-alloc gate (branch-blind) rejects. This variant extracts only
    /// the `Copy` errno and forgets the error, so the body carries no
    /// allocator call in ANY branch. The caller maps the tag to a message.
    pub fn append_warm(&mut self, row: &[u8]) -> Result<(), i32> {
        match self.file.write_all(row) {
            Ok(()) => Ok(()),
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0);
                // Do not run the Drop glue (it would call `free`).
                std::mem::forget(e);
                Err(code)
            }
        }
    }
}

/// Read a fixed-width `(sop, study)` pair from a 129-byte row.
///
/// Trims trailing NUL/space padding. Returns `None` for a malformed/torn tail
/// row (not a full [`ROW_LEN`], or lacking the trailing newline).
fn decode_row(row: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if row.len() != ROW_LEN || row[ROW_LEN - 1] != b'\n' {
        return None;
    }
    let sop = trim_pad(&row[..UID_LEN]);
    let study = trim_pad(&row[UID_LEN..UID_LEN * 2]);
    Some((sop, study))
}

/// Trim trailing ASCII spaces (the field pad) and any NULs.
fn trim_pad(field: &[u8]) -> Vec<u8> {
    let mut end = field.len();
    while end > 0 && (field[end - 1] == b' ' || field[end - 1] == 0) {
        end -= 1;
    }
    field[..end].to_vec()
}

/// Read an immutable day segment's rows, skipping a torn tail row.
fn read_segment(path: &Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let mut rows = Vec::new();
    let mut off = 0;
    while off + ROW_LEN <= buf.len() {
        let row = &buf[off..off + ROW_LEN];
        if row[ROW_LEN - 1] != b'\n' {
            // Torn tail row - stop (rest is garbage).
            break;
        }
        if let Some(pair) = decode_row(row) {
            rows.push(pair);
        }
        off += ROW_LEN;
    }
    Ok(rows)
}

/// An in-memory, sorted snapshot of one day's `sop_owner` segment.
///
/// Memoization unit for the hotpath: the snapshot is loaded and
/// sorted ONCE, then binary-searched without re-reading the file. The caller
/// re-loads only when the segment's file length has grown (new appends land
/// at the tail), keyed by `(day, file_len)`.
pub struct SopOwnerSnapshot {
    /// Segment file length at snapshot time (bytes).
    pub file_len: u64,
    /// Rows sorted by SOP (in-memory copy).
    rows: Vec<(Vec<u8>, Vec<u8>)>,
}

impl SopOwnerSnapshot {
    /// Load + sort a day segment into a memoizable snapshot.
    ///
    /// Returns `Ok(None)` when no segment exists for `day` (never written, or
    /// empty) - the `dates.idx`-class miss.
    pub fn load(dir: &Path, day: &DayKey) -> Result<Option<SopOwnerSnapshot>, SopOwnerError> {
        let path = file_path(dir, day);
        let file_len = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => return Ok(None),
        };
        let mut rows = read_segment(&path).map_err(SopOwnerError::Io)?;
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Some(SopOwnerSnapshot { file_len, rows }))
    }

    /// The current file length of the day segment (for change detection).
    pub fn current_len(&self) -> u64 {
        self.file_len
    }

    /// B1: fold one appended row into the in-memory snapshot.
    ///
    /// The index writer calls this under its mutex right after appending the
    /// row to disk, so later lookups in the same process never re-read the
    /// file for this process's own appends. The row is inserted in sorted
    /// position (the disk segment is arrival-ordered; the snapshot is the
    /// sorted copy) and `file_len` advances exactly [`ROW_LEN`].
    pub fn append_row(&mut self, sop: &[u8], study: &[u8]) {
        let pos = self
            .rows
            .binary_search_by(|r| r.0.as_slice().cmp(sop))
            .unwrap_or_else(|i| i);
        self.rows.insert(pos, (sop.to_vec(), study.to_vec()));
        self.file_len += ROW_LEN as u64;
    }

    /// Binary-search the snapshot for the study owner of `sop`.
    /// Returns the owner study UID bytes, or `None` when absent.
    pub fn find(&self, sop: &[u8]) -> Option<&[u8]> {
        let (mut lo, mut hi) = (0usize, self.rows.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.rows[mid].0.as_slice().cmp(sop) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.rows[mid].1.as_slice()),
            }
        }
        None
    }

    /// WARM variant of [`find`]: NO panic, NO bounds check, NO allocation.
    ///
    /// Identical binary search, but every access is `.get()` returning
    /// `Option`, so the linked body has no `panic_bounds_check`. Used by the
    /// statically gated warm lookup.
    pub fn find_warm(&self, sop: &[u8]) -> Option<&[u8]> {
        let (mut lo, mut hi) = (0usize, self.rows.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row = self.rows.get(mid)?;
            match row.0.as_slice().cmp(sop) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(row.1.as_slice()),
            }
        }
        None
    }
}

/// Compute the current byte length of a day's `sop_owner` segment, for the
/// memoization change check. `None` when the segment does not exist.
pub fn segment_len(dir: &Path, day: &DayKey) -> Option<u64> {
    std::fs::metadata(file_path(dir, day)).ok().map(|m| m.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(sop: &str, study: &str) -> [u8; ROW_LEN] {
        let mut row = [0u8; ROW_LEN];
        encode_row(sop.as_bytes(), study.as_bytes(), &mut row).unwrap();
        row
    }

    #[test]
    fn round_trip() {
        let row = encode("1.2.3.4", "9.8.7");
        let (sop, study) = decode_row(&row).unwrap();
        assert_eq!(sop, b"1.2.3.4");
        assert_eq!(study, b"9.8.7");
    }

    #[test]
    fn uid_too_long() {
        let mut row = [0u8; ROW_LEN];
        let long = b"x".repeat(65);
        assert!(encode_row(&long, b"1", &mut row).is_err());
        assert!(encode_row(b"1", &long, &mut row).is_err());
    }

    #[test]
    fn snapshot_finds_and_misses() {
        let dir = tempfile::tempdir().unwrap();
        // Rows appended in UNSORTED arrival order - the snapshot sorts.
        let mut idx = open_appender(dir.path(), b"20240115").unwrap();
        idx.append(&encode("1.2.5", "study-5")).unwrap();
        idx.append(&encode("1.2.1", "study-1")).unwrap();
        idx.append(&encode("1.2.2", "study-2")).unwrap();
        drop(idx);

        let snap = SopOwnerSnapshot::load(dir.path(), b"20240115")
            .unwrap()
            .expect("segment exists");
        assert_eq!(snap.find(b"1.2.2").unwrap(), b"study-2");
        assert_eq!(snap.find(b"1.2.1").unwrap(), b"study-1");
        assert_eq!(snap.find(b"1.2.5").unwrap(), b"study-5");
        assert!(snap.find(b"1.2.3").is_none());
        assert!(snap.find(b"1.2.0").is_none());
        assert!(snap.find(b"1.2.6").is_none());
    }

    #[test]
    fn snapshot_across_days_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut idx = open_appender(dir.path(), b"20240115").unwrap();
            idx.append(&encode("1.2.1", "study-A")).unwrap();
            idx.append(&encode("1.2.2", "study-B")).unwrap();
            idx.append(&encode("1.2.3", "study-X")).unwrap();
        }
        {
            // Day 2 re-owns 1.2.3 under study-C.
            let mut idx = open_appender(dir.path(), b"20240116").unwrap();
            idx.append(&encode("1.2.3", "study-C")).unwrap();
        }
        let day1: DayKey = *b"20240115";
        let day2: DayKey = *b"20240116";

        let s1 = SopOwnerSnapshot::load(dir.path(), &day1).unwrap().unwrap();
        let s2 = SopOwnerSnapshot::load(dir.path(), &day2).unwrap().unwrap();

        // Most-recent day wins (day2) for the re-owned SOP.
        assert_eq!(s2.find(b"1.2.3").unwrap(), b"study-C");
        // Day2 does not have the other SOPs; day1 does.
        assert!(s2.find(b"1.2.1").is_none());
        assert_eq!(s1.find(b"1.2.1").unwrap(), b"study-A");
        assert!(s1.find(b"1.2.3").unwrap() != b"smoke"); // exists in day1 under study-X
        // Fresh SOP is absent in both.
        assert!(s1.find(b"9.9.9").is_none());
        assert!(s2.find(b"9.9.9").is_none());
    }

    #[test]
    fn snapshot_memoizes_by_file_len() {
        let dir = tempfile::tempdir().unwrap();
        let snap0 = SopOwnerSnapshot::load(dir.path(), b"20240115").unwrap();
        assert!(snap0.is_none(), "no segment yet -> None");

        let mut idx = open_appender(dir.path(), b"20240115").unwrap();
        idx.append(&encode("1.2.1", "study-A")).unwrap();
        drop(idx);

        let len1 = segment_len(dir.path(), b"20240115").unwrap();
        let snap = SopOwnerSnapshot::load(dir.path(), b"20240115")
            .unwrap()
            .unwrap();
        assert_eq!(snap.current_len(), len1);
        assert_eq!(snap.find(b"1.2.1").unwrap(), b"study-A");

        // Append another row; file length grows; a fresh snapshot sees it.
        let mut idx = open_appender(dir.path(), b"20240115").unwrap();
        idx.append(&encode("1.2.2", "study-B")).unwrap();
        drop(idx);
        assert!(segment_len(dir.path(), b"20240115").unwrap() > len1);
        let snap2 = SopOwnerSnapshot::load(dir.path(), b"20240115")
            .unwrap()
            .unwrap();
        assert_eq!(snap2.find(b"1.2.2").unwrap(), b"study-B");
    }

    /// The hot-path snapshot struct (binary-searched on the
    /// ingest decision path) is cache-line clean. Gate lives here because
    /// `rows` is private (offset_of! requires visibility at the invocation
    /// site).
    #[test]
    fn snapshot_fields_do_not_cross_cache_lines() {
        for span in ferrite_core::cache_line_fields!(
            SopOwnerSnapshot,
            64,
            file_len: u64,
            rows: Vec<(Vec<u8>, Vec<u8>)>,
        ) {
            assert!(
                !span.crosses_line(),
                "SopOwnerSnapshot.{} spans two cache lines",
                span.name
            );
        }
    }
}
