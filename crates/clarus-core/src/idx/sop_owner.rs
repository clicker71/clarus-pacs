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
    /// Set when an append failed part-way through a row.
    ///
    /// The public field is private to the type on purpose: a caller must not
    /// be able to clear it, or it could resubmit a row onto a fragment.
    poisoned: bool,
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
    // A crash or an ENOSPC mid-`write_all` leaves a tail shorter than one row.
    // The next append would then start MID-ROW, so every later row is shifted
    // and `read_segment` (which stops at the first misaligned row) silently
    // drops the rest - the SOP-conflict oracle would miss duplicates until the
    // day rolled over.
    //
    // Truncating the partial tail is safe because this is TODAY's file: the
    // mmap read path (`idx/preload.rs`) maps only rolled-over days, so no
    // consumer holds a mapping over the bytes being cut.
    repair_torn_tail(&path, ROW_LEN).map_err(SopOwnerError::Io)?;
    Ok(SopOwnerIndex {
        day: *day,
        file,
        poisoned: false,
    })
}

/// Truncate a segment's partial trailing row, if any.
///
/// `row_len` is the record size; a valid file is always a whole multiple of it.
/// Only the incomplete tail is removed - every complete row before it is kept,
/// because the file is append-only and those rows are already published.
pub(crate) fn repair_torn_tail(path: &Path, row_len: usize) -> std::io::Result<()> {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        // A file that does not exist yet has nothing to repair.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let row_len = row_len as u64;
    let aligned = len - (len % row_len);
    if aligned == len {
        return Ok(());
    }
    OpenOptions::new().write(true).open(path)?.set_len(aligned)
}

/// Truncate a variable-length, newline-terminated segment back to its last
/// complete row.
///
/// Rows here are not fixed width, so alignment says nothing: the test is
/// whether the file ends with the `\n` that terminates a row. Without it the
/// next append would fuse onto the fragment, and the two together read back as
/// one name that belongs to no study.
///
/// Only the unterminated tail goes; every complete row before it is kept.
pub(crate) fn repair_torn_newline_tail(path: &Path) -> std::io::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if len == 0 {
        return Ok(());
    }
    // Read the final byte: a well-formed file always ends with `\n`.
    let mut f = OpenOptions::new().read(true).open(path)?;
    f.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    drop(f);
    if last[0] == b'\n' {
        return Ok(());
    }
    // Walk back to the last complete row and cut everything after it.
    let mut all = Vec::new();
    OpenOptions::new()
        .read(true)
        .open(path)?
        .read_to_end(&mut all)?;
    let keep = all
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(keep as u64)
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
    ///
    /// A failed write leaves the handle POISONED. `write_all` may have written
    /// a prefix before failing (ENOSPC, a signal), so the file can end
    /// mid-record; retrying on this handle would start the next row after the
    /// fragment and misalign every row behind it, which `read_segment` then
    /// drops. The next attempt reopens instead, and `open_appender` repairs the
    /// tail first.
    pub fn append(&mut self, row: &[u8]) -> Result<(), SopOwnerError> {
        if self.poisoned {
            return Err(SopOwnerError::Io(io::Error::from(
                io::ErrorKind::BrokenPipe,
            )));
        }
        match self.file.write_all(row) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.poisoned = true;
                Err(SopOwnerError::Io(e))
            }
        }
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
        if self.poisoned {
            // A real errno, not a sentinel: the caller already reports this as
            // an I/O failure, and it must not read as a clean append.
            return Err(EPOISONED);
        }
        match self.file.write_all(row) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.poisoned = true;
                let code = e.raw_os_error().unwrap_or(0);
                // Do not run the Drop glue (it would call `free`).
                std::mem::forget(e);
                Err(code)
            }
        }
    }

    /// Whether a failed append left this handle unusable.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Replace a poisoned handle with a fresh one, repairing the torn tail.
    ///
    /// A poisoned writer refuses every append until this runs: the repair is a
    /// truncate, and doing it on a handle that may still be mid-append would
    /// race. Here the old handle is dropped first, so the truncate sees a
    /// quiescent file.
    pub fn reopen(&mut self, dir: &Path) -> Result<(), SopOwnerError> {
        let path = file_path(dir, &self.day);
        // Drop the old descriptor BEFORE truncating: on Windows a second
        // handle cannot shorten a file another handle holds open, and the
        // error would read as a corrupt index.
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(SopOwnerError::Io)?;
        repair_torn_tail(&path, ROW_LEN).map_err(SopOwnerError::Io)?;
        self.poisoned = false;
        Ok(())
    }
}

/// Errno reported by `append_warm` on an already-poisoned writer.
///
/// `EIO` rather than 0: 0 means "success" to any caller that only checks the
/// code, and a poisoned writer must never look like one.
const EPOISONED: i32 = 5;

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

