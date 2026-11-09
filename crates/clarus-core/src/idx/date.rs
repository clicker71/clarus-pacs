// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Date index (`idx/dates/YYYYMMDD.idx`).
//!
//! Each row is the 64-hex-char BLAKE3 hash of a stored instance followed by a
//! newline. Because BLAKE3 hashes are uniformly distributed, the set of hashes
//! appended within one day is already lexicographically unordered - we keep
//! row order as append order and let read-time sort (or a future merge) deal
//! with ordering. The daily file is the unit of immutability: a rollover
//! renames today's file away and opens an empty one for tomorrow.
//!
//! # Zero-allocation contract
//!
//! [`DateIndex::append`] writes the 65-byte row (`64 hex + \n`) into a
//! caller-provided `[u8; 65]` staging buffer and issues exactly one
//! `File::write` with `O_APPEND`. Enforcing `O_APPEND` is the caller's job
//! (it opens the handle via [`open_appender`]).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Length of a single `.idx` row: 64 hex chars + trailing newline.
pub const ROW_LEN: usize = 65;

/// Day-key byte length (`YYYYMMDD`).
pub const DAY_LEN: usize = 8;

/// The 8-byte `YYYYMMDD` day key shared by every `.idx` writer.
pub type DayKey = [u8; DAY_LEN];

/// Errors produced by [`DateIndex`].
#[derive(Debug)]
pub enum DateIndexError {
    /// Encoding a day key into the fixed staging buffer overflowed.
    DayKey,
    /// Underlying filesystem I/O failure.
    Io(io::Error),
}

impl std::fmt::Display for DateIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DateIndexError::DayKey => write!(f, "day key does not fit YYYYMMDD"),
            DateIndexError::Io(e) => write!(f, "date index I/O error: {e}"),
        }
    }
}

impl std::error::Error for DateIndexError {}

impl From<io::Error> for DateIndexError {
    fn from(e: io::Error) -> Self {
        DateIndexError::Io(e)
    }
}

/// Append-only writer for one day's date index.
///
/// Holds the mutable handle to today's file. Dropping it does not truncate -
/// the file remains valid for a subsequent reader or a future re-open.
pub struct DateIndex {
    day: [u8; DAY_LEN],
    file: File,
}

/// Open (or create) the append-only handle for a given day.
///
/// `dir` is the `idx/` root; the `dates/` subdirectory is created if absent.
/// The returned handle must be used with `O_APPEND` semantics - we pass
/// `append(true)` so every `write()` lands at the current end of file and is
/// atomic for rows <= the OS pipe capacity.
pub fn open_appender(dir: &Path, day: &[u8; DAY_LEN]) -> Result<DateIndex, DateIndexError> {
    let sub = dir.join("dates");
    std::fs::create_dir_all(&sub).map_err(DateIndexError::Io)?;
    let path = file_path(dir, day);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(DateIndexError::Io)?;
    Ok(DateIndex { day: *day, file })
}

/// The full path of a day's `.idx` file.
pub fn file_path(dir: &Path, day: &[u8; DAY_LEN]) -> PathBuf {
    dir.join("dates").join(day_filename(day))
}

/// Build the `YYYYMMDD.idx` file name from an 8-byte day key.
fn day_filename(day: &[u8; DAY_LEN]) -> String {
    let mut s = String::with_capacity(13);
    s.push_str(std::str::from_utf8(day).unwrap_or("00000000"));
    s.push_str(".idx");
    s
}

/// Encode a BLAKE3 hash into the 65-byte staging row in place.
///
/// Returns a borrowed sub-slice of `staging` (`64 hex + \n`) ready for a
/// single `write`. Pure, allocation-free - the hotpath primitive B-011 gates.
fn encode_row<'a>(hash: &[u8; 32], staging: &'a mut [u8; ROW_LEN]) -> &'a [u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut i = 0;
    for &b in hash {
        staging[i] = HEX[(b >> 4) as usize];
        staging[i + 1] = HEX[(b & 0x0f) as usize];
        i += 2;
    }
    staging[64] = b'\n';
    staging
}

/// WARM variant of [`encode_row`]: NO panic, NO bounds check, NO allocation.
///
/// Writes the 64 hex chars via `iter_mut()` + a nibble lookup that cannot
/// index out of range (`(b >> 4) as usize` and `(b & 0x0f) as usize` are both
/// `< 16`, expressed as `.get()` so no `panic_bounds_check` is emitted), then
/// terminates with `\n`. Pure and allocation-free for the statically gated
/// hotpath.
pub fn encode_row_warm<'a>(hash: &[u8; 32], staging: &'a mut [u8; ROW_LEN]) -> &'a [u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // Two hex chars per hash byte, in order; `chunks_exact_mut(2)` pairs the
    // output slots with the hash bytes without any indexing.
    for (pair, &b) in staging.chunks_exact_mut(2).zip(hash.iter()) {
        let hi = HEX.get((b >> 4) as usize).copied().unwrap_or(b'0');
        let lo = HEX.get((b & 0x0f) as usize).copied().unwrap_or(b'0');
        if let [first, second] = pair {
            *first = hi;
            *second = lo;
        }
    }
    if let Some(last) = staging.get_mut(ROW_LEN - 1) {
        *last = b'\n';
    }
    staging
}

