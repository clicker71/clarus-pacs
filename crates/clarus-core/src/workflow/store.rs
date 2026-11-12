// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! File-backed MWL and MPPS store (`workflow/mwl/`, `workflow/mpps/`).
//!
//! Two directories under one root, one CBOR file per record, keyed by the
//! record's own UID. Writes are atomic (tmp + rename, the discipline the study
//! manifest, the UPS workitem and the commitment store all share); reads are a
//! per-UID file read, and a search is a linear scan of the MPPS/MWL directory.
//!
//! The record sets are small by construction: an MWL row exists only for a
//! scheduled step, and an MPPS row is one performed step written by one
//! modality. There is no index and no lock, because a scan over a directory of
//! this size costs less than the file it reads, and the concurrency rule of MPPS
//! is last-writer-wins (there is no Transaction UID in MPPS - unlike UPS
//! 0008,1195 - so one MPPS UID is written by one modality and a lock would
//! guard a race that the standard's own model does not have).

use crate::workflow::record::{MppsError, MppsRecord, SpsError, SpsRecord};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

/// Process-wide counter for temp-file names (see `write_atomic`).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Errors produced by the workflow store.
#[derive(Debug)]
pub enum WorkflowStoreError {
    /// Underlying filesystem failure.
    Io(io::Error),
    /// The bytes on disk did not decode as an SPS record.
    Sps(SpsError),
    /// The bytes on disk did not decode as an MPPS record.
    Mpps(MppsError),
    /// An insert found a record with the same UID already on disk.
    Duplicate(String),
}

impl std::fmt::Display for WorkflowStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "workflow store I/O error: {e}"),
            Self::Sps(e) => write!(f, "workflow store SPS decode error: {e}"),
            Self::Mpps(e) => write!(f, "workflow store MPPS decode error: {e}"),
            Self::Duplicate(uid) => write!(f, "workflow record already exists: {uid}"),
        }
    }
}

impl std::error::Error for WorkflowStoreError {}

impl From<io::Error> for WorkflowStoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The two record families the store holds, each with its own subdirectory.
const MWL_DIR: &str = "mwl";
const MPPS_DIR: &str = "mpps";

/// The on-disk workflow store rooted at a `workflow/` directory.
///
/// `Clone` is cheap (it carries one `PathBuf`), so the store is shared across
/// handler threads with no interior lock; each write is atomic and each file is
/// touched by one writer at a time (MPPS is last-writer-wins).
#[derive(Debug, Clone)]
pub struct WorkflowStore {
    /// Root directory holding the `mwl/` and `mpps/` subdirectories.
    root: PathBuf,
}

impl WorkflowStore {
    /// Open a store rooted at `root` (the directories are created lazily on
    /// the first write).
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The store root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding the MWL records.
    #[must_use]
    pub fn mwl_dir(&self) -> PathBuf {
        self.root.join(MWL_DIR)
    }

    /// The directory holding the MPPS records.
    #[must_use]
    pub fn mpps_dir(&self) -> PathBuf {
        self.root.join(MPPS_DIR)
    }

    // -- MWL -------------------------------------------------------------

    /// Path to one SPS record's CBOR file.
    ///
    /// The key is sanitised the same way the commitment store sanitises its
    /// Transaction UID: a key containing a separator or `..` must not be able
    /// to address a file outside the store.
    #[must_use]
    pub fn sps_path(&self, sps_uid: &str) -> PathBuf {
        self.mwl_dir().join(format!("{}.cbor", safe_stem(sps_uid)))
    }

