// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Series-owner index (`idx/series_owner/YYYYMMDD.idx`) - the Weasis
//! series-root QIDO oracle (1611 #14).
//!
//! Maps `SeriesInstanceUID -> StudyInstanceUID` so the series-root instances
//! route (`GET /dicomweb/series/{SeriesInstanceUID}/instances`) resolves the
//! owning study without decoding every manifest on the request thread. The
//! layout and the read machinery mirror `idx/sop_owner.rs` (fixed-size rows,
//! sorted in-memory snapshot, day-segmented files); only the directory name
//! and the row semantics differ. The write side lives in `shadow.rs`
//! (`dual_write`), which appends one row per accepted STOW.
//!
//! # Record format (fixed 129 bytes)
//!
//! ```text
//! Series (64 bytes, space-padded) | Study (64 bytes, space-padded) | '\n'
//! ```
//!
//! The Series field is the fixed-width search key. Rows within a day are
//! appended in arrival order; the reader sorts its snapshot before binary
//! search, exactly as `sop_owner` does.

#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Row width: Series (64) + Study (64) + '\n' (1).
pub const ROW_LEN: usize = 129;
/// UID field width (PS3.5 caps UIDs at 64 chars).
pub const UID_LEN: usize = 64;
/// The 8-byte `YYYYMMDD` day key shared by every `.idx` writer.
pub type DayKey = [u8; 8];

/// Errors produced by [`SeriesOwnerIndex`].
#[derive(Debug)]
pub enum SeriesOwnerError {
    /// A UID exceeds the fixed field width.
    UidTooLong,
    /// Underlying filesystem I/O failure.
    Io(io::Error),
}

impl std::fmt::Display for SeriesOwnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeriesOwnerError::UidTooLong => write!(f, "series_owner: UID exceeds 64 chars"),
            SeriesOwnerError::Io(e) => write!(f, "series_owner I/O error: {e}"),
        }
    }
}

impl std::error::Error for SeriesOwnerError {}

impl From<io::Error> for SeriesOwnerError {
    fn from(e: io::Error) -> Self {
        SeriesOwnerError::Io(e)
    }
}

/// Encode one `(series, study)` pair into a fixed 129-byte row.
///
/// The Series occupies the leading 64 bytes so the row sorts by the search
/// key. Returns `Err` if either UID exceeds [`UID_LEN`].
pub fn encode_row<'a>(
    series: &[u8],
    study: &[u8],
    row: &'a mut [u8; ROW_LEN],
) -> Result<&'a [u8], SeriesOwnerError> {
    if series.len() > UID_LEN || study.len() > UID_LEN {
        return Err(SeriesOwnerError::UidTooLong);
    }
    let mut i = 0;
    for &b in series {
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

/// Append-only writer for one day's `series_owner` segment.
pub struct SeriesOwnerIndex {
    file: File,
    /// Set when an append failed part-way through a row (same poison policy
    /// as `sop_owner`: retrying on this handle would start mid-record).
    poisoned: bool,
}

/// Open (or create) the append-only handle for a day.
pub fn open_appender(dir: &Path, day: &DayKey) -> Result<SeriesOwnerIndex, SeriesOwnerError> {
    let sub = dir.join("series_owner");
    std::fs::create_dir_all(&sub).map_err(SeriesOwnerError::Io)?;
    let path = file_path(dir, day);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(SeriesOwnerError::Io)?;
    // Same torn-tail repair as `sop_owner`: a crash mid-`write_all` leaves a
    // partial trailing row that would misalign every later row.
    repair_torn_tail(&path, ROW_LEN).map_err(SeriesOwnerError::Io)?;
    Ok(SeriesOwnerIndex {
        file,
        poisoned: false,
    })
}

/// Truncate a segment's partial trailing row, if any (copy of the `sop_owner`
/// policy: keep every complete row, cut only the incomplete tail).
fn repair_torn_tail(path: &Path, row_len: usize) -> std::io::Result<()> {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
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

/// Full path of a day's `series_owner.idx` file.
pub fn file_path(dir: &Path, day: &DayKey) -> PathBuf {
    dir.join("series_owner").join(day_filename(day))
}

/// Build `YYYYMMDD.idx` from an 8-byte day key.
fn day_filename(day: &DayKey) -> String {
    let mut s = String::with_capacity(13);
    s.push_str(std::str::from_utf8(day).unwrap_or("00000000"));
    s.push_str(".idx");
    s
}

impl SeriesOwnerIndex {
    /// Append one `(series, study)` row with a single write.
    pub fn append(&mut self, row: &[u8]) -> Result<(), SeriesOwnerError> {
        if self.poisoned {
            return Err(SeriesOwnerError::Io(io::Error::from(
                io::ErrorKind::BrokenPipe,
            )));
        }
        match self.file.write_all(row) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.poisoned = true;
                Err(SeriesOwnerError::Io(e))
            }
        }
    }

    /// Encode + append in one call.
    pub fn append_pair(&mut self, series: &[u8], study: &[u8]) -> Result<(), SeriesOwnerError> {
        let mut row = [0u8; ROW_LEN];
        let encoded = encode_row(series, study, &mut row)?;
        self.append(encoded)
    }

    /// Whether a failed append left this handle unusable.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

/// Read a fixed-width `(series, study)` pair from a 129-byte row.
fn decode_row(row: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if row.len() != ROW_LEN || row[ROW_LEN - 1] != b'\n' {
        return None;
    }
    let series = trim_pad(&row[..UID_LEN]);
    let study = trim_pad(&row[UID_LEN..UID_LEN * 2]);
    Some((series, study))
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
            break;
        }
        if let Some(pair) = decode_row(row) {
            rows.push(pair);
        }
        off += ROW_LEN;
    }
    Ok(rows)
}

