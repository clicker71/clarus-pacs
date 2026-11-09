// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Modality index (`idx/modality/{CT,MR,...}/YYYYMMDD.idx`).
//!
//! One subdirectory per modality, one `.idx` per day inside it. Each row is a
//! study UID terminated by a newline. The path shapes the directory tree the
//! way the plan specifies - a rollover renames a whole day's file across all
//! modalities at once.
//!
//! # Zero-allocation contract
//!
//! [`append`](ModalityIndex::append) writes a `study_uid\n` row directly
//! (the O_APPEND handle guarantees a single append syscall). No formatting,
//! no heap. The buffer a caller provides is opaque bytes; this writer only
//! appends the row.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The 8-byte `YYYYMMDD` day key shared by every `.idx` writer.
pub type DayKey = [u8; 8];

/// Errors produced by [`ModalityIndex`].
#[derive(Debug)]
pub enum ModalityIndexError {
    /// The modality string is empty (would create a broken path).
    EmptyModality,
    /// Underlying filesystem I/O failure.
    Io(io::Error),
}

impl std::fmt::Display for ModalityIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModalityIndexError::EmptyModality => write!(f, "modality index: empty modality"),
            ModalityIndexError::Io(e) => write!(f, "modality index I/O error: {e}"),
        }
    }
}

impl std::error::Error for ModalityIndexError {}

impl From<io::Error> for ModalityIndexError {
    fn from(e: io::Error) -> Self {
        ModalityIndexError::Io(e)
    }
}

/// Append-only writer for one modality's one-day index.
pub struct ModalityIndex {
    modality: String,
    day: DayKey,
    file: File,
}

/// Open (or create) the append-only handle for a modality/day pair.
pub fn open_appender(
    dir: &Path,
    modality: &str,
    day: &DayKey,
) -> Result<ModalityIndex, ModalityIndexError> {
    if modality.is_empty() {
        return Err(ModalityIndexError::EmptyModality);
    }
    // Guard against a modality that smuggles path separators - a modality is a
    // single CS value (PS3.3 Section C.7.3.1.1), never a path fragment. Reject '/'
    // and '\\' so an untrusted dataset cannot escape the modality tree.
    if modality.contains('/') || modality.contains('\\') {
        return Err(ModalityIndexError::EmptyModality);
    }
    // `.` and `..` carry no separator and still walk the tree: `dir/modality/..`
    // IS `dir`, so a dataset with Modality ".." wrote its index into the root -
    // and `.` merged every such instance into one directory. Neither is a CS
    // value; both are directory links.
    if modality == "." || modality == ".." {
        return Err(ModalityIndexError::EmptyModality);
    }
    let sub = dir.join("modality").join(modality);
    std::fs::create_dir_all(&sub).map_err(ModalityIndexError::Io)?;
    let path = file_path(dir, modality, day);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(ModalityIndexError::Io)?;
    Ok(ModalityIndex {
        modality: modality.to_string(),
        day: *day,
        file,
    })
}

/// Full path of a modality/day `.idx` file.
pub fn file_path(dir: &Path, modality: &str, day: &DayKey) -> PathBuf {
    dir.join("modality").join(modality).join(day_filename(day))
}

/// Build the `YYYYMMDD.idx` file name from an 8-byte day key.
fn day_filename(day: &DayKey) -> String {
    let mut s = String::with_capacity(13);
    s.push_str(std::str::from_utf8(day).unwrap_or("00000000"));
    s.push_str(".idx");
    s
}

/// Max study UID length before a row is considered oversized (PS3.5 Section 9.1
/// caps UIDs at 64 chars; we allow one extra byte for the trailing newline).
pub const MAX_ROW_LEN: usize = 65;

impl ModalityIndex {
    /// The modality this writer targets.
    pub fn modality(&self) -> &str {
        &self.modality
    }

    /// The day key this writer targets.
    pub fn day(&self) -> &DayKey {
        &self.day
    }

    /// Append a single `study_uid\n` row with one syscall.
    ///
    /// `row` is the caller-provided staging buffer (`uid` bytes + a trailing
    /// newline) - reused across calls so the hotpath allocates nothing.
    pub fn append(&mut self, row: &[u8]) -> Result<(), ModalityIndexError> {
        self.file.write_all(row).map_err(ModalityIndexError::Io)?;
        Ok(())
    }