    /// Read one SPS record. `Ok(None)` when the file does not exist.
    pub fn find_sps(&self, sps_uid: &str) -> Result<Option<SpsRecord>, WorkflowStoreError> {
        let path = self.sps_path(sps_uid);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(WorkflowStoreError::Io(e)),
        };
        SpsRecord::decode(&bytes)
            .map(Some)
            .map_err(WorkflowStoreError::Sps)
    }

    /// Write an SPS record atomically (create or replace).
    pub fn put_sps(&self, rec: &SpsRecord) -> Result<(), WorkflowStoreError> {
        let bytes = rec.encode().map_err(WorkflowStoreError::Sps)?;
        write_atomic(&self.sps_path(&rec.sps_uid), &bytes)
    }

    /// Insert a new SPS record, failing when the UID is already present.
    ///
    /// The install is an atomic create-if-absent, so two concurrent inserts of
    /// one UID cannot both pass an existence test and then both publish.
    pub fn insert_sps(&self, rec: &SpsRecord) -> Result<(), WorkflowStoreError> {
        let bytes = rec.encode().map_err(WorkflowStoreError::Sps)?;
        write_if_absent(&self.sps_path(&rec.sps_uid), &bytes)
    }

    /// Delete one SPS record. `true` when a file was removed.
    pub fn delete_sps(&self, sps_uid: &str) -> Result<bool, WorkflowStoreError> {
        delete_file(&self.sps_path(sps_uid))
    }

    /// Every SPS record in the store (linear scan; corrupt files are skipped,
    /// the same resilience the UPS scan applies).
    pub fn all_sps(&self) -> Result<Vec<SpsRecord>, WorkflowStoreError> {
        let mut out = Vec::new();
        scan_dir(
            &self.mwl_dir(),
            |bytes| SpsRecord::decode(bytes).map_err(WorkflowStoreError::Sps),
            &mut out,
        )?;
        Ok(out)
    }

    // -- MPPS ------------------------------------------------------------

    /// Path to one MPPS record's CBOR file.
    #[must_use]
    pub fn mpps_path(&self, mpps_uid: &str) -> PathBuf {
        self.mpps_dir()
            .join(format!("{}.cbor", safe_stem(mpps_uid)))
    }

    /// Read one MPPS record. `Ok(None)` when the file does not exist.
    pub fn find_mpps(&self, mpps_uid: &str) -> Result<Option<MppsRecord>, WorkflowStoreError> {
        let path = self.mpps_path(mpps_uid);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(WorkflowStoreError::Io(e)),
        };
        MppsRecord::decode(&bytes)
            .map(Some)
            .map_err(WorkflowStoreError::Mpps)
    }

    /// Write an MPPS record atomically (create or replace).
    ///
    /// MPPS has no Transaction UID, so there is no compare-and-swap here: the
    /// write is last-writer-wins on the record's own UID, which is the model
    /// PS3.4 F.7 defines (one performer writes one step).
    pub fn put_mpps(&self, rec: &MppsRecord) -> Result<(), WorkflowStoreError> {
        let bytes = rec.encode().map_err(WorkflowStoreError::Mpps)?;
        write_atomic(&self.mpps_path(&rec.mpps_uid), &bytes)
    }

    /// Insert a new MPPS record, failing when the UID is already present - the
    /// N-CREATE duplicate path (PS3.4 F.7.2.1: N-CREATE on an existing SOP
    /// Instance is refused).
    pub fn insert_mpps(&self, rec: &MppsRecord) -> Result<(), WorkflowStoreError> {
        let bytes = rec.encode().map_err(WorkflowStoreError::Mpps)?;
        write_if_absent(&self.mpps_path(&rec.mpps_uid), &bytes)
    }

    /// Delete one MPPS record. `true` when a file was removed.
    pub fn delete_mpps(&self, mpps_uid: &str) -> Result<bool, WorkflowStoreError> {
        delete_file(&self.mpps_path(mpps_uid))
    }

    /// Every MPPS record in the store (linear scan; corrupt files are skipped).
    pub fn all_mpps(&self) -> Result<Vec<MppsRecord>, WorkflowStoreError> {
        let mut out = Vec::new();
        scan_dir(
            &self.mpps_dir(),
            |bytes| MppsRecord::decode(bytes).map_err(WorkflowStoreError::Mpps),
            &mut out,
        )?;
        Ok(out)
    }
}

/// Encode a record key as a file stem, injectively.
///
/// A DICOM UID is `[0-9.]`, but an SPS identifier may also be a site's TEXTUAL
/// step id, and the router has already validated only the path segment. Two
/// requirements follow:
///
/// - nothing may address a file outside the store, so separators and `..` are
///   not allowed through;
/// - the mapping must be INJECTIVE. The previous `[^0-9.] -> '_'` rule sent both
///   `"SPS-A"` and `"SPS-B"` to `_____`, so two different steps shared one file
///   and `put_sps` overwrote one with the other.
///
/// Printable ASCII that is safe in a filename passes through unchanged, so a
/// normal UID keeps its exact stem. Everything else is percent-encoded as
/// `%XX`, and `%` itself is encoded first, which makes the map reversible and
/// therefore collision-free for distinct inputs.
fn safe_stem(uid: &str) -> String {
    let mut out = String::with_capacity(uid.len());
    let bytes = uid.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let passthrough = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_');
        // `.` passes through UNCHANGED so every existing DICOM-UID file keeps
        // its current name - re-encoding it would rename the whole store for a
        // conformance fix. The one case that must not survive is a `..` run,
        // which would address a parent directory; the SECOND dot of such a run
        // is encoded, which leaves a normal UID (`1.2.840...`) untouched.
        let dot_ok = b == b'.' && bytes.get(i.wrapping_sub(1)) != Some(&b'.');
        if passthrough || dot_ok {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
            out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
        }
    }
    out
}

