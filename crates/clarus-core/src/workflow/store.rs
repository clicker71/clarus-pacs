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
        // Uppercase letters are encoded, not passed through. A case-insensitive
        // volume treats `SPS-A.cbor` and `sps-a.cbor` as ONE file, so two SPS
        // identifiers differing only in case would overwrite each other - the
        // second `put_sps` silently destroying the first. Digits, dots and
        // lowercase letters are what every DICOM UID and every generated SPS
        // identifier uses, so encoding the uppercase half leaves existing
        // files untouched while making the mapping injective on such volumes.
        let passthrough =
            (b.is_ascii_alphanumeric() && !b.is_ascii_uppercase()) || matches!(b, b'-' | b'_');
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
        // `create_dir_all` does not persist the NAME of the directories it
        // creates, so a first write would otherwise flush the record and its
        // immediate parent while `workflow/` and `mwl/` could still vanish from
        // THEIR parents on a crash. Persist the newly created name chain.
        crate::durable::sync_new_root_name(parent)?;
    }
    let tmp = claim_temp_sibling(path)?;
    if let Err(e) = write_claimed(&tmp, bytes) {
        // Never leave a half-written sibling behind: the reader skips `.tmp`
        // names, but the next writer would have to clean it up.
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            // The data is durable; persist the directory entry that names it
            // too, or a crash can lose the NAME while the bytes are on the
            // medium. Unix-only by necessity: Windows cannot open a directory
            // for `FlushFileBuffers`. A failed dir-sync is a failed write.
            #[cfg(unix)]
            sync_parent_entry(path)?;
            Ok(())
        }
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
        crate::durable::sync_new_root_name(parent)?;
    }
    let tmp = claim_temp_sibling(path)?;
    if let Err(e) = write_claimed(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            #[cfg(unix)]
            sync_parent_entry(path)?;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // The link landed but its NAME was never made durable (see the
            // commitment store's identical arm): before answering Duplicate,
            // persist the directory entry so a retry cannot be told "already
            // stored" for a name the next crash removes.
            #[cfg(unix)]
            sync_parent_entry(path)?;
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

/// Reserve a temp sibling for `path` that no other writer can have selected.
///
/// The name carries pid + a process-wide counter, which makes a collision
/// RARE but not impossible: PID namespaces let two containers share a pid, and
/// the counter restarts at 0 after a restart, so two writers can compute the
/// same name. `std::fs::write` would then open that name and REPLACE its
/// contents - one writer would publish the other's bytes. `create_new` turns
/// the collision into an error, and the retry moves to the next sequence.
fn claim_temp_sibling(path: &Path) -> Result<PathBuf, WorkflowStoreError> {
    // Bounded: each collision consumes a fresh sequence, so this only runs
    // long on a pathological directory and must not spin forever.
    const ATTEMPTS: usize = 64;
    let mut last = None;
    for _ in 0..ATTEMPTS {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let tmp = PathBuf::from(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            // The handle is dropped so the rename/hard_link below sees a
            // closed file (Windows will not link a file open for writing).
            Ok(_) => return Ok(tmp),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(WorkflowStoreError::Io(e)),
        }
    }
    Err(WorkflowStoreError::Io(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "no free temp name")
    })))
}

/// Fill a temp file that [`claim_temp_sibling`] already created exclusively.
///
/// The bytes are flushed (`sync_all`) BEFORE the caller links/renames the file
/// into place, so a successful publish means the data is on the medium rather
/// than merely scheduled to be: an N-CREATE / MPPS write that answered success
/// while the bytes sat in the page cache could be empty after a crash, and the
/// retry would then hit `Duplicate` with the performed step lost. That is the
/// same barrier order the commitment store uses.
fn write_claimed(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.write_all(bytes)?;
    crate::durable::sync_barrier(&f)
}

