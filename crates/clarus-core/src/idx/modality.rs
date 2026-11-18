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
    /// The modality string is not a conformant CS value (PS3.5 Table 6.2-1):
    /// at most 16 chars drawn from uppercase A-Z, digits 0-9, SPACE and `_`.
    InvalidModality,
    /// Underlying filesystem I/O failure.
    Io(io::Error),
}

impl std::fmt::Display for ModalityIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModalityIndexError::EmptyModality => write!(f, "modality index: empty modality"),
            ModalityIndexError::InvalidModality => write!(f, "modality index: invalid modality"),
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
    /// Full path of the `.idx` file, kept so [`append`](Self::append) can run
    /// the no-repair newline-boundary check without re-deriving the path.
    path: PathBuf,
    file: File,
    /// Set when an append failed part-way through a row.
    poisoned: bool,
    /// Whether this handle may truncate a torn tail on [`reopen`](Self::reopen).
    ///
    /// Only the rollover writer (which owns today's never-mapped file) may cut
    /// a tail; recovery and the fixer open past days with
    /// [`open_appender_no_repair`] and must leave every existing byte in place.
    repair: bool,
}

/// Open (or create) the append-only handle for a modality/day pair.
///
/// This is the ROLLOVER WRITER's opener: it may truncate a torn tail, because
/// it targets TODAY's file - the day the writer currently appends to, which the
/// mmap read path never maps. Recovery and the runtime fixer append to PAST
/// days that may already be mapped as sealed, so they must use
/// [`open_appender_no_repair`] instead.
pub fn open_appender(
    dir: &Path,
    modality: &str,
    day: &DayKey,
) -> Result<ModalityIndex, ModalityIndexError> {
    let idx = open_handle(dir, modality, day, true)?;
    let path = file_path(dir, modality, day);
    // Modality rows are variable-length and NEWLINE-terminated, so alignment is
    // not the test: a torn tail is one without the trailing `\n`, and the next
    // append would fuse its bytes onto the fragment - producing a UID that
    // belongs to no instance. Same truncate rule as `sop_owner`, and safe for
    // the same reason (today's file is never mmapped).
    super::sop_owner::repair_torn_newline_tail(&path).map_err(ModalityIndexError::Io)?;
    Ok(idx)
}

/// Open (or create) the append-only handle WITHOUT repairing a torn tail.
///
/// Recovery and the runtime fixer append to PAST days, whose files the mmap
/// read path may already have mapped as sealed (`idx/preload.rs`). Truncating
/// a mapped file faults SIGBUS on Linux, while appending is benign. A torn
/// tail on a past day is an accelerator-only loss (the manifest is the truth;
/// the lazy fixer re-heals on the next pass), never a reason to truncate. Only
/// the rollover writer - which owns today's never-mapped file - may cut a
/// tail, so it keeps [`open_appender`].
pub fn open_appender_no_repair(
    dir: &Path,
    modality: &str,
    day: &DayKey,
) -> Result<ModalityIndex, ModalityIndexError> {
    open_handle(dir, modality, day, false)
}

/// Shared open: CS validation + directory creation + append-only handle.
///
/// The tail repair itself stays in [`open_appender`]; the helper only records
/// the opener's repair policy on the handle so [`reopen`](ModalityIndex::reopen)
/// can honor it later.
fn open_handle(
    dir: &Path,
    modality: &str,
    day: &DayKey,
    repair: bool,
) -> Result<ModalityIndex, ModalityIndexError> {
    // A modality is one CS value (PS3.3 C.7.3.1.1). PS3.5 Table 6.2-1 bounds
    // CS to at most 16 chars drawn from uppercase A-Z, digits 0-9, SPACE and
    // underscore, so one allowlist replaces the three former ad-hoc guards.
    // It catches what they missed: a Windows drive prefix (`D:`, `D:x`) is not
    // a CS value (`:` is illegal), yet `Path::join` replaces the whole base
    // path for a drive prefix, so the index would land outside `idx/modality/`.
    // `/`, `\`, `.`, `..` and lowercase all fall out of the allowlist too.
    // Fail-closed: a non-conformant value is refused rather than re-homed.
    // (Reserved device names `CON`/`NUL` are legal CS strings; the filesystem
    // itself refuses to create them as directories on Windows.)
    let cs_ok = !modality.is_empty()
        && modality.len() <= 16
        && modality
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b' ');
    if !cs_ok {
        return Err(ModalityIndexError::InvalidModality);
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
        path,
        file,
        poisoned: false,
        repair,
    })
}