/// Write bytes to `path` atomically via a `.tmp` sibling + rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), WorkflowStoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    // Unique per write: a fixed `{uid}.cbor.tmp` is one buffer shared by every
    // concurrent `put_*` of the same record, so their bytes interleaved and
    // whichever renamed last published a spliced record.
    tmp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        // Never leave a half-written sibling behind: the reader skips `.tmp`
        // names, but the next writer would have to clean it up.
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(WorkflowStoreError::Io(e))
        }
    }
}

/// Write bytes to `path` only if nothing occupies it, atomically.
///
/// `hard_link` is the affordable create-if-absent: it fails with
/// `AlreadyExists` in the kernel, so two concurrent inserts cannot both observe
/// "absent" and both publish - the loser is told `Duplicate` instead of silently
/// overwriting. Same primitive `WorkItemStore::write_if_absent` and the
/// commitment store use.
fn write_if_absent(path: &Path, bytes: &[u8]) -> Result<(), WorkflowStoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(WorkflowStoreError::Duplicate(stem_of(path)))
        }
        Err(e) => Err(WorkflowStoreError::Io(e)),
    }
}

/// The stem of a record path, for the `Duplicate` payload.
fn stem_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Remove a file, reporting whether it existed.
fn delete_file(path: &Path) -> Result<bool, WorkflowStoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(WorkflowStoreError::Io(e)),
    }
}