/// An in-memory, sorted snapshot of one day's `series_owner` segment.
///
/// Memoization unit for the lookup: loaded and sorted once, then
/// binary-searched without re-reading the file.
pub struct SeriesOwnerSnapshot {
    /// Segment file length at snapshot time (bytes).
    pub file_len: u64,
    /// Rows sorted by Series (in-memory copy).
    rows: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Whether a `metadata` failure means the segment genuinely does not exist
/// (every other error means "may exist and could not be inspected").
fn metadata_means_absent(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

impl SeriesOwnerSnapshot {
    /// Load + sort a day segment into a memoizable snapshot.
    ///
    /// Returns `Ok(None)` when no segment exists for `day`.
    pub fn load(dir: &Path, day: &DayKey) -> Result<Option<SeriesOwnerSnapshot>, SeriesOwnerError> {
        let path = file_path(dir, day);
        let file_len = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) => {
                if !metadata_means_absent(&e) {
                    return Err(SeriesOwnerError::Io(e));
                }
                return Ok(None);
            }
        };
        let mut rows = read_segment(&path).map_err(SeriesOwnerError::Io)?;
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Some(SeriesOwnerSnapshot { file_len, rows }))
    }

    /// The current file length of the day segment (for change detection).
    pub fn current_len(&self) -> u64 {
        self.file_len
    }

    /// Fold one appended row into the in-memory snapshot (B1, mirror of the
    /// `sop_owner` writer: inserted in sorted position, length advances by
    /// exactly [`ROW_LEN`]).
    pub fn append_row(&mut self, series: &[u8], study: &[u8]) {
        let pos = self
            .rows
            .binary_search_by(|r| r.0.as_slice().cmp(series))
            .unwrap_or_else(|i| i);
        self.rows.insert(pos, (series.to_vec(), study.to_vec()));
        self.file_len += ROW_LEN as u64;
    }

    /// Binary-search the snapshot for the study owner of `series`.
    pub fn find(&self, series: &[u8]) -> Option<&[u8]> {
        let (mut lo, mut hi) = (0usize, self.rows.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.rows[mid].0.as_slice().cmp(series) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.rows[mid].1.as_slice()),
            }
        }
        None
    }

    /// WARM variant of [`find`]: NO panic, NO bounds check, NO allocation.
    ///
    /// Every access is `.get()` returning `Option`, so the linked body has no
    /// `panic_bounds_check`. Used by the statically gated warm lookup
    /// (`ShadowTier::series_owner_study_warm`).
    pub fn find_warm(&self, series: &[u8]) -> Option<&[u8]> {
        let (mut lo, mut hi) = (0usize, self.rows.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row = self.rows.get(mid)?;
            match row.0.as_slice().cmp(series) {
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

    #[test]
    fn round_trip_and_snapshot_find() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut idx = open_appender(dir.path(), b"20240115").expect("open");
        idx.append_pair(b"1.2.5", b"study-5").expect("a");
        idx.append_pair(b"1.2.1", b"study-1").expect("b");
        idx.append_pair(b"1.2.2", b"study-2").expect("c");
        drop(idx);

        let snap = SeriesOwnerSnapshot::load(dir.path(), b"20240115")
            .expect("load")
            .expect("segment exists");
        assert_eq!(snap.find(b"1.2.2").unwrap(), b"study-2");
        assert_eq!(snap.find(b"1.2.1").unwrap(), b"study-1");
        assert_eq!(snap.find_warm(b"1.2.5").unwrap(), b"study-5");
        assert!(snap.find(b"1.2.3").is_none());
        assert!(snap.find_warm(b"1.2.9").is_none());
    }
}