/// Ensure a variable-length `.idx` file ends with a newline, appending one if
/// it does not.
///
/// A no-repair append (reconcile/heal) must NOT truncate a past day's file:
/// the mmap read path may already have mapped it as sealed, and cutting a
/// mapped file faults SIGBUS on Linux. Instead, when the final byte is not
/// `\n` (a torn tail), a single `\n` is appended so the next row starts on
/// its own line and the torn fragment stays readable as its own UID.
///
/// Only the LAST byte is examined (no whole-file read), and the newline is
/// written through a fresh O_APPEND handle after the read handle is dropped
/// (a second handle cannot append on Windows while the read handle is open).
/// Two concurrent checks may both see the torn tail and both append one `\n`
/// - one empty line, which `read_study_uids` skips. Benign, so no lock.
pub(crate) fn ensure_newline_boundary(path: &Path) -> io::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if len == 0 {
        return Ok(());
    }
    let mut f = OpenOptions::new().read(true).open(path)?;
    f.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    drop(f);
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut w = OpenOptions::new().append(true).open(path)?;
    w.write_all(b"\n")
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

/// Errno reported by `append_warm` on an already-poisoned writer.
///
/// `EIO` rather than 0: 0 means "success" to any caller that only checks the
/// code, and a poisoned writer must never look like one.
const EPOISONED: i32 = 5;

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
        if self.poisoned {
            return Err(ModalityIndexError::Io(io::Error::from(
                io::ErrorKind::BrokenPipe,
            )));
        }
        // A no-repair handle (past-day reconcile/heal) must not truncate a
        // mapped file; it establishes a newline boundary instead so this row
        // does not fuse onto a torn tail. The rollover writer's own file is
        // already repaired at open, so the check is skipped for it.
        if !self.repair {
            ensure_newline_boundary(&self.path).map_err(ModalityIndexError::Io)?;
        }
        match self.file.write_all(row) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.poisoned = true;
                Err(ModalityIndexError::Io(e))
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
    /// allocator call in ANY branch.
    pub fn append_warm(&mut self, row: &[u8]) -> Result<(), i32> {
        if self.poisoned {
            return Err(EPOISONED);
        }
        match self.file.write_all(row) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.poisoned = true;
                let code = e.raw_os_error().unwrap_or(0);
                std::mem::forget(e);
                Err(code)
            }
        }
    }

    /// Whether a failed append left this handle unusable.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Replace a poisoned handle with a fresh one.
    ///
    /// Rows here are newline-terminated and variable length, so a fragment
    /// left by a failed write would be fused onto the next UID - producing one
    /// row that belongs to no study. When the handle was opened by
    /// [`open_appender`], the torn tail is cut here. A no-repair handle
    /// (opened by [`open_appender_no_repair`]) targets a past day the mmap
    /// read path may already have mapped as sealed, so its reopen must not
    /// truncate: it only swaps in a fresh descriptor and clears the poison.
    pub fn reopen(&mut self, dir: &Path) -> Result<(), ModalityIndexError> {
        let path = file_path(dir, &self.modality, &self.day);
        // Drop the old descriptor before truncating: on Windows a second
        // handle cannot shorten a file another handle holds open.
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(ModalityIndexError::Io)?;
        if self.repair {
            super::sop_owner::repair_torn_newline_tail(&path).map_err(ModalityIndexError::Io)?;
        } else {
            // A no-repair reopen swaps in a fresh descriptor but must not cut
            // a mapped past-day file; the newline-boundary check keeps the next
            // append off a torn tail without truncating anything.
            ensure_newline_boundary(&path).map_err(ModalityIndexError::Io)?;
        }
        self.poisoned = false;
        Ok(())
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
    // A row is `uid\n`; the final segment without a terminating newline is a
    // torn append (a crash between `write_all` and the newline), not a study
    // UID. `split` returns it as if it were whole, so it is excluded below.
    let mut segments: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    if data.last() != Some(&b'\n') {
        segments.pop();
    }
    for line in segments {
        if line.is_empty() {
            continue;
        }
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
    /// Both are also non-CS, so the allowlist rejects them with the rest.
    #[test]
    fn open_rejects_dot_and_dot_dot_modality() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            open_appender(dir.path(), "..", b"20240115"),
            Err(ModalityIndexError::InvalidModality)
        ));
        assert!(matches!(
            open_appender(dir.path(), ".", b"20240115"),
            Err(ModalityIndexError::InvalidModality)
        ));
        // CS forbids '.' entirely, so a dotted modality is rejected too.
        assert!(matches!(
            open_appender(dir.path(), "CT.1", b"20240115"),
            Err(ModalityIndexError::InvalidModality)
        ));
    }

    /// The CS allowlist (PS3.5 Table 6.2-1) must reject every value that is
    /// not a conformant Modality - including a Windows drive prefix, which the
    /// old ad-hoc guards let through and which `Path::join` re-homes outside
    /// `idx/modality/` - and accept conformant values (space and underscore
    /// are legal CS chars). Reserved device names (`CON`, `NUL`) are NOT
    /// asserted here: they are legal CS strings (uppercase), so the allowlist
    /// admits them, and on Windows `create_dir_all` rejects them with a
    /// reserved-name error (fail-closed via the filesystem).
    #[test]
    fn open_rejects_non_cs_modalities() {
        let dir = tempfile::tempdir().expect("tempdir");
        let overlong = "ABCDEFGHIJKLMNOPQ"; // 17 chars, one past the CS bound
        for bad in ["D:", "D:x", "..", ".", "", "ct", "CT.1", overlong] {
            assert!(
                matches!(
                    open_appender(dir.path(), bad, b"20240115"),
                    Err(ModalityIndexError::InvalidModality)
                ),
                "modality {bad:?} must be rejected as non-CS"
            );
        }
        for ok in ["CT", "MR", "PR", "CT_MR", "US 1"] {
            assert!(
                open_appender(dir.path(), ok, b"20240115").is_ok(),
                "conformant CS modality {ok:?} must open"
            );
        }
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

    /// A no-repair append must establish a newline boundary before the next row
    /// so a torn tail stays a separate UID instead of fusing onto it.
    ///
    /// A file ending `1.2\\n1.` (a torn `1.2.3`) appended with `2.3\\n` used to
    /// become `1.2\\n1.2.3\\n` - the healed row is not readable and the fragment
    /// is glued onto a UID that belongs to no study.
    #[test]
    fn a_no_repair_append_establishes_a_newline_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = *b"20240115";
        let path = file_path(dir.path(), "CT", &day);
        {
            let mut w = open_appender(dir.path(), "CT", &day).expect("open");
            let mut row = [0u8; 128];
            w.append(format_row("1.2", &mut row)).expect("seed");
        }
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(b"1.").expect("torn write");
        }

        let mut w = open_appender_no_repair(dir.path(), "CT", &day).expect("no repair");
        let mut row = [0u8; 128];
        w.append(format_row("2.3", &mut row)).expect("append");

        let uids = read_study_uids(dir.path(), "CT", &day).expect("read");
        assert_eq!(
            uids,
            vec!["1.2".to_string(), "1.".to_string(), "2.3".to_string()],
            "the torn fragment must stay its own UID, not fuse into 1.2.3"
        );
    }

    /// A failed append must not be followed by an in-place append.
    ///
    /// Rows are newline-terminated, so a fragment left by a failed write would
    /// be fused onto the next UID - one row naming an instance that does not
    /// exist.
    #[test]
    fn a_failed_append_poisons_the_writer_until_it_reopens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = *b"20250101";
        let mut idx = open_appender(dir.path(), "CT", &day).expect("open");
        let mut row = [0u8; MAX_ROW_LEN];
        idx.append(format_row("uid-a", &mut row)).expect("a");

        let path = file_path(dir.path(), "CT", &day);
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(b"uid-part").expect("torn write");
        }

        idx.poisoned = true;
        assert!(
            idx.append(format_row("uid-b", &mut row)).is_err(),
            "a poisoned writer must refuse further appends"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as u64,
            b"uid-a\n".len() as u64 + 8
        );

        idx.reopen(dir.path()).expect("reopen");
        assert!(!idx.is_poisoned());
        idx.append(format_row("uid-b", &mut row)).expect("b");
        let data = std::fs::read(&path).expect("read");
        assert_eq!(data, b"uid-a\nuid-b\n", "no joined UID");
    }

    /// The no-repair opener appends after a torn tail, never truncates it, and
    /// establishes a newline boundary so the next row is not fused onto the
    /// fragment.
    ///
    /// Recovery and the fixer append to PAST days whose files the mmap read
    /// path may already have mapped as sealed. Truncating a mapped file faults
    /// SIGBUS on Linux, while appending is benign, so those paths use the
    /// no-repair variant. This pins that it leaves every existing byte in
    /// place, inserts the missing newline, and lands the new row on its own
    /// line.
    #[test]
    fn no_repair_append_preserves_a_torn_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = *b"20250101";
        {
            let mut idx = open_appender(dir.path(), "CT", &day).expect("open");
            let mut row = [0u8; 128];
            idx.append(format_row("uid-a", &mut row)).expect("seed");
        }
        let path = file_path(dir.path(), "CT", &day);
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).expect("open");
            f.write_all(b"uid-torn").expect("torn write");
        }
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            b"uid-a\nuid-torn".len() as u64,
            "fixture must start torn"
        );

        let mut idx = open_appender_no_repair(dir.path(), "CT", &day).expect("no-repair open");
        let mut row = [0u8; 128];
        idx.append(format_row("uid-b", &mut row)).expect("append");

        let data = std::fs::read(&path).expect("read");
        assert_eq!(
            data, b"uid-a\nuid-torn\nuid-b\n",
            "the append must keep the torn tail, add the missing newline, and \
             land the new row on its own line"
        );
    }

    /// Reopen on a no-repair handle must not truncate the torn tail; it
    /// establishes the newline boundary instead.
    ///
    /// A no-repair handle targets a PAST day the mmap read path may already
    /// have mapped as sealed; truncating a mapped file faults SIGBUS on Linux.
    /// `reopen` must therefore restore a usable descriptor and clear the
    /// poison flag without cutting bytes, honoring the opener's no-repair
    /// policy - the same guarantee the append path already pins.
    #[test]
    fn reopen_on_no_repair_handle_preserves_a_torn_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = *b"20250101";
        {
            let mut idx = open_appender(dir.path(), "CT", &day).expect("open");
            let mut row = [0u8; 128];
            idx.append(format_row("uid-a", &mut row)).expect("seed");
        }
        let path = file_path(dir.path(), "CT", &day);
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).expect("open");
            f.write_all(b"uid-torn").expect("torn write");
        }
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            b"uid-a\nuid-torn".len() as u64,
            "fixture must start torn"
        );

        let mut idx = open_appender_no_repair(dir.path(), "CT", &day).expect("no-repair open");
        idx.poisoned = true;
        idx.reopen(dir.path()).expect("reopen");
        assert!(!idx.is_poisoned());

        let data = std::fs::read(&path).expect("read");
        assert_eq!(
            data, b"uid-a\nuid-torn\n",
            "reopen must keep the torn tail and add the missing newline on a \
             no-repair handle"
        );
    }
}