/// Decode every `*.cbor` file in `dir` through `decode`, pushing results into
/// `out`. A missing directory is an empty result; a corrupt file is skipped
/// (the scan stays useful when one record is damaged).
fn scan_dir<T, F>(dir: &Path, decode: F, out: &mut Vec<T>) -> Result<(), WorkflowStoreError>
where
    F: Fn(&[u8]) -> Result<T, WorkflowStoreError>,
{
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(WorkflowStoreError::Io(e)),
    };
    for entry in rd {
        let entry = entry?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let fname = entry.file_name().to_string_lossy().into_owned();
        // Skip the atomic-write tmp sibling; it is never a committed record.
        if !fname.ends_with(".cbor") {
            continue;
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let Ok(rec) = decode(&bytes) {
            out.push(rec);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::record::{MppsRecord, SpsRecord};

    fn sps(uid: &str) -> SpsRecord {
        SpsRecord {
            sps_uid: uid.to_string(),
            station_aet: "CT01".to_string(),
            sps_start_date: "20261011".to_string(),
            sps_start_time: "090000".to_string(),
            modality: "CT".to_string(),
            scheduled_performing_physician: "SMITH^JOHN".to_string(),
            patient_name: "IVANOV^IVAN".to_string(),
            patient_id: "PAT-1".to_string(),
            ..Default::default()
        }
    }

    fn mpps(uid: &str, status: &str) -> MppsRecord {
        MppsRecord {
            mpps_uid: uid.to_string(),
            status: status.to_string(),
            modality: Some("CT".to_string()),
            ..Default::default()
        }
    }

    fn store() -> WorkflowStore {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir path: the store outlives this scope in the tests.
        WorkflowStore::new(dir.keep().join("workflow"))
    }

    #[test]
    fn sps_insert_find_delete_roundtrip() {
        let s = store();
        let rec = sps("1.2.3");
        s.insert_sps(&rec).expect("insert");
        assert_eq!(s.find_sps("1.2.3").expect("find"), Some(rec));
        assert!(s.delete_sps("1.2.3").expect("delete"));
        assert!(s.find_sps("1.2.3").expect("find").is_none());
        assert!(!s.delete_sps("1.2.3").expect("delete-again"));
    }

    #[test]
    fn sps_duplicate_insert_conflicts() {
        let s = store();
        let rec = sps("1.2.3");
        s.insert_sps(&rec).expect("insert");
        assert!(matches!(
            s.insert_sps(&rec),
            Err(WorkflowStoreError::Duplicate(_))
        ));
    }

    /// C7: two different textual SPS identifiers must not share a file.
    ///
    /// `sps_uid` also carries a site's textual step id. The old stem rule
    /// mapped every non-digit character to `_`, so `"SPS-A"` and `"SPS-B"` both
    /// became `_____`: one insert silently overwrote the other and a scan
    /// returned one record where two were stored.
    #[test]
    fn textual_sps_ids_do_not_collide_on_one_file() {
        let s = store();
        s.insert_sps(&sps("SPS-A")).expect("insert A");
        s.insert_sps(&sps("SPS-B")).expect("insert B");

        assert_eq!(
            s.find_sps("SPS-A").expect("find A").map(|r| r.sps_uid),
            Some("SPS-A".to_string()),
            "A must still be readable"
        );
        assert_eq!(
            s.find_sps("SPS-B").expect("find B").map(|r| r.sps_uid),
            Some("SPS-B".to_string()),
            "B must not have overwritten A"
        );
        assert_eq!(s.all_sps().expect("scan").len(), 2, "both records stored");

        // The stem is still a safe filename: no separator can escape the store.
        let stem = safe_stem("../../etc/passwd");
        assert!(!stem.contains('/'), "no path separator: {stem}");
        assert!(!stem.contains('\\'), "no path separator: {stem}");
        assert!(!stem.contains(".."), "no traversal: {stem}");
    }

    /// C8: the SPS install is create-if-absent, so an occupied path is refused
    /// rather than replaced.
    ///
    /// A `find()`-then-`put()` pair let two concurrent inserts of one UID both
    /// pass the test and both publish. Driving the install directly reproduces
    /// the state a racing winner leaves behind; the old path overwrote it.
    #[test]
    fn an_sps_install_onto_an_occupied_path_is_refused() {
        let s = store();
        let winner = sps("SPS-X");
        let bytes = winner.encode().expect("encode");
        write_if_absent(&s.sps_path("SPS-X"), &bytes).expect("winner installs");

        let mut loser = sps("SPS-X");
        loser.station_aet = "MR99".to_string();
        let loser_bytes = loser.encode().expect("encode");
        assert!(
            matches!(
                write_if_absent(&s.sps_path("SPS-X"), &loser_bytes),
                Err(WorkflowStoreError::Duplicate(_))
            ),
            "an occupied path must be refused"
        );

        let back = s.find_sps("SPS-X").expect("find").expect("present");
        assert_eq!(
            back.station_aet, "CT01",
            "the winner must not have been replaced"
        );
    }

    /// C8: the MPPS install has the same guarantee as SPS (the duplicate path
    /// is covered by `mpps_duplicate_insert_conflicts`; this pins the case a
    /// racing winner produces, where the path is already occupied).
    #[test]
    fn an_mpps_install_onto_an_occupied_path_is_refused() {
        let s = store();
        let winner = mpps("1.2.9", "IN PROGRESS");
        let bytes = winner.encode().expect("encode");
        write_if_absent(&s.mpps_path("1.2.9"), &bytes).expect("winner installs");

        let loser = mpps("1.2.9", "COMPLETED");
        let loser_bytes = loser.encode().expect("encode");
        assert!(
            matches!(
                write_if_absent(&s.mpps_path("1.2.9"), &loser_bytes),
                Err(WorkflowStoreError::Duplicate(_))
            ),
            "an occupied path must be refused"
        );
        let back = s.find_mpps("1.2.9").expect("find").expect("present");
        assert_eq!(back.status, "IN PROGRESS", "the winner must survive");
    }

    #[test]
    fn sps_scan_returns_every_record() {
        let s = store();
        s.put_sps(&sps("1.2.3")).unwrap();
        s.put_sps(&sps("1.2.4")).unwrap();
        let all = s.all_sps().expect("scan");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn mpps_insert_find_delete_roundtrip() {
        let s = store();
        let rec = mpps("4.5.6", "IN PROGRESS");
        s.insert_mpps(&rec).expect("insert");
        assert_eq!(s.find_mpps("4.5.6").expect("find"), Some(rec));
        assert!(s.delete_mpps("4.5.6").expect("delete"));
        assert!(s.find_mpps("4.5.6").expect("find").is_none());
    }

    #[test]
    fn mpps_duplicate_insert_conflicts() {
        let s = store();
        let rec = mpps("4.5.6", "IN PROGRESS");
        s.insert_mpps(&rec).expect("insert");
        assert!(matches!(
            s.insert_mpps(&rec),
            Err(WorkflowStoreError::Duplicate(_))
        ));
    }

    #[test]
    fn mpps_put_replaces_last_writer_wins() {
        let s = store();
        s.put_mpps(&mpps("4.5.6", "IN PROGRESS")).unwrap();
        s.put_mpps(&mpps("4.5.6", "COMPLETED")).unwrap();
        let got = s.find_mpps("4.5.6").expect("find").expect("some");
        assert_eq!(got.status, "COMPLETED");
    }

    #[test]
    fn a_key_with_a_separator_stays_inside_the_store() {
        // A traversal-shaped key must not escape the directory.
        let s = store();
        let path = s.sps_path("../escape");
        assert!(path.starts_with(s.mwl_dir()));
    }

    #[test]
    fn missing_directory_is_an_empty_scan() {
        let s = store();
        assert!(s.all_sps().expect("scan").is_empty());
        assert!(s.all_mpps().expect("scan").is_empty());
    }
}