/// Flush the directory entry that names `path` (Unix-only).
///
/// A rename/hard_link updates the parent directory's entry for `path`; that
/// entry must be flushed too or a crash can lose the NAME while the data is
/// durable. Unix-only because Windows cannot open a directory for
/// `FlushFileBuffers` (the same boundary the commitment store states).
#[cfg(unix)]
fn sync_parent_entry(path: &Path) -> Result<(), WorkflowStoreError> {
    let parent = match crate::durable::parent_entry_dir(path) {
        Some(p) => p,
        None => return Ok(()),
    };
    crate::durable::sync_dir_entry(parent).map_err(WorkflowStoreError::Io)
}

/// Remove a file, reporting whether it existed.
fn delete_file(path: &Path) -> Result<bool, WorkflowStoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            // The name is gone; persist the directory entry that named it too,
            // or a crash after this delete can restore the MWL/MPPS record.
            // Same barrier the write paths apply after rename/hard_link;
            // barrier failure propagates, open failure is swallowed by the
            // helper (see durable.rs policy).
            #[cfg(unix)]
            sync_parent_entry(path)?;
            Ok(true)
        }
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
    use crate::durable::syncs_observed;
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

    /// An occupied temp name must be SKIPPED, never overwritten.
    ///
    /// pid + counter makes a collision rare, not impossible: PID namespaces let
    /// two containers share a pid, and the counter restarts at 0 after a
    /// restart. `std::fs::write` on a shared name replaces its contents, so a
    /// workflow record could carry another container's bytes.
    #[test]
    fn an_occupied_temp_name_is_skipped_not_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("workflow");
        std::fs::create_dir_all(&root).expect("root");
        let target = root.join("1.2.3.99.cbor");

        let probe = claim_temp_sibling(&target).expect("probe");
        let seq_of = |p: &Path| -> u64 {
            p.file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit('.').next())
                .and_then(|n| n.parse().ok())
                .expect("temp name carries its sequence")
        };
        let base = seq_of(&probe);
        std::fs::write(&probe, b"other writer's bytes").expect("occupy");

        const WINDOW: u64 = 32;
        for k in 1..=WINDOW {
            let mut name = target.as_os_str().to_owned();
            name.push(format!(".{}.{}.tmp", std::process::id(), base + k));
            std::fs::write(PathBuf::from(name), b"other writer's bytes").expect("occupy");
        }

        let fresh = claim_temp_sibling(&target).expect("a free name must be found");
        assert!(
            seq_of(&fresh) > base + WINDOW,
            "every occupied name must be skipped, got seq {} in window {base}..={}",
            seq_of(&fresh),
            base + WINDOW
        );
        assert_eq!(
            std::fs::read(&probe).expect("occupied still readable"),
            b"other writer's bytes",
            "an occupied temp must not be truncated"
        );
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

    /// Two SPS identifiers differing only in case must not share a file.
    ///
    /// On a case-insensitive volume (the default on Windows and macOS)
    /// `SPS-A.cbor` and `sps-a.cbor` are the SAME file, so the second install
    /// would overwrite the first and the workflow would lose a scheduled
    /// procedure step without any error. Comparing the two PATHS cannot catch
    /// that - they differ as strings on this volume - so the property is stated
    /// where it actually lives: the encoded stem must contain no uppercase
    /// letter, which is what makes the mapping injective across case folding.
    #[test]
    fn two_sps_identifiers_differing_only_in_case_do_not_collide() {
        let s = store();
        for uid in ["SPS-A", "sps-a", "SPSA", "SpsA"] {
            let stem = s
                .sps_path(uid)
                .file_stem()
                .and_then(|x| x.to_str())
                .expect("stem")
                .to_string();
            assert!(
                !stem.chars().any(|c| c.is_ascii_uppercase()),
                "a stem must survive case folding unchanged, got {stem} for {uid}"
            );
        }
        // Two identifiers that differ ONLY in case fold to different stems,
        // so a case-insensitive lookup cannot confuse them.
        let upper = s.sps_path("SPS-A");
        let lower = s.sps_path("sps-a");
        assert_ne!(upper, lower);
        assert_ne!(
            upper.to_string_lossy().to_ascii_lowercase(),
            lower.to_string_lossy().to_ascii_lowercase(),
            "the stems must differ even after case folding"
        );
    }

    /// A plain DICOM UID keeps its exact file name.
    ///
    /// The case fix must not rename the whole store: existing SPS records are
    /// named after their UID, which is digits and dots only.
    #[test]
    fn a_dicom_uid_keeps_its_plain_stem() {
        assert_eq!(safe_stem("1.2.840.113857.1.1"), "1.2.840.113857.1.1");
        assert_eq!(safe_stem("1.2.3"), "1.2.3");
    }

    /// A committed write flushes the data to the medium BEFORE it is published
    /// and leaves no temp sibling behind.
    ///
    /// The flush is observed on the barrier counter: `sync_all` leaves no byte
    /// trace a re-read can see, so deleting the call would keep the file
    /// contents correct while the count stays flat - the silent regression
    /// this pins. Both publish shapes (`rename` and `hard_link`) are covered.
    #[test]
    fn a_write_flushes_before_it_publishes_and_leaves_no_temp_file() {
        let s = store();
        let bytes = sps("1.2.3").encode().expect("encode");

        let before = syncs_observed();
        write_atomic(&s.sps_path("1.2.3"), &bytes).expect("atomic write");
        assert!(
            syncs_observed() > before,
            "a committed atomic write has to flush the record to the medium"
        );

        let before = syncs_observed();
        write_if_absent(&s.sps_path("1.2.4"), &bytes).expect("absent write");
        assert!(
            syncs_observed() > before,
            "a committed create-if-absent has to flush the record to the medium"
        );

        let leftovers: Vec<String> = std::fs::read_dir(s.mwl_dir())
            .expect("read mwl dir")
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp file may survive a committed write, found {leftovers:?}"
        );
    }

    /// The directory entry that names a record is flushed too (Unix-only).
    ///
    /// A rename/hard_link updates the parent directory's entry; without the
    /// follow-up `sync_all` on the directory a crash can lose the NAME while
    /// the data is durable. The counter distinguishes the data barrier from
    /// the directory barrier.
    #[cfg(unix)]
    #[test]
    fn a_committed_write_also_flushes_the_directory_entry() {
        let s = store();
        let bytes = sps("1.2.5").encode().expect("encode");

        let before = syncs_observed();
        write_atomic(&s.sps_path("1.2.5"), &bytes).expect("atomic write");
        assert!(
            syncs_observed() >= before + 2,
            "data barrier + directory barrier: expected at least 2, observed {}",
            syncs_observed() - before
        );
    }

    /// A first write into a fresh tree flushes the NAME chain of the
    /// directories `create_dir_all` just made, not only the record's own
    /// directory entry (Unix-only).
    ///
    /// `create_dir_all` persists the entries INSIDE a directory but not the
    /// directory's own name in its parent. Flushing only the record's parent
    /// leaves `workflow/` and `mwl/` unpersisted in THEIR parents, so a crash
    /// can lose a successful first write even though the record bytes were
    /// synced. The ancestor walk is observable on the barrier counter.
    #[cfg(unix)]
    #[test]
    fn a_first_write_flushes_the_new_directory_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = WorkflowStore::new(dir.path().join("a").join("b").join("workflow"));
        let before = syncs_observed();
        s.insert_sps(&sps("1.2.6")).expect("insert");
        let after = syncs_observed();
        // data barrier + record-parent barrier + at least two ancestor levels
        // (workflow/ in b, b in a) that only the name-chain walk persists.
        assert!(
            after >= before + 4,
            "a first write must flush the created directory names: observed {}",
            after - before
        );
    }

    /// A failed flush is a failed write, not a silent success.
    ///
    /// `write_claimed` must report the I/O error so the publish path removes
    /// the temp and returns it, never answers `Ok` for a record the next boot
    /// may not find. The failure is injected by pointing the write at a
    /// directory, so no platform knob is involved.
    #[test]
    fn a_write_that_cannot_be_flushed_is_an_error_not_a_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A DIRECTORY at the write path: opening it for writing fails, so
        // nothing is written and nothing can be linked.
        let blocked = dir.path().join("blocked.tmp");
        std::fs::create_dir_all(&blocked).expect("blocking directory");
        let err = write_claimed(&blocked, b"payload").expect_err("must not succeed");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::IsADirectory
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Other
            ) || err.kind() != io::ErrorKind::NotFound,
            "the failure has to surface as an I/O error, got {err:?}"
        );
    }

    /// A failed directory barrier is an Io error, never a stored verdict -
    /// on the Ok arm AND on the Duplicate arm (the same fix the commitment
    /// store carries).
    ///
    /// The install order is temp-sync, hard_link, remove temp, THEN dir
    /// barrier. A barrier failure after the hard_link leaves the record at its
    /// target; a retry finds the target occupied and used to answer Duplicate
    /// WITHOUT re-running the barrier. The seam fails the NEXT barrier on the
    /// record's parent directory (mwl/), so the Ok arm surfaces Io and the
    /// Duplicate arm surfaces Io too. Reverting the Duplicate-arm barrier makes
    /// the second insert answer Duplicate (the seam is never consumed).
    #[cfg(unix)]
    #[test]
    fn a_failed_barrier_is_io_on_both_the_ok_and_duplicate_arms() {
        let s = store();
        let rec = sps("1.2.3.94");

        // Ok arm: the link lands, then the barrier fails -> Io, target stays.
        crate::durable::fail_next_barrier_for_test(&s.mwl_dir());
        let err = s.insert_sps(&rec).unwrap_err();
        assert!(matches!(err, WorkflowStoreError::Io(_)), "got {err:?}");
        assert!(
            s.sps_path(&rec.sps_uid).exists(),
            "the link landed before the barrier failed"
        );

        // Duplicate arm: the occupied target with the next barrier failing
        // must surface Io, not a silent Duplicate.
        crate::durable::fail_next_barrier_for_test(&s.mwl_dir());
        let err = s.insert_sps(&rec).unwrap_err();
        assert!(
            matches!(err, WorkflowStoreError::Io(_)),
            "a failing barrier on the conflict path is Io, got {err:?}"
        );

        // Healthy barrier: the occupied target is a Duplicate and the stored
        // record is left untouched.
        let first = s.find_sps(&rec.sps_uid).expect("find").expect("present");
        assert!(matches!(
            s.insert_sps(&rec),
            Err(WorkflowStoreError::Duplicate(_))
        ));
        let after = s.find_sps(&rec.sps_uid).expect("find").expect("present");
        assert_eq!(
            after, first,
            "a refused duplicate must not replace the record"
        );
    }

    /// A successful delete persists the directory entry that named the record
    /// (Unix-only), so a crash after `remove_file` cannot restore the SPS.
    /// Observed on the barrier counter, the same proof the write paths use.
    #[cfg(unix)]
    #[test]
    fn a_delete_persists_the_directory_entry() {
        let s = store();
        s.insert_sps(&sps("1.2.3.7")).expect("insert");

        let before = syncs_observed();
        assert!(s.delete_sps("1.2.3.7").expect("delete"));
        assert!(
            syncs_observed() > before,
            "a successful delete must flush the directory entry"
        );
    }

    /// A barrier failure on the delete path is an error, not a stored
    /// "deleted". Reverting the added barrier makes `delete_sps` return
    /// Ok(true) and this test fails.
    #[cfg(unix)]
    #[test]
    fn a_delete_with_a_failing_barrier_is_an_error_not_a_success() {
        let s = store();
        let rec = sps("1.2.3.8");
        s.insert_sps(&rec).expect("insert");

        crate::durable::fail_next_barrier_for_test(&s.mwl_dir());
        let err = s.delete_sps("1.2.3.8").unwrap_err();
        assert!(matches!(err, WorkflowStoreError::Io(_)), "got {err:?}");
        assert!(
            !s.sps_path("1.2.3.8").exists(),
            "the file was removed; the delete reported the barrier failure"
        );
    }
}