    /// WARM append: like [`append`], but on failure it does NOT propagate the
    /// `io::Error` value.
    ///
    /// An `io::Error` owns a heap box in its custom variant, so returning it
    /// (or dropping it) emits `free` drop glue in the linked body - which the
    /// static no-alloc gate (branch-blind) rejects. This variant extracts only
    /// the `Copy` errno and forgets the error, so the body carries no
    /// allocator call in ANY branch.
    pub fn append_warm(&mut self, row: &[u8]) -> Result<(), i32> {
        match self.file.write_all(row) {
            Ok(()) => Ok(()),
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0);
                std::mem::forget(e);
                Err(code)
            }
        }
    }

    /// Number of bytes currently in the file (for rollover sizing).
    pub fn len(&self) -> io::Result<u64> {
        self.file.metadata().map(|m| m.len())
    }

    /// Whether the file is currently empty.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// Read every stored study UID from a modality/day `.idx` file.
///
/// Each row is `study_uid\n`. A torn trailing row (crash during append) is
/// dropped. Used by the shadow-read proptest to compare B-tree contents.
pub fn read_study_uids(dir: &Path, modality: &str, day: &DayKey) -> io::Result<Vec<String>> {
    let data = std::fs::read(file_path(dir, modality, day))?;
    let mut out = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        // A torn trailing row lacks its newline; only accept whole lines.
        let s = String::from_utf8_lossy(line);
        out.push(s.into_owned());
    }
    Ok(out)
}

/// Format a `study_uid\n` row into `row` (length `uid.len() + 1`).
///
/// Panics if `row` is smaller than `uid.len() + 1`; callers size it from the
/// UID. Returns the filled slice.
pub fn format_row<'a>(uid: &str, row: &'a mut [u8]) -> &'a [u8] {
    assert!(row.len() > uid.len(), "row buffer too small for study UID");
    row[..uid.len()].copy_from_slice(uid.as_bytes());
    row[uid.len()] = b'\n';
    &row[..uid.len() + 1]
}

/// WARM variant of [`format_row`] into a fixed `[u8; N]`: NO panic, NO bounds
/// check, NO allocation - every slice access is a `.get*` returning `Option`,
/// so a too-small buffer is `None` instead of a `panic_fmt` /
/// `panic_bounds_check` the static no-alloc gate would reject.
///
/// Returns the filled `study_uid\n` prefix, or `None` when it does not fit.
pub fn format_row_warm<'a, const N: usize>(uid: &str, row: &'a mut [u8; N]) -> Option<&'a [u8]> {
    let n = uid.len();
    // `n + 1` must fit; `checked_add` keeps the bound arithmetic panic-free.
    let end = n.checked_add(1)?;
    let dst = row.get_mut(..n)?;
    dst.copy_from_slice(uid.as_bytes());
    *row.get_mut(n)? = b'\n';
    row.get(..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_rejects_empty_modality() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(open_appender(dir.path(), "", b"20240115").is_err());
    }

    #[test]
    fn open_rejects_path_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(open_appender(dir.path(), "../evil", b"20240115").is_err());
    }

    /// `.` and `..` contain no separator and still walk the tree: `dir/modality/..`
    /// IS `dir`. A dataset with Modality `..` wrote its index outside the
    /// modality subtree, and `.` merged every such instance into one directory.
    #[test]
    fn open_rejects_dot_and_dot_dot_modality() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(open_appender(dir.path(), "..", b"20240115").is_err());
        assert!(open_appender(dir.path(), ".", b"20240115").is_err());
        // A modality that merely CONTAINS a dot is a legal CS value.
        assert!(open_appender(dir.path(), "CT.1", b"20240115").is_ok());
    }

    #[test]
    fn append_writes_study_uid_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = *b"20240115";
        let mut idx = open_appender(dir.path(), "CT", &day).expect("open");
        let mut row = [0u8; 128];
        let uid = "1.2.840.1.2.3";
        idx.append(format_row(uid, &mut row)).expect("append");

        let data = std::fs::read(file_path(dir.path(), "CT", &day)).expect("read");
        assert_eq!(data, format!("{uid}\n").as_bytes());
    }

    #[test]
    fn distinct_modalities_get_distinct_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = *b"20240115";
        let mut ct = open_appender(dir.path(), "CT", &day).expect("ct");
        let mut mr = open_appender(dir.path(), "MR", &day).expect("mr");
        let mut row = [0u8; 128];
        ct.append(format_row("uid-ct", &mut row)).expect("ct a");
        mr.append(format_row("uid-mr", &mut row)).expect("mr a");

        let ct_data = std::fs::read(file_path(dir.path(), "CT", &day)).expect("read ct");
        let mr_data = std::fs::read(file_path(dir.path(), "MR", &day)).expect("read mr");
        assert_eq!(ct_data, b"uid-ct\n");
        assert_eq!(mr_data, b"uid-mr\n");
    }
}