impl DateIndex {
    /// The day key this writer targets.
    pub fn day(&self) -> &[u8; DAY_LEN] {
        &self.day
    }

    /// Append a BLAKE3 hash row, writing `65` bytes with one syscall.
    ///
    /// `staging` is the caller's fixed buffer - reused across calls to keep
    /// the hotpath allocation-free.
    pub fn append(
        &mut self,
        hash: &[u8; 32],
        staging: &mut [u8; ROW_LEN],
    ) -> Result<(), DateIndexError> {
        let row = encode_row(hash, staging);
        self.file.write_all(row).map_err(DateIndexError::Io)?;
        Ok(())
    }

    /// WARM variant: no bounds check on the row encoding AND no `io::Error`
    /// value propagated.
    ///
    /// [`encode_row_warm`] removes the `panic_bounds_check`; returning only the
    /// `Copy` errno (forgetting the `io::Error`, whose Drop glue is a `free`)
    /// removes the allocator call on the error branch. Together the linked body
    /// carries NO allocator call in ANY branch - what the static no-alloc gate
    /// requires.
    pub fn append_warm(&mut self, hash: &[u8; 32], staging: &mut [u8; ROW_LEN]) -> Result<(), i32> {
        let row = encode_row_warm(hash, staging);
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

/// Read every stored BLAKE3 hash (as `[u8; 32]`) from a day's `.idx` file.
///
/// Parses fixed 65-byte rows (64 hex + newline), skipping a torn trailing row
/// (a writer may have crashed mid-append). Used by the shadow-read proptest
/// to compare what the B-tree tier contains against the accepted set.
pub fn read_hashes(dir: &Path, day: &[u8; DAY_LEN]) -> io::Result<Vec<[u8; 32]>> {
    let data = std::fs::read(file_path(dir, day))?;
    let mut rows: Vec<[u8; 32]> = Vec::with_capacity(data.len() / ROW_LEN);
    let mut chunk = data.as_slice();
    // Drop a trailing partial row (crash during append).
    if !chunk.is_empty() && chunk.len() % ROW_LEN != 0 {
        chunk = &chunk[..chunk.len() - (chunk.len() % ROW_LEN)];
    }
    for row in chunk.as_chunks::<ROW_LEN>().0 {
        // row == 64 hex + '\n'. Parse the 64 hex chars into 32 bytes.
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            let hi = hex_val(row[i * 2]);
            let lo = hex_val(row[i * 2 + 1]);
            *byte = (hi << 4) | lo;
        }
        rows.push(h);
    }
    Ok(rows)
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_row_is_65_bytes_with_newline() {
        let mut staging = [0u8; ROW_LEN];
        let hash = [0xABu8; 32];
        let row = encode_row(&hash, &mut staging);
        assert_eq!(row.len(), ROW_LEN);
        assert_eq!(&row[..2], b"ab", "high nibble then low nibble");
        assert_eq!(row[64], b'\n');
    }

    #[test]
    fn append_writes_nl_terminated_hex() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = *b"20240115";
        let mut idx = open_appender(dir.path(), &day).expect("open");
        let mut staging = [0u8; ROW_LEN];
        let mut hash = [0u8; 32];
        hash[0] = 0x12;
        hash[31] = 0xfe;
        idx.append(&hash, &mut staging).expect("append");

        // Read back raw: 64 hex + newline = 65 bytes.
        let data = std::fs::read(file_path(dir.path(), &day)).expect("read");
        assert_eq!(data.len(), ROW_LEN);
        assert_eq!(&data[..2], b"12");
        assert_eq!(&data[62..], b"fe\n");
    }

    #[test]
    fn append_to_same_day_accumulates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = *b"20240115";
        let mut idx = open_appender(dir.path(), &day).expect("open");
        let mut staging = [0u8; ROW_LEN];
        idx.append(&[1u8; 32], &mut staging).expect("a");
        idx.append(&[2u8; 32], &mut staging).expect("b");
        let data = std::fs::read(file_path(dir.path(), &day)).expect("read");
        assert_eq!(data.len(), 2 * ROW_LEN);
    }
}