/// Whether a `metadata` failure means the segment genuinely does not exist.
///
/// Only `NotFound` means "never written". Every other error (EACCES, EIO, a
/// permissions change) means the segment may EXIST and could not be inspected,
/// and reading that as "no owner" would let a conflict check accept a different
/// owner for a segment it simply failed to look at.
fn metadata_means_absent(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
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
            Err(e) => {
                if !metadata_means_absent(&e) {
                    return Err(SopOwnerError::Io(e));
                }
                return Ok(None);
            }
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

        let snap = SopOwnerSnapshot::load(dir.path(), b"20240115")
            .unwrap()
            .unwrap();
        let len1 = snap.current_len();
        assert_eq!(snap.find(b"1.2.1").unwrap(), b"study-A");

        // Append another row; file length grows; a fresh snapshot sees it.
        let mut idx = open_appender(dir.path(), b"20240115").unwrap();
        idx.append(&encode("1.2.2", "study-B")).unwrap();
        drop(idx);
        let snap2 = SopOwnerSnapshot::load(dir.path(), b"20240115")
            .unwrap()
            .unwrap();
        assert!(snap2.current_len() > len1);
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

    /// A tail shorter than one row must be cut before the first append.
    ///
    /// Without the repair the next row lands mid-row, every following row is
    /// shifted, and `read_segment` stops at the first misaligned row - the
    /// conflict oracle then silently loses every later SOP.
    #[test]
    fn a_torn_tail_is_repaired_before_the_first_append() {
        let dir = std::env::temp_dir().join(format!("sop_torn_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let day = *b"20250101";

        // Seed one whole row, then simulate a crash mid-write.
        {
            let mut idx = open_appender(&dir, &day).unwrap();
            let mut row = [b'x'; ROW_LEN];
            row[ROW_LEN - 1] = b'\n';
            idx.append(&row).unwrap();
        }
        let path = file_path(&dir, &day);
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"partial").unwrap();
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (ROW_LEN + 7) as u64,
            "fixture must start torn"
        );

        let mut idx = open_appender(&dir, &day).unwrap();
        let mut row = [b'y'; ROW_LEN];
        row[ROW_LEN - 1] = b'\n';
        idx.append(&row).unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(
            data.len(),
            2 * ROW_LEN,
            "the torn fragment must be gone and rows must stay aligned"
        );
        assert_eq!(data[ROW_LEN], b'y', "the new row starts on a row boundary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A writer that failed an append must not accept the next row.
    ///
    /// `write_all` can write a PREFIX before failing (ENOSPC). The file is then
    /// mid-record while the handle is still open. Appending again on that
    /// handle puts the next row after the fragment, and `read_segment` - which
    /// stops at the first misaligned row - silently loses everything behind it.
    /// The writer is poisoned instead, and `reopen` repairs the tail.
    #[test]
    fn a_failed_append_poisons_the_writer_until_it_reopens() {
        let dir = std::env::temp_dir().join(format!("sop_poison_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let day = *b"20250101";

        let mut idx = open_appender(&dir, &day).unwrap();
        let mut row = [b'a'; ROW_LEN];
        row[ROW_LEN - 1] = b'\n';
        idx.append(&row).unwrap();
        let path = file_path(&dir, &day);

        // Reproduce the state a failed write leaves behind: a partial row on
        // disk with the writer still believing it can append.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"partial").unwrap();
        }

        // Poison the handle the way a real failed write would.
        idx.poisoned = true;
        let mut row2 = [b'b'; ROW_LEN];
        row2[ROW_LEN - 1] = b'\n';
        assert!(
            idx.append(&row2).is_err(),
            "a poisoned writer must refuse further appends"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            ROW_LEN + 7,
            "the refused append must not have touched the file"
        );

        // Reopening repairs the fragment and the next row lands aligned.
        idx.reopen(&dir).unwrap();
        assert!(!idx.is_poisoned());
        idx.append(&row2).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 2 * ROW_LEN);
        assert_eq!(data[ROW_LEN], b'b');
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only `NotFound` means "this segment was never written".
    ///
    /// Mapping every metadata error to `Ok(None)` made an unreadable segment
    /// look absent, and the conflict check then accepted a DIFFERENT owner for
    /// a segment that existed but could not be inspected.
    /// Only `NotFound` means "this segment was never written".
    ///
    /// Mapping every metadata error to `Ok(None)` made an unreadable segment
    /// look absent, and the conflict check then accepted a DIFFERENT owner for
    /// a segment that existed but could not be inspected.
    #[test]
    fn only_not_found_means_the_segment_is_absent() {
        assert!(
            metadata_means_absent(&io::Error::from(io::ErrorKind::NotFound)),
            "NotFound is the one error that means 'never written'"
        );
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
            io::ErrorKind::InvalidData,
        ] {
            assert!(
                !metadata_means_absent(&io::Error::from(kind)),
                "{kind:?} must not read as absence"
            );
        }

        // End to end: a missing segment is still an absence.
        let dir = std::env::temp_dir().join(format!("sop_absent_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let day = *b"20250101";
        let missing = SopOwnerSnapshot::load(&dir, &day);
        assert!(
            matches!(missing, Ok(None)),
            "a never-written segment must read as absent"
        );
    }
}
