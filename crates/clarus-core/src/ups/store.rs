// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! File-backed UPS workitem store (`workitems/{uid}.cbor`).
//!
//! a KISS filesystem store replacing the SQLite workitems CRUD for the
//! UPS-RS path (C-014). Each workitem lives in its own CBOR file keyed by
//! `{uid}.cbor`. Writes are atomic (tmp + rename, same as the study manifest);
//! updates are compare-and-swap on the Transaction UID (G-08 optimistic lock);
//! search is a linear scan of the `workitems/` directory (schema counts are
//! small - the same E-014 full-scan precedent as patients).
//!
//! The record format lives in [`super::record`]; this module is the on-disk
//! layer (mapping [`WorkItemRecord`] to paths) and the filter/sort semantics
//! that mirror the SQLite oracle (`clarus-adapters/src/sqlite/operations.rs`).

use crate::ups::record::WorkItemRecord;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Errors produced by the workitem store.
#[derive(Debug)]
pub enum StoreError {
    /// Underlying filesystem failure.
    Io(io::Error),
    /// The bytes on disk did not decode as a workitem record.
    Decode(String),
    /// Compare-and-swap failed: the on-disk Transaction UID differs from the
    /// caller's expectation (the 409 optimistic-lock path).
    CasConflict(String),
    /// The UID is not a single conforming path component (fail-closed at the
    /// store boundary; see [`crate::uid::validate_path_uid`]).
    InvalidUid(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "workitem store I/O error: {e}"),
            StoreError::Decode(e) => write!(f, "workitem store decode error: {e}"),
            StoreError::CasConflict(uid) => write!(f, "workitem CAS conflict: {uid}"),
            StoreError::InvalidUid(uid) => write!(f, "invalid workitem UID: {uid}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// The on-disk workitem store rooted at a `workitems/` directory.
///
/// `Clone` is cheap: it just carries the root `PathBuf`. Methods are
/// synchronous and take `&self`, so the store can be shared across handler
/// threads with no interior lock per request - mutations take a STRIPED lock
/// (see [`uid_lock`]) so two writers of the same workitem cannot interleave
/// their read-modify-write, while writers of different workitems never
/// contend.
///
/// # Deployment invariant: ONE writer process per root
///
/// The striped locks are PROCESS-LOCAL. Exactly one process writes this store
/// root; multi-writer shared-root deployments (two servers, or a server and a
/// maintenance tool, pointed at the same directory) are unsupported. There is
/// no cross-process lock, so two such writers would be serialised only by the
/// filesystem's atomic create (`hard_link` for inserts, `rename` for updates) -
/// enough to avoid a torn file, but NOT enough to keep a read-modify-write
/// coherent: the second writer's update could silently overwrite the first's.
#[derive(Debug, Clone)]
pub struct WorkItemStore {
    /// Directory holding `{uid}.cbor` files. One process writes it (see above).
    root: PathBuf,
}

/// Number of striped locks. 64 is a compromise: enough that unrelated
/// workitems rarely collide, few enough that the table is 64 `Arc`s.
const UID_LOCK_STRIPES: usize = 64;

/// Process-wide striped locks, one per `hash(uid) % UID_LOCK_STRIPES`.
///
/// A `std::sync::Mutex` rather than a per-UID map: the store's job is to
/// serialise a read-modify-write of ONE file, and a fixed table does that
/// without growing unboundedly with the number of workitems ever seen.
fn uid_lock(uid: &str) -> Arc<Mutex<()>> {
    static STRIPES: OnceLock<Vec<Arc<Mutex<()>>>> = OnceLock::new();
    let stripes = STRIPES.get_or_init(|| {
        (0..UID_LOCK_STRIPES)
            .map(|_| Arc::new(Mutex::new(())))
            .collect()
    });
    // FNV-1a over the uid, the same cheap mix `auth.rs` uses; the quality of
    // the distribution only has to beat "every uid in one stripe".
    let mut h: u32 = 0x811c_9dc5;
    for &b in uid.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    Arc::clone(&stripes[(h as usize) % UID_LOCK_STRIPES])
}

/// Search filters over the store: workitem_uid, study_uid, accession_number,
/// step_label, station_aet, scheduled_workitem_code_value,
/// requested_procedure_id, patient_id, modality, workflow_state plus
/// limit/offset, `ORDER BY created_at DESC`.
#[derive(Debug, Clone, Default)]
pub struct StoreFilter<'a> {
    /// Workitem UID (0008,0018) - the identity of the WORKITEM.
    ///
    /// This is the value in the `/workitems/{uid}` path, not the study. The
    /// (0020,000D) search key goes in `study_uid` below.
    pub workitem_uid: Option<&'a str>,
    /// Study Instance UID of the input study (0020,000D).
    ///
    /// Matched against `WorkItemRecord.input_study_uid`, which is captured
    /// from the (0040,4021) Input Information Sequence at create time. A
    /// workitem's own UID is a DIFFERENT value, so this is a separate field
    /// and not a second use of `workitem_uid`: routing the (0020,000D) key
    /// into `workitem_uid` made the search silently return nothing.
    pub study_uid: Option<&'a str>,
    /// (0040,A370) Referenced Request Sequence > (0008,0050) Accession Number.
    ///
    /// Matched against `WorkItemRecord.accession_number`, which the create
    /// path reads from the top-level (0008,0050) the requester sent. PS3.4
    /// Table CC.2.5-3 lists the key as a REQUIRED matching key, so a wider
    /// server accepts it; the sequence in the standard's path is how a
    /// request reaches the attribute, and on this server it is one scalar.
    pub accession_number: Option<&'a str>,
    /// (0074,1204) Procedure Step Label - a standard matching key.
    pub step_label: Option<&'a str>,
    /// (0040,4025) Scheduled Station Name Code Sequence > (0008,0100) Code
    /// Value - a standard matching key (PS3.4 Table CC.2.5-3, R).
    ///
    /// The internal field is still called `station_aet` because that is the
    /// fact the record holds: AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3 puts the
    /// performer AE Title in the Code Value.
    pub station_aet: Option<&'a str>,
    /// (0040,4018) Scheduled Workitem Code Sequence > (0008,0100) Code Value -
    /// a standard matching key (PS3.4 Table CC.2.5-3, R).
    ///
    /// The standard types the key as a Sequence, so the request states the Code
    /// Value through a path; the record stores the whole code item and this
    /// filter compares the Code Value inside it. A workitem with no scheduled
    /// code does not match a code query - it names no code to match.
    pub scheduled_workitem_code_value: Option<&'a str>,
    /// (0040,A370) Referenced Request Sequence > (0040,1001) Requested
    /// Procedure ID - a standard matching key (PS3.4 Table CC.2.5-3, R).
    pub requested_procedure_id: Option<&'a str>,
    pub patient_id: Option<&'a str>,
    /// (0040,4005) Scheduled Procedure Step Start DateTime - a REQUIRED
    /// matching key (PS3.4 Table CC.2.5-3, "Single Value or Range").
    ///
    /// A `-`-separated value is a RANGE across the stored DT, which is what the
    /// standard's matchType states; a single value is an exact match.
    pub sps_start_datetime: Option<&'a str>,
    /// Default UTC offset (seconds) applied at COMPARISON time when a DT value
    /// carries no `&ZZXX` zone suffix.
    ///
    /// An offset-free DT is local time (PS3.5 6.2), so the comparison happens
    /// in the deployment's local frame - `[ups] local_utc_offset`, default
    /// +0000 = UTC. A value with an explicit zone keeps its own offset and
    /// stored data is never rewritten. Default 0 keeps every other
    /// `StoreFilter` construction byte-identical.
    pub default_offset_secs: i64,
    /// (0074,1200) Scheduled Procedure Step Priority - a REQUIRED matching key
    /// (Single Value).
    pub sps_priority: Option<&'a str>,
    /// (0010,0010) Patient's Name - a REQUIRED matching key.
    pub patient_name: Option<&'a str>,
    /// (0008,0060) Modality - vendor extended, matched case-insensitively
    /// (a modality is a DICOM CS value, so its case is not significant).
    pub modality: Option<&'a str>,
    pub workflow_state: Option<&'a str>,
    /// (0040,4041) Input Readiness State - a matching key in PS3.4 Table
    /// CC.2.5-3. `READY` must exclude an INCOMPLETE record, so a workitem with
    /// no readiness does NOT match a stated one: it has not been declared
    /// ready, and treating "absent" as "ready" would hand a performer inputs
    /// the requester never confirmed.
    pub input_readiness: Option<&'a str>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

impl WorkItemStore {
    /// Open (or lazily create on first write) a store rooted at `root`.
    pub fn new(root: PathBuf) -> Self {
        WorkItemStore { root }
    }

    /// The store root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to a single workitem's CBOR file.
    ///
    /// Fails closed on a UID that is not a single conforming path component.
    /// The store is a public library API, so a caller that forgets its own
    /// validation must not be able to make `Path::join` escape `root`: an
    /// absolute `{uid}.cbor` replaces the root and a separator addresses a
    /// sibling. [`crate::uid::validate_path_uid`] is the canonical rule.
    pub fn path_for(&self, uid: &str) -> Result<PathBuf, StoreError> {
        crate::uid::validate_path_uid(uid).map_err(|e| StoreError::InvalidUid(e.to_string()))?;
        Ok(self.root.join(format!("{uid}.cbor")))
    }

    /// Read one workitem by UID. Returns `Ok(None)` when the file does not
    /// exist (mirrors the SQLite `find_workitem_by_uid` `QueryReturnedNoRows`
    /// -> `Ok(None)` contract).
    pub fn find_by_uid(&self, uid: &str) -> Result<Option<WorkItemRecord>, StoreError> {
        let path = self.path_for(uid)?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Io(e)),
        };
        WorkItemRecord::decode(&bytes)
            .map(Some)
            .map_err(|e| StoreError::Decode(e.to_string()))
    }

    /// Insert a new workitem. Fails with a conflict if it already exists.
    ///
    /// The write runs under the uid's stripe lock and the file itself is
    /// created with `hard_link` from the temp file: a `link` that finds the
    /// target already present fails with `AlreadyExists`, so "create only if
    /// absent" holds even against a writer this process cannot see (another
    /// process, another container). The lock alone would only serialise THIS
    /// process's threads.
    pub fn insert(&self, rec: &WorkItemRecord) -> Result<(), StoreError> {
        let stripe = uid_lock(&rec.workitem_uid);
        let _guard = stripe.lock().unwrap_or_else(|e| e.into_inner());
        self.write_if_absent(rec)
    }

    /// CAS update: only apply the write when the on-disk Transaction UID
    /// matches `expected_tuid`. When it does not match, returns
    /// [`StoreError::CasConflict`] (the caller maps this to 409).
    ///
    /// A `None` `expected_tuid` (e.g. a create path where the caller already
    /// performed the staleness check) is treated as "no optimistic-lock
    /// precondition" and updates unconditionally.
    ///
    /// A CAS with an expectation against a record that is NOT THERE is a
    /// conflict, never a create: the workitem was deleted between the caller's
    /// read and this write, and silently writing the file back would resurrect
    /// it (PS3.18 11.6 makes the Transaction UID the proof that the caller is
    /// updating a state it actually observed).
    ///
    /// The Transaction UID ALONE is not enough. Table 11.6-1 keeps the UID
    /// unchanged across a successful update, so two concurrent
    /// read-modify-write cycles that both read the same state present the same
    /// token, both pass, and the second silently overwrites the first - the
    /// per-UID lock below covers only this call, not the caller's read and
    /// merge. The record's `revision` closes that: `rec.revision` is the value
    /// the caller READ, a mismatch means somebody wrote in between, and a
    /// successful write bumps it so the next stale reader is caught too.
    ///
    /// Revisions must be named explicitly with `expected_revision`.
    ///
    /// `Some(n)` means "I read revision n, write only if it is still n", and
    /// `n == 0` is a real observation: a record that was created but never
    /// updated really is at revision 0. Treating 0 as "no precondition" made
    /// two callers who both read a revision-0 record both pass their check, so
    /// the second overwrote the first with no conflict reported. `None` is the
    /// separate, explicit "unconditional" case, reserved for callers that
    /// never read a record (the create path).
    ///
    /// KNOWN LIMIT: the revision identifies a POSITION, not an INCARNATION.
    /// Deleting a workitem and creating a different one under the same UID
    /// restarts the counter at 0, so a caller that read revision 0 before the
    /// delete presents `Some(0)`, finds a record at revision 0, and overwrites
    /// the replacement without a conflict. Closing that needs the caller to
    /// name the incarnation it read - a creation timestamp or a stored
    /// generation - which means a new parameter on this public signature and a
    /// matching field on every record. It is recorded here rather than
    /// half-done: the current API cannot express "the same record" and
    /// pretending otherwise would give callers a guarantee they do not have.
    pub fn cas_update(
        &self,
        rec: &WorkItemRecord,
        expected_tuid: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<(), StoreError> {
        let stripe = uid_lock(&rec.workitem_uid);
        let _guard = stripe.lock().unwrap_or_else(|e| e.into_inner());
        let existing = self.find_by_uid(&rec.workitem_uid)?;
        match (&existing, expected_tuid) {
            (None, Some(_)) => {
                return Err(StoreError::CasConflict(format!(
                    "workitem vanished before the update: {}",
                    rec.workitem_uid
                )));
            }
            (None, None) => {}
            (Some(existing), Some(expected)) => match existing.transaction_uid.as_deref() {
                Some(on_disk) if on_disk == expected => {}
                Some(on_disk) => {
                    return Err(StoreError::CasConflict(format!(
                        "transaction uid mismatch: expected '{}', on-disk '{}'",
                        expected, on_disk
                    )));
                }
                None => {
                    return Err(StoreError::CasConflict(format!(
                        "no on-disk transaction uid for '{}'",
                        rec.workitem_uid
                    )));
                }
            },
            (Some(_), None) => {}
        }
        // The revision check runs AFTER the UID check so the more specific
        // message wins: a caller that named the wrong transaction learns that,
        // not that it also lost a race.
        if let (Some(on_disk), Some(expected)) = (&existing, expected_revision)
            && expected != on_disk.revision
        {
            return Err(StoreError::CasConflict(format!(
                "workitem {} was modified concurrently: expected revision {}, on-disk {}",
                rec.workitem_uid, expected, on_disk.revision
            )));
        }
        if let (None, Some(expected)) = (&existing, expected_revision) {
            return Err(StoreError::CasConflict(format!(
                "workitem vanished before the update: {} (expected revision {})",
                rec.workitem_uid, expected
            )));
        }
        self.write_bumped(rec, existing.as_ref())
    }

    /// Update without any precondition.
    ///
    /// The explicit form of "I never read this record": it still bumps the
    /// revision so later conditional callers notice the write, but it performs
    /// no comparison. Callers holding an observation must use
    /// [`Self::cas_update`] instead.
    pub fn upsert_unconditional(&self, rec: &WorkItemRecord) -> Result<(), StoreError> {
        let stripe = uid_lock(&rec.workitem_uid);
        let _guard = stripe.lock().unwrap_or_else(|e| e.into_inner());
        let existing = self.find_by_uid(&rec.workitem_uid)?;
        self.write_bumped(rec, existing.as_ref())
    }

    /// Write `rec` with the revision bumped past whatever is on disk.
    ///
    /// Caller holds the stripe lock and has already applied its preconditions.
    fn write_bumped(
        &self,
        rec: &WorkItemRecord,
        existing: Option<&WorkItemRecord>,
    ) -> Result<(), StoreError> {
        // Bump on every successful write so a later reader holding the value it
        // just observed is caught. Wrapping is harmless: a record would have to
        // be written 2^64 times for the counter to repeat.
        let mut to_write = rec.clone();
        to_write.revision = existing.map_or(1, |e| e.revision.wrapping_add(1));
        self.write_atomic(&to_write)
    }

    /// Delete a workitem by UID. Returns `true` when a file was removed.
    ///
    /// Takes the same striped lock as [`insert`] and [`cas_update`]: a delete
    /// racing an insert of the SAME uid could otherwise remove the file the
    /// insert just committed, or the insert could re-create it under the
    /// delete's `remove_file`, and the store would hand out an "absent" and a
    /// "present" answer for one workitem in the same instant.
    pub fn delete(&self, uid: &str) -> Result<bool, StoreError> {
        let stripe = uid_lock(uid);
        let _guard = stripe.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.path_for(uid)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {
                // The name is gone; persist the directory entry that named it
                // too, or a crash after this delete can restore the workitem.
                // Same barrier the insert/update paths apply after the
                // rename/hard_link; barrier failure propagates, open failure
                // is swallowed by the helper (see durable.rs policy).
                #[cfg(unix)]
                sync_parent_entry(&path)?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // A previous delete may have unlinked the file and then failed
                // the directory barrier; this retry is what completes it. The
                // entry is already gone, so the barrier is the SAME one the
                // success path runs - without it the retry reports a finished
                // deletion whose directory update was never made durable, and
                // a crash can bring the workitem back.
                #[cfg(unix)]
                sync_parent_entry(&path)?;
                Ok(false)
            }
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// Search over the store with the given filters (linear scan, sorted by
    /// `created_at DESC` to match the SQLite oracle; UID ascending as a stable
    /// tiebreak so ordering is deterministic across both backends).
    pub fn search(&self, f: &StoreFilter<'_>) -> Result<(Vec<WorkItemRecord>, usize), StoreError> {
        let mut all = Vec::new();
        let rd = std::fs::read_dir(&self.root);
        let rd = match rd {
            Ok(rd) => rd,
            // A store with no directory yet is an empty search, not an error.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => return Err(StoreError::Io(e)),
        };
        for entry in rd {
            let entry = entry.map_err(StoreError::Io)?;
            // A `file_type` failure is an enumeration failure, not a directory
            // with fewer files: `unwrap_or(false)` turned it into "skip this
            // entry" and the caller received an ordinary page with an
            // ordinary match count.
            if !entry.file_type().map_err(StoreError::Io)?.is_file() {
                continue;
            }
            let fname = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = fname.strip_suffix(".cbor") else {
                continue;
            };
            // Skip the atomic-write tmp sibling (never a committed record).
            if stem.ends_with(".tmp") {
                continue;
            }
            // A record that vanishes between the walk and the read is an
            // EXPECTED omission (a concurrent delete); anything else - an I/O
            // error, an undecodable file - means the page is incomplete, and
            // the caller has to be able to tell that from a complete one.
            let rec = match self.find_by_uid(stem) {
                Ok(Some(r)) => r,
                // Gone now: the delete that removed it owns the truth.
                Ok(None) => continue,
                Err(StoreError::Io(e)) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if matches_filter(&rec, f) {
                all.push(rec);
            }
        }

        // ORDER BY created_at DESC, then UID ascending (deterministic tiebreak).
        all.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.workitem_uid.cmp(&b.workitem_uid))
        });

        // limit/offset applied after sort (SQLite semantics). The
        // pre-pagination length is reported so the caller can compute the
        // PS3.18 8.3.4.4.1 `remaining` count for the truncation Warning.
        let matches = all.len();
        let offset = f.offset.unwrap_or(0) as usize;
        if offset >= matches {
            return Ok((Vec::new(), matches));
        }
        let skip = all.into_iter().skip(offset);
        let limit = f.limit.map(|l| l as usize).unwrap_or(usize::MAX);
        Ok((skip.take(limit).collect(), matches))
    }

    /// Atomically write a record via tmp + rename.
    fn write_atomic(&self, rec: &WorkItemRecord) -> Result<(), StoreError> {
        let path = self.path_for(&rec.workitem_uid)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            // `create_dir_all` persists the entries INSIDE a directory but not
            // the directory's own name in its parent, so a first write would
            // leave `workitems/` (and the store root above it) unpersisted.
            crate::durable::sync_new_root_name(parent)?;
        }
        let tmp = self.write_tmp(rec)?;
        match std::fs::rename(&tmp, &path) {
            Ok(()) => {
                // The data is durable; persist the directory entry that names
                // it too. Unix-only by necessity: Windows cannot open a
                // directory for `FlushFileBuffers`.
                #[cfg(unix)]
                sync_parent_entry(&path)?;
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(StoreError::Io(e))
            }
        }
    }

    /// Write the record to a uniquely named temp sibling and return its path.
    ///
    /// The temp name carries the pid and a process-wide counter. A fixed
    /// `{uid}.cbor.tmp` is a single buffer shared by every writer of that
    /// workitem: two concurrent writes interleaved their bytes and whichever
    /// renamed last published a spliced record. The counter has to be here and
    /// not in the caller, because several stores (and the CAD path) write into
    /// the same directory.
    fn write_tmp(&self, rec: &WorkItemRecord) -> Result<PathBuf, StoreError> {
        let path = self.path_for(&rec.workitem_uid)?;
        let bytes = rec
            .encode()
            .map_err(|e| StoreError::Decode(e.to_string()))?;
        let tmp = tmp_sibling(&path)?;
        if let Err(e) = write_claimed(&tmp, &bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::Io(e));
        }
        Ok(tmp)
    }

    /// Create the record only if no file exists at its path.
    ///
    /// `hard_link` is the affordable atomic create-if-absent: it fails with
    /// `AlreadyExists` when the target is taken, and it does so in the kernel,
    /// so a second process cannot slip a write in between the check and the
    /// install. Returns [`StoreError::CasConflict`] when the workitem is
    /// already there.
    fn write_if_absent(&self, rec: &WorkItemRecord) -> Result<(), StoreError> {
        let path = self.path_for(&rec.workitem_uid)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            crate::durable::sync_new_root_name(parent)?;
        }
        let tmp = self.write_tmp(rec)?;
        let linked = std::fs::hard_link(&tmp, &path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => {
                #[cfg(unix)]
                sync_parent_entry(&path)?;
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // The link landed but its NAME was never made durable (see the
                // commitment store's identical arm): before answering
                // CasConflict, persist the directory entry so a retry cannot be
                // told "already stored" for a name the next crash removes.
                #[cfg(unix)]
                sync_parent_entry(&path)?;
                Err(StoreError::CasConflict(format!(
                    "workitem already exists: {}",
                    rec.workitem_uid
                )))
            }
            Err(e) => Err(StoreError::Io(e)),
        }
    }
}

/// Whether a record satisfies all the filters (AND semantics).
fn matches_filter(rec: &WorkItemRecord, f: &StoreFilter<'_>) -> bool {
    if let Some(v) = f.workitem_uid
        && rec.workitem_uid != v
    {
        return false;
    }
    // The input study: a workitem with no input study does not match a study
    // query at all (None != Some), which is right - it names no study to match.
    if let Some(v) = f.study_uid
        && rec.input_study_uid.as_deref() != Some(v)
    {
        return false;
    }
    if let Some(v) = f.step_label
        && rec.step_label.as_deref() != Some(v)
    {
        return false;
    }
    if let Some(v) = f.station_aet
        && rec.station_aet.as_deref() != Some(v)
    {
        return false;
    }
    // (0040,4018) > (0008,0100): the record holds the code as an item, so the
    // comparison reaches into it. `None` in the record is "no code" and never
    // equals a stated value, the same rule the study key applies above.
    if let Some(v) = f.scheduled_workitem_code_value
        && rec
            .scheduled_workitem_code
            .as_ref()
            .map(|c| c.value.as_str())
            != Some(v)
    {
        return false;
    }
    if let Some(v) = f.patient_id
        && rec.patient_id.as_deref() != Some(v)
    {
        return false;
    }
    // (0040,0010) Patient's Name: literal comparison, the same convention the
    // sibling LO/SH keys use here.
    //
    // One deliberate difference: the caller has already stripped surrounding
    // whitespace from `v`, and the comparison ignores it on BOTH sides. A PN
    // arriving through a URL is routinely padded, and a padding difference is
    // not a different patient.
    if let Some(v) = f.patient_name
        && rec.patient_name.as_deref().map(str::trim) != Some(v.trim())
    {
        return false;
    }
    if let Some(v) = f.sps_priority
        && rec.sps_priority.as_deref() != Some(v)
    {
        return false;
    }
    // (0040,4041) Input Readiness State: an absent readiness is not READY.
    // The stored value is a DICOM CS, so the comparison is case-insensitive.
    if let Some(v) = f.input_readiness
        && !rec
            .input_readiness
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case(v))
    {
        return false;
    }
    // (0040,4005) ScheduledProcedureStepStartDateTime: "Single Value or Range"
    // per Table CC.2.5-3. An empty value matches everything (the zero-length
    // key convention); `<lo>-<hi>` is an inclusive range; anything else is one
    // instant compared exactly.
    //
    // A trailing SPACE is legal padding, not part of the value: PS3.5 6.2
    // requires every character-string VR except UI to be padded with SPACE
    // when an odd length would otherwise result, and DT is in that list
    // (standard/ps3.5/string-value-padding.json `spacePaddedVrs`). The padding
    // belongs to the ENCODING, so it is stripped from the stored value before
    // parsing - otherwise `20261115120000+0100 ` fell through to the lexical
    // fallback and stopped matching the same instant written `20261115110000+0000`.
    //
    // DT is compared as an INSTANT, not as text. The offset is part of the
    // value (PS3.5 Section 6.2), so `20261115120000+0100` and
    // `20261115110000+0000` are the same moment while sorting differently as
    // strings - a lexical range test therefore misses values that sit inside
    // the range it was asked for. Values this parser cannot read fall back to
    // the previous lexical comparison rather than being dropped.
    if let Some(v) = f.sps_start_datetime
        && !v.is_empty()
    {
        let Some(stored) = rec.sps_start_datetime.as_deref().map(str::trim_end) else {
            // A workitem with no start datetime does not match a datetime query.
            return false;
        };
        // The query value carries the same padding rule as the stored one.
        let v = v.trim_end();
        let matched = match dt_instant(stored, f.default_offset_secs) {
            Some(stored_i) => match split_dt_range(v) {
                // A range: both endpoints inclusive. An absent endpoint is an
                // open end, which is what a bare `-hi` / `lo-` means.
                //
                // Both endpoints are already known to parse: `split_dt_range`
                // returns a range only after `dt_instant` accepted each one,
                // and `parse_dt` succeeding is the only way into this arm. A
                // bound that fails here is therefore a programming error, not
                // an input condition - so it rejects rather than widening the
                // range to "no bound", which is what a skipped failure would
                // do.
                (lo_txt, Some(hi_txt)) => {
                    let lo_ok = if lo_txt.is_empty() {
                        true
                    } else {
                        match dt_instant(lo_txt, f.default_offset_secs) {
                            Some(lo) => stored_i >= lo,
                            None => return false,
                        }
                    };
                    let hi_ok = if hi_txt.is_empty() {
                        true
                    } else {
                        match dt_upper_instant(hi_txt, f.default_offset_secs) {
                            Some(hi) => stored_i <= hi,
                            None => return false,
                        }
                    };
                    lo_ok && hi_ok
                }
                // No separator: ONE value. A short value names a PERIOD (a
                // day, a month, a year), not the lowest instant in it, so
                // `20261115` matches any stored instant during that day. A
                // full-precision value's period is one second, which keeps the
                // exact-instant equality a single value means.
                (_, None) => match (
                    dt_instant(v, f.default_offset_secs),
                    dt_upper_instant(v, f.default_offset_secs),
                ) {
                    (Some(lo), Some(hi)) => stored_i >= lo && stored_i <= hi,
                    _ => false,
                },
            },
            // Unparseable stored value: keep the historical lexical
            // comparison, which is the only ordering available for text we
            // cannot place in time. What it MUST NOT do is drop a bound: an
            // endpoint that is present and does not parse rejects the
            // workitem, exactly as the parsed arm above does. Skipping it
            // answered with a wider set than the client asked for.
            None => match split_dt_range(v) {
                (lo, Some(hi)) => {
                    let lo_ok = if lo.is_empty() {
                        true
                    } else if dt_instant(lo, f.default_offset_secs).is_some() {
                        stored >= lo
                    } else {
                        return false;
                    };
                    let hi_ok = if hi.is_empty() {
                        true
                    } else if dt_instant(hi, f.default_offset_secs).is_some() {
                        stored <= hi
                    } else {
                        return false;
                    };
                    lo_ok && hi_ok
                }
                (_, None) => stored == v,
            },
        };
        if !matched {
            return false;
        }
    }
    if let Some(v) = f.requested_procedure_id
        && rec.requested_procedure_id.as_deref() != Some(v)
    {
        return false;
    }
    // (0008,0050) Accession Number: an SH value, matched the same way as the
    // Patient ID above (exact, case-sensitive) - the LO/SH keys of UPS-RS
    // Table CC.2.5-3 are literal string comparisons on this server, and a
    // second convention for a sibling key would be a filter that behaves
    // differently for no reason a caller could predict.
    if let Some(v) = f.accession_number
        && rec.accession_number.as_deref() != Some(v)
    {
        return false;
    }
    // Case-insensitive exact: a modality is a DICOM CS value, and the catalog
    // upper-cases its keys, so a lower-case query must still match.
    if let Some(v) = f.modality
        && !rec.modality.eq_ignore_ascii_case(v)
    {
        return false;
    }
    if let Some(v) = f.workflow_state
        && rec.workflow_state != v
    {
        return false;
    }
    true
}

/// Components of a DICOM DT value shared by [`dt_instant`] and
/// [`dt_upper_instant`].
///
/// The two ends of a range must parse the SAME string with the SAME rules or
/// they drift apart and answer the wrong worklist, so the parsing lives in one
/// place.
struct DtParts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    offset_secs: i64,
    /// Fractional second (`.FFFFFF`) scaled to nanoseconds: the fraction value
    /// times 10^(6 - digit_count). 0 when no fraction was present.
    nanos: u32,
    /// Whether the value carried an explicit `.FFFFFF` fraction. A value with
    /// `.000000` names exactly one instant, so the upper bound must not widen
    /// it to the whole second - which is what "fraction absent, precision 14"
    /// means. The two are only distinguishable with this flag.
    has_fraction: bool,
    /// The least-precise PRESENT component, as the length of the date-time
    /// part actually in the value: 4 (year only), 6 (month), 8 (day), 10
    /// (hour), 12 (minute), 14 (second).
    precision: usize,
}

/// Parse a DICOM DT value into its components and UTC offset.
///
/// DT (PS3.5 Section 6.2) is `YYYYMMDDHHMMSS.FFFFFF&ZZXX`, where the offset is
/// part of the VALUE. Two DT strings naming the same instant differ whenever
/// their zones differ - `20261115120000+0100` and `20261115110000+0000` are the
/// same moment - so a lexical comparison of the raw text is not a time
/// comparison. Storing each value as seconds since the epoch puts every zone on
/// one scale.
///
/// Returns `None` for a value this parser cannot read; the caller falls back to
/// the lexical comparison, which is what every record used before this existed.
fn parse_dt(value: &str, default_offset_secs: i64) -> Option<DtParts> {
    let b = value.as_bytes();
    // Measure the leading run of ASCII digits: that run is the date-time part
    // `YYYYMMDDHHMMSS`, and everything after it (`.FFFFFF` fraction, `&ZZXX`
    // offset) is NOT a component. The offset may follow a trailing-null
    // date-time (standard/ps3.5/dt-vr.json `offsetSuffixNotAComponent`), so the
    // component lengths MUST come from the digit-run length, never from the
    // whole byte length - reading hour/minute/second by `b.len()` read the
    // offset's own bytes and rejected a valid `20261116+0100`.
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    // The date-time part is exactly 4, 6, 8, 10, 12 or 14 digits (year, month,
    // day, hour, minute, second); any other run length is not a DT prefix.
    if !matches!(n, 4 | 6 | 8 | 10 | 12 | 14) {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let slice = b.get(from..to)?;
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse().ok()
    };
    let year = num(0, 4)?;
    let month = if n >= 6 { num(4, 6)? } else { 1 };
    let day = if n >= 8 { num(6, 8)? } else { 1 };
    let hour = if n >= 10 { num(8, 10)? } else { 0 };
    let minute = if n >= 12 { num(10, 12)? } else { 0 };
    let second = if n >= 14 { num(12, 14)? } else { 0 };

    // Fractional seconds: `.` followed by 1..6 digits (PS3.5 Table 6.2-1,
    // `FFFFFF`). The fraction is retained, scaled to nanoseconds: dropping it
    // reduced a value at `.900000` and a range ending `.200000` both to the
    // whole second, so the stored value matched a range it sits OUTSIDE.
    let mut nanos: u32 = 0;
    let mut has_fraction = false;
    let mut rest = &b[n..];
    if let Some(after_dot) = rest.strip_prefix(b".") {
        let digit_count = after_dot.iter().take_while(|c| c.is_ascii_digit()).count();
        // PS3.5 caps the fraction at 6 digits; a longer digit run is not a
        // conformant fraction, so only the first 6 digits scale and the rest
        // falls through to the offset parser below.
        let digits = digit_count.min(6);
        if digits > 0 {
            let frac_val: u32 = std::str::from_utf8(&after_dot[..digits])
                .ok()?
                .parse()
                .ok()?;
            nanos = frac_val * 10u32.pow((6 - digits) as u32);
            has_fraction = true;
        }
        rest = after_dot.get(digit_count..).unwrap_or(&[]);
    }

    let offset_secs = match rest {
        // No zone: an offset-free DT is local time (PS3.5 6.2). This server
        // compares offset-free values in the deployment's local frame, which
        // is the configured `[ups] local_utc_offset` (default +0000 = UTC);
        // a full-precision value carrying an explicit `&ZZXX` keeps its own
        // offset. Stored data is never rewritten.
        [] => default_offset_secs,
        [sign, d0, d1, d2, d3]
            if (*sign == b'+' || *sign == b'-')
                && [d0, d1, d2, d3].iter().all(|c| c.is_ascii_digit()) =>
        {
            let hours: i64 = std::str::from_utf8(&[*d0, *d1]).ok()?.parse().ok()?;
            let minutes: i64 = std::str::from_utf8(&[*d2, *d3]).ok()?.parse().ok()?;
            // PS3.5 6.2 Note 1: the zone offset is bounded to -1200..+1400, and
            // the minutes field is a clock field (00..59).
            //
            // This used to accept anything: `20261115120000-0500-2027` had its
            // SECOND dash read as the range separator, because
            // `parse_dt("0500-2027")` saw year 0500 at offset -2027 minutes
            // (-20:27). The resulting range straddled the wrong period and
            // excluded a December 2026 workitem that sits between the real
            // bounds. Rejecting an out-of-range offset makes that first split
            // fail, so the splitter keeps the valid `-0500` lower bound.
            if !offset_in_range_signed(*sign == b'+', hours, minutes) {
                return None;
            }
            let magnitude = hours * 3600 + minutes * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };

    // Which component is the last one the string actually names. A trailing
    // null component makes the value a PERIOD
    // (standard/ps3.5/dt-vr.json `trailingNullComponentsDenotePeriod`), and
    // dt_upper_instant needs to know where that period ends. It is exactly the
    // digit-run length: the offset suffix is not a component.
    let precision = n;

    Some(DtParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
        offset_secs,
        nanos,
        has_fraction,
        precision,
    })
}

/// The FIRST instant a DICOM DT value names, as `(seconds, nanos)` since the
/// epoch.
///
/// A truncated value fills every omitted component with its lowest value, so
/// `20261116` reads as 2026-11-16 00:00:00 - the start of the period. Use
/// [`dt_upper_instant`] for the other end of a range. The nanosecond part is
/// the fractional second (0 when absent); `(sec, nanos)` tuples compare
/// lexicographically, so a fraction-aware range test is a plain tuple
/// comparison.
fn dt_instant(value: &str, default_offset_secs: i64) -> Option<(i64, u32)> {
    let p = parse_dt(value, default_offset_secs)?;
    let days = crate::time::days_from_civil(p.year, p.month as u32, p.day as u32);
    Some((
        days * 86_400 + p.hour * 3600 + p.minute * 60 + p.second - p.offset_secs,
        p.nanos,
    ))
}

/// The LAST instant a DICOM DT value names, as `(seconds, nanos)`.
///
/// PS3.5 Table 6.2-1: trailing null components mean the value is "not precise
/// to the precision of those components" - `20261116` names the WHOLE day, not
/// midnight (standard/ps3.5/dt-vr.json `trailingNullComponentsDenotePeriod`).
/// The upper bound of a range and a single-value key must cover the whole
/// period, or a conformant query silently answers an empty worklist.
///
/// The end is the last instant of the least-precise PRESENT component. A
/// full-precision value with no fraction names one whole second, so its upper
/// instant is `(sec, 999_999_999)`; a full-precision value WITH a fraction
/// names exactly that instant (`(sec, nanos)`), and `dt_instant` matches it
/// exactly. A shorter value's period ends at the last whole second of the
/// period (with nanos 999_999_999). The UTC offset is applied exactly as in
/// [`dt_instant`].
fn dt_upper_instant(value: &str, default_offset_secs: i64) -> Option<(i64, u32)> {
    let p = parse_dt(value, default_offset_secs)?;
    let days = crate::time::days_from_civil(p.year, p.month as u32, p.day as u32);
    let whole_sec = days * 86_400 + p.hour * 3600 + p.minute * 60 + p.second;
    if p.has_fraction {
        return Some((whole_sec - p.offset_secs, p.nanos));
    }
    // days_from_civil rolls month=13 and day=32 into the next month, so the
    // +1 forms need no clamp.
    let local_end = match p.precision {
        4 => crate::time::days_from_civil(p.year + 1, 1, 1) * 86_400 - 1,
        6 => crate::time::days_from_civil(p.year, (p.month + 1) as u32, 1) * 86_400 - 1,
        8 => crate::time::days_from_civil(p.year, p.month as u32, (p.day + 1) as u32) * 86_400 - 1,
        10 => days * 86_400 + p.hour * 3600 + 3599,
        12 => days * 86_400 + p.hour * 3600 + p.minute * 60 + 59,
        _ => whole_sec,
    };
    Some((local_end - p.offset_secs, 999_999_999))
}

/// Whether a stored DT value satisfies a DT filter value.
///
/// The one implementation of PS3.4 C.2.2.2.5 range matching for DT: a single
/// value names a PERIOD (so a date-only value matches its whole day) and a
/// `lo-hi` pair is an inclusive window with either end open. Every matcher that
/// takes a DT filter - the workitem search and the push subscription filter -
/// calls this, so the two can never disagree about what a range means.
///
/// `stored` is the value from the record; `pattern` is what the filter asked
/// for; `default_offset_secs` is the deployment's local UTC offset applied to
/// values that carry no `&ZZXX` zone (PS3.5 6.2).
///
/// Returns `false` for a stored value the parser cannot place in time rather
/// than guessing, and `false` for a malformed bound rather than widening the
/// window to match everything.
pub fn dt_filter_matches(stored: &str, pattern: &str, default_offset_secs: i64) -> bool {
    let stored = stored.trim_end();
    let pattern = pattern.trim_end();
    let Some(stored_i) = dt_instant(stored, default_offset_secs) else {
        return false;
    };
    match split_dt_range(pattern) {
        (lo_txt, Some(hi_txt)) => {
            let lo_ok = if lo_txt.is_empty() {
                true
            } else {
                match dt_instant(lo_txt, default_offset_secs) {
                    Some(lo) => stored_i >= lo,
                    None => return false,
                }
            };
            let hi_ok = if hi_txt.is_empty() {
                true
            } else {
                match dt_upper_instant(hi_txt, default_offset_secs) {
                    Some(hi) => stored_i <= hi,
                    None => return false,
                }
            };
            lo_ok && hi_ok
        }
        // One value names a PERIOD, not its lowest instant: `20261115` matches
        // any instant during that day, and a full-precision value's period is
        // one second, which keeps exact equality what a single value means.
        (_, None) => match (
            dt_instant(pattern, default_offset_secs),
            dt_upper_instant(pattern, default_offset_secs),
        ) {
            (Some(lo), Some(hi)) => stored_i >= lo && stored_i <= hi,
            _ => false,
        },
    }
}

/// Split a DT range value into its endpoints.
///
/// A DT value may itself begin with an offset-free date, so the range separator
/// cannot be found by looking for the FIRST `-`. The endpoints are separated by
/// a `-` that follows a COMPLETE datetime, which is what `split(_once)` on
/// `-` cannot express: `20261115120000-0500` is one instant in a negative zone,
/// while `20261115120000-20261116` is a range. The rule used here is the one
/// that makes both work: a `-` is a separator only when the text before it is a
/// non-empty prefix that is ITSELF a valid range endpoint AND the text before
/// the `-` contains no offset (an offset ends the value, so nothing may follow
/// it inside one endpoint).
fn split_dt_range(value: &str) -> (&str, Option<&str>) {
    // A DT value can never start with `-`: PS3.5 Table 6.2-1 begins with the
    // year YYYY, four digits (standard/ps3.5/dt-vr.json
    // `leadingMinusIsRangeSeparator`). A leading `-` is therefore ALWAYS the
    // range separator - the open lower bound of an "up to hi" range - never a
    // value and never a negative offset. The loop below deliberately skips
    // index 0 (a bare offset must not be misread), so this case is handled
    // before it.
    if let Some(hi) = value.strip_prefix('-') {
        return ("", Some(hi));
    }
    // Scan candidate separators left to right; the first one whose prefix
    // parses as an instant without consuming an offset is the separator.
    let b = value.as_bytes();
    for (i, c) in value.char_indices() {
        if c != '-' || i == 0 {
            continue;
        }
        // A trailing `-HHMM` is an offset sign, not a separator (PS3.5
        // Table 6.2-1: the zone `&ZZXX` is the LAST component of a DT value
        // and nothing follows it inside one endpoint). The offset suffix is
        // NOT a component, so it may follow a trailing-null date-time - a
        // date-only or hour-only prefix can carry a zone
        // (standard/ps3.5/dt-vr.json `offsetSuffixNotAComponent`).
        //
        // The disambiguation between "one instant with a negative zone" and a
        // range is the offset RANGE (PS3.5 6.2 Note 1: -1200..+1400): a `-`
        // followed by exactly four trailing digits is the offset sign only
        // when those digits form a VALID negative offset (HHMM <= 1200).
        // `-0500` is valid -> one instant; `-2027` (20:27) is out of range ->
        // the year range `2026-2027` stays a range. `-0000` parses as offset 0
        // here, consistent with `parse_dt`.
        if i + 5 == b.len()
            && b[i + 1..].iter().all(|c| c.is_ascii_digit())
            && neg_offset_magnitude_valid(&b[i + 1..])
        {
            continue;
        }
        let (lo, hi) = (&value[..i], &value[i + 1..]);
        if dt_instant(lo, 0).is_some() && (hi.is_empty() || dt_instant(hi, 0).is_some()) {
            return (lo, Some(hi));
        }
    }
    (value, None)
}

/// Whether a four-digit `HHMM` run is a valid NEGATIVE UTC offset magnitude.
///
/// PS3.5 6.2 Note 1 bounds the offset to -1200..+1400, so a negative offset's
/// magnitude must not exceed 1200. This is the one fact that tells
/// `20261116-0500` (offset -05:00, valid) from `2026-2027` (year range: 20:27
/// is not an offset).
fn neg_offset_magnitude_valid(digits: &[u8]) -> bool {
    if digits.len() != 4 {
        return false;
    }
    let Ok(s) = std::str::from_utf8(digits) else {
        return false;
    };
    let Ok(n) = s.parse::<u16>() else {
        return false;
    };
    // The SAME bound `parse_dt` applies - including the minutes field, which
    // this used to ignore entirely (`-1299` was accepted as -12:99).
    offset_in_range_signed(false, (n / 100) as i64, (n % 100) as i64)
}

/// Whether `HHMM` names an offset PS3.5 6.2 permits: `-1200` through `+1400`.
///
/// The bound is asymmetric on purpose (an offset may be an hour further east
/// of UTC than west), and the minutes field is a clock field. This is the ONE
/// place the rule lives: `parse_dt` applies it to a value's own zone, and
/// `split_dt_range` applies it to decide whether a trailing `-HHMM` is an
/// offset sign or the start of a range endpoint. Two copies of the bound drift,
/// and a drift here silently re-reads `20261116-0500` as the range
/// `20261116`..`0500`.
///
/// `positive` is the sign: PS3.5 6.2 Note 1 bounds the offset VALUE, so
/// `-1201` is out of range (past 12:00 west) while `-1200` is in it, and
/// `+1400` is in while `+1401` is out. A single symmetric window gets one of
/// those two edges wrong.
fn offset_in_range_signed(positive: bool, hours: i64, minutes: i64) -> bool {
    if minutes > 59 {
        return false;
    }
    let (max_hours, max_minutes) = if positive { (14, 0) } else { (12, 0) };
    hours < max_hours || (hours == max_hours && minutes <= max_minutes)
}

/// Build `{path}.tmp` - the sibling used by the atomic rename.
/// Reserve a temp sibling for `path` that no other writer can have selected.
///
/// The name carries pid + a process-wide counter, which makes a collision
/// RARE but not impossible: PID namespaces let two containers share a pid, and
/// the counter restarts at 0 after a restart, so two writers can compute the
/// same name. `std::fs::write` would then open that name and REPLACE its
/// contents - one writer would publish the other's bytes. `create_new` turns
/// the collision into an error, and the retry moves to the next sequence.
fn tmp_sibling(path: &Path) -> Result<PathBuf, StoreError> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Bounded: each collision consumes a fresh sequence, so this only runs
    // long on a pathological directory and must not spin forever.
    const ATTEMPTS: usize = 64;
    let mut last = None;
    for _ in 0..ATTEMPTS {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let tmp = PathBuf::from(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            // The handle is dropped so the rename/hard_link sees a closed
            // file (Windows will not link a file open for writing).
            Ok(_) => return Ok(tmp),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(StoreError::Io(e)),
        }
    }
    Err(StoreError::Io(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "no free temp name")
    })))
}

/// Fill a temp file that [`tmp_sibling`] already created exclusively.
///
/// The bytes are flushed (`sync_all`) BEFORE the caller links/renames the file
/// into place, so a successful publish means the data is on the medium rather
/// than merely scheduled to be: a UPS N-CREATE / N-ACTION that answered 2xx
/// while the bytes sat in the page cache could be empty after a crash, and the
/// retry then fails while the performed step is lost. That is the same barrier
/// order the commitment store uses.
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
fn sync_parent_entry(path: &Path) -> Result<(), StoreError> {
    let parent = match crate::durable::parent_entry_dir(path) {
        Some(p) => p,
        None => return Ok(()),
    };
    crate::durable::sync_dir_entry(parent).map_err(StoreError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::syncs_observed;

    fn sample(uid: &str, state: &str, created: &str) -> WorkItemRecord {
        WorkItemRecord {
            id: None,
            workitem_uid: uid.to_string(),
            workflow_state: state.to_string(),
            station_aet: Some("STATION01".to_string()),
            patient_id: Some("PAT1".to_string()),
            step_label: Some("CAD".to_string()),
            transaction_uid: Some(format!("tuid-{uid}")),
            created_at: Some(created.to_string()),
            ..Default::default()
        }
    }

    /// A store rooted in a `TempDir` that the CALLER keeps alive.
    ///
    /// Returning only the store leaked the directory: `TempDir`'s `Drop` ran
    /// before any test touched the store, so the first insert recreated the
    /// path unowned and every run left workitem files and temp siblings behind
    /// in the system temp directory. The fixture keeps the owner in the test
    /// scope, so the data is removed when the test ends.
    struct StoreFixture {
        store: WorkItemStore,
        // Held for its `Drop`, never read: dropping it deletes the tree.
        _dir: tempfile::TempDir,
    }

    impl std::ops::Deref for StoreFixture {
        type Target = WorkItemStore;
        fn deref(&self) -> &WorkItemStore {
            &self.store
        }
    }

    fn store() -> StoreFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = WorkItemStore::new(dir.path().join("workitems"));
        StoreFixture { store, _dir: dir }
    }

    /// An occupied temp name must be SKIPPED, never overwritten.
    ///
    /// pid + counter makes a collision rare, not impossible: PID namespaces let
    /// two containers share a pid, and the counter restarts at 0 after a
    /// restart. `std::fs::write` on a shared name replaces its contents, so one
    /// writer would publish the other's bytes. The occupied names are placed in
    /// the allocator's own sequence window, since allocator-built names advance
    /// the counter and cannot be pre-created from outside.
    #[test]
    fn an_occupied_temp_name_is_skipped_not_overwritten() {
        let s = store();
        let target = s.path_for("1.2.3.99").expect("valid uid");
        std::fs::create_dir_all(s.root()).expect("root");

        // Learn the current sequence from one allocation, then occupy a window
        // starting there.
        let probe = tmp_sibling(&target).expect("probe");
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

        let fresh = tmp_sibling(&target).expect("a free name must be found");
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
    fn insert_find_delete_roundtrip() {
        let s = store();
        let rec = sample("1.2.3", "SCHEDULED", "2026-01-01 00:00:00");
        s.insert(&rec).expect("insert");
        let got = s.find_by_uid("1.2.3").expect("find").expect("some");
        assert_eq!(got, rec);

        assert!(s.delete("1.2.3").expect("delete"));
        assert!(s.find_by_uid("1.2.3").expect("find").is_none());
        assert!(!s.delete("1.2.3").expect("delete-again"));
    }

    #[test]
    fn insert_duplicate_conflicts() {
        let s = store();
        let rec = sample("1.2.3", "SCHEDULED", "2026-01-01");
        s.insert(&rec).expect("insert");
        assert!(matches!(s.insert(&rec), Err(StoreError::CasConflict(_))));
    }

    /// A UID that is not one conforming path component is refused at the store
    /// boundary, not joined. `path_for` is a public API, so a caller that
    /// forgets its own validation must get an error - and no file may be
    /// created inside or outside the root through a foreign component.
    #[test]
    fn a_non_path_uid_is_rejected_at_the_store() {
        let s = store();
        for uid in ["..", "1.2..3", "/etc/x"] {
            assert!(
                matches!(s.find_by_uid(uid), Err(StoreError::InvalidUid(_))),
                "{uid} must be refused, not joined"
            );
            assert!(
                matches!(s.delete(uid), Err(StoreError::InvalidUid(_))),
                "{uid} must be refused, not joined"
            );
        }
        assert!(
            !s.root().exists() || std::fs::read_dir(s.root()).expect("root").next().is_none(),
            "no file may be created for a refused UID"
        );
    }

    #[test]
    fn cas_update_honours_transaction_uid() {
        let s = store();
        let rec = sample("1.2.3", "SCHEDULED", "2026-01-01");
        s.insert(&rec).expect("insert");

        // Correct TUid -> update applies.
        let expected = rec.transaction_uid.clone().unwrap();
        let mut updated = rec.clone();
        updated.workflow_state = "IN PROGRESS".to_string();
        s.cas_update(&updated, Some(&expected), Some(0))
            .expect("cas ok");
        assert_eq!(
            s.find_by_uid("1.2.3")
                .expect("find")
                .unwrap()
                .workflow_state,
            "IN PROGRESS"
        );

        // Wrong TUid -> conflict.
        let on_disk = s.find_by_uid("1.2.3").expect("find").unwrap().revision;
        let mut again = updated.clone();
        again.workflow_state = "COMPLETED".to_string();
        assert!(matches!(
            s.cas_update(&again, Some("WRONG"), Some(on_disk)),
            Err(StoreError::CasConflict(_))
        ));
    }

    #[test]
    fn search_filters_and_sorts() {
        let s = store();
        s.insert(&sample("1.2.1", "SCHEDULED", "2026-01-01"))
            .expect("1");
        s.insert(&sample("1.2.2", "SCHEDULED", "2026-01-02"))
            .expect("2");
        s.insert(&sample("1.2.3", "COMPLETED", "2026-01-03"))
            .expect("3");

        // No filter -> all, sorted created DESC.
        let (all, _) = s.search(&StoreFilter::default()).expect("all");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].workitem_uid, "1.2.3"); // newest first

        // workflow_state filter.
        let f = StoreFilter {
            workflow_state: Some("SCHEDULED"),
            ..Default::default()
        };
        let (r, _) = s.search(&f).expect("state");
        assert_eq!(r.len(), 2);

        // limit + offset.
        let f = StoreFilter {
            limit: Some(1),
            offset: Some(1),
            ..Default::default()
        };
        let (r, _) = s.search(&f).expect("page");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].workitem_uid, "1.2.2");
    }

    /// (0040,A370 > 0008,0050) Accession Number is a matching key
    /// (PS3.4 Table CC.2.5-3) and behaves like its sibling string keys.
    #[test]
    fn search_by_accession_number() {
        let s = store();
        for (uid, acc, study) in [
            ("1.2.1", Some("ACC-1"), Some("1.2.840.1")),
            ("1.2.2", Some("ACC-2"), Some("1.2.840.1")),
            ("1.2.3", None, Some("1.2.840.2")),
        ] {
            let mut rec = sample(uid, "SCHEDULED", "2026-01-01");
            rec.accession_number = acc.map(str::to_string);
            rec.input_study_uid = study.map(str::to_string);
            s.insert(&rec).expect("insert");
        }

        let f = StoreFilter {
            accession_number: Some("ACC-1"),
            ..Default::default()
        };
        let (r, _) = s.search(&f).expect("accession");
        assert_eq!(r.len(), 1, "only the workitem carrying the value matches");
        assert_eq!(r[0].workitem_uid, "1.2.1");

        // A workitem with no accession number does not match a value query
        // (None != Some), which is right - it states no accession to match.
        let f = StoreFilter {
            accession_number: Some("ACC-3"),
            ..Default::default()
        };
        assert!(s.search(&f).expect("miss").0.is_empty());

        // Combined with the study key: AND semantics, as for every other pair.
        let f = StoreFilter {
            accession_number: Some("ACC-1"),
            study_uid: Some("1.2.840.2"),
            ..Default::default()
        };
        assert!(
            s.search(&f).expect("mismatched pair").0.is_empty(),
            "both keys must hold, so ACC-1 in the other study matches nothing"
        );

        let f = StoreFilter {
            accession_number: Some("ACC-1"),
            study_uid: Some("1.2.840.1"),
            ..Default::default()
        };
        assert_eq!(s.search(&f).expect("matching pair").0.len(), 1);
    }

    /// (0040,4018 > 0008,0100) Scheduled Workitem Code Value is a REQUIRED
    /// matching key (PS3.4 Table CC.2.5-3), so it filters on the code the
    /// record stores - it is not an accepted no-op.
    #[test]
    fn search_by_scheduled_workitem_code_value() {
        let s = store();
        for (uid, code) in [
            ("1.2.1", Some("CT-ORGAN-SEG")),
            ("1.2.2", Some("MR-SEG")),
            ("1.2.3", None),
        ] {
            let mut rec = sample(uid, "SCHEDULED", "2026-01-01");
            rec.scheduled_workitem_code = code.map(|c| crate::ups::record::WorkItemCode {
                value: c.to_string(),
                scheme: "99CLARUS".to_string(),
                meaning: "step".to_string(),
            });
            s.insert(&rec).expect("insert");
        }

        let f = StoreFilter {
            scheduled_workitem_code_value: Some("CT-ORGAN-SEG"),
            ..Default::default()
        };
        let (r, _) = s.search(&f).expect("code");
        assert_eq!(r.len(), 1, "only the workitem carrying that code matches");
        assert_eq!(r[0].workitem_uid, "1.2.1");

        // A workitem with no scheduled code states none, so it does not match
        // a value query (None != Some).
        let f = StoreFilter {
            scheduled_workitem_code_value: Some("NOPE"),
            ..Default::default()
        };
        assert!(s.search(&f).expect("miss").0.is_empty());
    }

    /// (0040,A370 > 0040,1001) Requested Procedure ID is a REQUIRED matching
    /// key of CC.2.5-3, stored on the record and filtered exactly.
    #[test]
    fn search_by_requested_procedure_id() {
        let s = store();
        for (uid, rp) in [
            ("1.2.1", Some("RP-1")),
            ("1.2.2", Some("RP-2")),
            ("1.2.3", None),
        ] {
            let mut rec = sample(uid, "SCHEDULED", "2026-01-01");
            rec.requested_procedure_id = rp.map(str::to_string);
            s.insert(&rec).expect("insert");
        }

        let f = StoreFilter {
            requested_procedure_id: Some("RP-1"),
            ..Default::default()
        };
        let (r, _) = s.search(&f).expect("rp");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].workitem_uid, "1.2.1");

        let f = StoreFilter {
            requested_procedure_id: Some("RP-9"),
            ..Default::default()
        };
        assert!(s.search(&f).expect("miss").0.is_empty());
    }

    /// (0040,4041) Input Readiness State is a matching key of PS3.4 Table
    /// CC.2.5-3: a `READY` query must exclude an INCOMPLETE record, and a
    /// record that never stated a readiness is not READY either.
    #[test]
    fn search_by_input_readiness_state() {
        let s = store();
        for (uid, readiness) in [
            ("1.2.1", Some("READY")),
            ("1.2.2", Some("INCOMPLETE")),
            ("1.2.3", None),
        ] {
            let mut rec = sample(uid, "SCHEDULED", "2026-01-01");
            rec.input_readiness = readiness.map(str::to_string);
            s.insert(&rec).expect("insert");
        }

        let f = StoreFilter {
            input_readiness: Some("READY"),
            ..Default::default()
        };
        let (r, _) = s.search(&f).expect("ready");
        assert_eq!(r.len(), 1, "only the READY record matches: {r:?}");
        assert_eq!(r[0].workitem_uid, "1.2.1");

        let f = StoreFilter {
            input_readiness: Some("INCOMPLETE"),
            ..Default::default()
        };
        let (r, _) = s.search(&f).expect("incomplete");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].workitem_uid, "1.2.2");

        // A DICOM CS is case-insensitive on the wire.
        let f = StoreFilter {
            input_readiness: Some("ready"),
            ..Default::default()
        };
        assert_eq!(s.search(&f).expect("lowercase").0.len(), 1);
    }

    /// A DT with a UTC offset must match a range naming the SAME instant.
    ///
    /// The offset is part of the DT value, so `20261115120000+0100` and
    /// `20261115110000+0000` are one moment written two ways. Comparing the
    /// strings ranks the first AFTER the second, so an inclusive range over the
    /// second excluded the first - a workitem that was inside the window the
    /// caller asked for.
    #[test]
    fn a_dt_with_an_offset_matches_an_equal_instant_range() {
        // Local noon in +0100 IS 11:00 UTC.
        let rec = WorkItemRecord {
            sps_start_datetime: Some("20261115120000+0100".to_string()),
            ..Default::default()
        };

        // The same instant, written in UTC, as a single-value range endpoint.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115110000+0000-20261115110000+0000"),
            ..Default::default()
        };
        assert!(
            matches_filter(&rec, &f),
            "an equal instant must fall inside its own inclusive range"
        );

        // And the case the lexical comparison got wrong: the range endpoint is
        // a bare local time with no zone.
        let f2 = StoreFilter {
            sps_start_datetime: Some("20261115110000-20261115110000"),
            ..Default::default()
        };
        assert!(
            matches_filter(&rec, &f2),
            "noon +0100 is 11:00 UTC and must match an 11:00 Z range"
        );

        // Outside the window still does not match.
        let f3 = StoreFilter {
            sps_start_datetime: Some("20261115130000+0000-20261115140000+0000"),
            ..Default::default()
        };
        assert!(!matches_filter(&rec, &f3));
    }

    /// A negative offset's `-` must not be read as the range separator.
    ///
    /// `20261115120000-0500` is ONE instant five hours behind UTC. Splitting on
    /// the first `-` turned it into a range from a bare date to `0500`, which
    /// would match a value the caller never named.
    #[test]
    fn a_negative_offset_is_not_a_range_separator() {
        let rec = WorkItemRecord {
            sps_start_datetime: Some("20261115120000-0500".to_string()),
            ..Default::default()
        };
        // The same instant is 17:00 UTC.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115170000+0000"),
            ..Default::default()
        };
        assert!(matches_filter(&rec, &f), "12:00 at -0500 is 17:00 UTC");

        // An unrelated instant must not match through a mis-split range.
        let other = StoreFilter {
            sps_start_datetime: Some("20260101000000+0000"),
            ..Default::default()
        };
        assert!(!matches_filter(&rec, &other));
    }

    /// A negative offset in the QUERY (not only in the stored value) is one
    /// instant, not a range.
    ///
    /// The run-1 test above covered the stored side only. `dt_instant` reads a
    /// 4-digit suffix as a year, so the old separator rule split a query
    /// `20261115120000-0500` into `[20261115120000, 0500]` and the search
    /// answered empty for its own value. The query side must recognise the
    /// trailing `-HHMM` as the zone and compare the whole value as an instant.
    #[test]
    fn a_negative_offset_in_the_query_is_not_a_range() {
        let s = store();
        let mut rec = sample("1.2.840.3", "SCHEDULED", "2026-01-01");
        rec.sps_start_datetime = Some("20261115120000-0500".to_string());
        s.insert(&rec).expect("insert");

        // The exact value as a query must match itself.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115120000-0500"),
            ..Default::default()
        };
        assert_eq!(s.search(&f).expect("exact").0.len(), 1);

        // The same instant written in UTC must match too.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115170000+0000"),
            ..Default::default()
        };
        assert_eq!(s.search(&f).expect("utc").0.len(), 1);

        // A full-datetime range still matches a value inside it.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115120000-20261116120000"),
            ..Default::default()
        };
        assert_eq!(s.search(&f).expect("full range").0.len(), 1);

        // A date range still behaves as a range: the stored value sits inside
        // 2026-11-15, so it matches.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115-20261116"),
            ..Default::default()
        };
        assert_eq!(s.search(&f).expect("date range").0.len(), 1);
    }

    /// A leading `-` is the open lower bound of a DT range, not a value.
    ///
    /// `-20261116` is the conformant UPS-RS spelling of "Scheduled Procedure
    /// Step Start DateTime up to 20261116". The first component of a DT value
    /// is the year YYYY (four digits), so a value never begins with `-`; the
    /// splitter must return an empty lower endpoint, which the range arm
    /// already treats as open. Before the fix the loop skipped index 0, the
    /// value was read as a single instant, `dt_instant` rejected the leading
    /// `-`, and the query matched nothing.
    #[test]
    fn a_leading_dash_is_an_open_lower_bound() {
        let s = store();
        let mut before_rec = sample("1.2.840.1", "SCHEDULED", "2026-01-01");
        before_rec.sps_start_datetime = Some("20261115120000".to_string());
        s.insert(&before_rec).expect("insert before");

        let mut after_rec = sample("1.2.840.2", "SCHEDULED", "2026-01-01");
        after_rec.sps_start_datetime = Some("20261117000000".to_string());
        s.insert(&after_rec).expect("insert after");

        let f = StoreFilter {
            sps_start_datetime: Some("-20261116"),
            ..Default::default()
        };
        let (hits, _) = s.search(&f).expect("search");
        assert_eq!(hits.len(), 1, "only the instant before the bound matches");
        assert_eq!(hits[0].workitem_uid, "1.2.840.1");
    }

    /// A short DT at the UPPER bound or as a single value covers the whole
    /// period it names, not just its first instant.
    ///
    /// PS3.5 Table 6.2-1: trailing null components mean the value is "not
    /// precise to the precision of those components", so `20261116` names all
    /// of 2026-11-16. Before the fix the upper bound `-20261116` and a
    /// single-value `20261116` both read as 2026-11-16 00:00:00, so a workitem
    /// at 08:00 that day was missed and a conformant worklist query answered
    /// empty.
    #[test]
    fn a_short_dt_matches_the_whole_period_it_names() {
        let s = store();
        let mut in_day = sample("1.2.840.5", "SCHEDULED", "2026-01-01");
        in_day.sps_start_datetime = Some("20261116080000".to_string());
        s.insert(&in_day).expect("insert in day");

        let mut next_day = sample("1.2.840.6", "SCHEDULED", "2026-01-01");
        next_day.sps_start_datetime = Some("20261117000000".to_string());
        s.insert(&next_day).expect("insert next day");

        // Open-lower-bound range: the upper bound covers the whole day.
        let f = StoreFilter {
            sps_start_datetime: Some("-20261116"),
            ..Default::default()
        };
        let (hits, _) = s.search(&f).expect("up-to");
        assert_eq!(hits.len(), 1, "08:00 on the 16th is inside `-20261116`");
        assert_eq!(hits[0].workitem_uid, "1.2.840.5");

        // Single value: a day covers any stored instant during that day.
        let f = StoreFilter {
            sps_start_datetime: Some("20261116"),
            ..Default::default()
        };
        let (hits, _) = s.search(&f).expect("single");
        assert_eq!(hits.len(), 1, "08:00 on the 16th is inside `20261116`");
        assert_eq!(hits[0].workitem_uid, "1.2.840.5");
    }

    /// A day-precision DT with a POSITIVE offset is a valid value and names the
    /// whole day in that zone, not nothing.
    ///
    /// PS3.5 Table 6.2-1: the offset suffix is not a component, so it may
    /// follow a trailing-null date-time
    /// (standard/ps3.5/dt-vr.json `offsetSuffixNotAComponent`). Before the fix
    /// the component reads used the whole byte length, so `20261116+0100`
    /// failed to parse and a single-value query matched nothing.
    #[test]
    fn a_short_dt_with_a_positive_offset_matches_the_period_it_names() {
        let s = store();
        let mut in_day = sample("1.2.840.7", "SCHEDULED", "2026-01-01");
        in_day.sps_start_datetime = Some("20261116120000+0100".to_string());
        s.insert(&in_day).expect("insert in day");

        let mut next_day = sample("1.2.840.8", "SCHEDULED", "2026-01-01");
        next_day.sps_start_datetime = Some("20261117000000+0100".to_string());
        s.insert(&next_day).expect("insert next day");

        let f = StoreFilter {
            sps_start_datetime: Some("20261116+0100"),
            ..Default::default()
        };
        let (hits, _) = s.search(&f).expect("search");
        assert_eq!(
            hits.len(),
            1,
            "noon on the 16th +0100 is inside `20261116+0100`"
        );
        assert_eq!(hits[0].workitem_uid, "1.2.840.7");
    }

    /// An open-lower-bound upper endpoint with a positive offset covers the
    /// whole day in that zone, so the day is not silently unbounded.
    ///
    /// Before the fix `dt_upper_instant("20261116+0100")` was `None`, and the
    /// range arm read that as "no upper limit" - the query matched EVERYTHING,
    /// including the next day.
    #[test]
    fn an_upper_bound_with_a_positive_offset_covers_the_whole_day() {
        let s = store();
        let mut in_day = sample("1.2.840.9", "SCHEDULED", "2026-01-01");
        in_day.sps_start_datetime = Some("20261116080000+0100".to_string());
        s.insert(&in_day).expect("insert in day");

        let mut next_day = sample("1.2.840.10", "SCHEDULED", "2026-01-01");
        next_day.sps_start_datetime = Some("20261117000000+0100".to_string());
        s.insert(&next_day).expect("insert next day");

        let f = StoreFilter {
            sps_start_datetime: Some("-20261116+0100"),
            ..Default::default()
        };
        let (hits, _) = s.search(&f).expect("search");
        assert_eq!(
            hits.len(),
            1,
            "08:00 on the 16th +0100 is inside `-20261116+0100`"
        );
        assert_eq!(hits[0].workitem_uid, "1.2.840.9");
    }

    /// An hour-precision DT with a positive offset covers the whole hour in
    /// that zone, not just its first instant.
    ///
    /// `2026111612+0100` is valid (the offset suffix follows the trailing-null
    /// minute/second components) and names 12:00..12:59 in +0100.
    #[test]
    fn an_hour_precision_dt_with_an_offset_covers_the_whole_hour() {
        let s = store();
        let mut in_hour = sample("1.2.840.11", "SCHEDULED", "2026-01-01");
        in_hour.sps_start_datetime = Some("20261116123000+0100".to_string());
        s.insert(&in_hour).expect("insert in hour");

        let mut next_hour = sample("1.2.840.12", "SCHEDULED", "2026-01-01");
        next_hour.sps_start_datetime = Some("20261116130000+0100".to_string());
        s.insert(&next_hour).expect("insert next hour");

        let f = StoreFilter {
            sps_start_datetime: Some("2026111612+0100"),
            ..Default::default()
        };
        let (hits, _) = s.search(&f).expect("search");
        assert_eq!(hits.len(), 1, "12:30 +0100 is inside `2026111612+0100`");
        assert_eq!(hits[0].workitem_uid, "1.2.840.11");
    }

    /// The range splitter keeps the batch-11 guard behavior on every form.
    ///
    /// A negative offset is ONE instant (the `-` is the zone sign), a full
    /// datetime pair is a range, and a trailing dash is an open upper end.
    /// The new leading-dash arm must not disturb any of these.
    #[test]
    fn split_dt_range_reads_every_endpoint_form() {
        assert_eq!(split_dt_range("-20261116"), ("", Some("20261116")));
        assert_eq!(
            split_dt_range("20261115120000-0500"),
            ("20261115120000-0500", None)
        );
        assert_eq!(
            split_dt_range("20261115120000-20261116"),
            ("20261115120000", Some("20261116"))
        );
        assert_eq!(split_dt_range("20261116-"), ("20261116", Some("")));
    }

    /// A negative offset suffix is valid after EVERY DT precision, not only a
    /// full `YYYYMMDDHHMMSS`.
    ///
    /// PS3.5 6.2: the offset suffix is not a component, so a trailing-null
    /// date-time may carry one (`offsetSuffixNotAComponent`). The old guard
    /// fired only at byte index >= 14, so `20261116-0500` (day precision) and
    /// `2026111612-0100` (hour precision) were mis-split as ranges and a
    /// conformant query answered an empty worklist. The offset RANGE
    /// (-1200..+1400) is what keeps `2026-2027` a year range: `-2027` is not
    /// an offset.
    #[test]
    fn a_negative_offset_follows_every_precision_but_a_year_range_stays_a_range() {
        assert_eq!(split_dt_range("20261116-0500"), ("20261116-0500", None));
        assert_eq!(split_dt_range("2026111612-0100"), ("2026111612-0100", None));
        assert_eq!(split_dt_range("2026-2027"), ("2026", Some("2027")));
    }

    /// PS3.5 6.2 Note 1 bounds an offset to `-1200..+1400` with a real minutes
    /// field, and `parse_dt` applies the SAME rule as `split_dt_range`.
    ///
    /// Without it `20261115120000-0500-2027` split at the FIRST dash: the
    /// remainder `0500-2027` parsed as year 0500 at -20:27, so the range was
    /// nonsense and a December 2026 workitem between the real bounds fell out.
    #[test]
    fn an_out_of_range_offset_is_not_an_instant() {
        // Legal bounds are accepted.
        assert!(dt_instant("20261115120000-1200", 0).is_some());
        assert!(dt_instant("20261115120000+1400", 0).is_some());
        assert!(dt_instant("20261115120000+1300", 0).is_some());
        // Past the bound, or with a minutes field that is not a clock field.
        assert!(dt_instant("20261115120000+1401", 0).is_none());
        assert!(dt_instant("20261115120000-1201", 0).is_none());
        assert!(dt_instant("20261115120000+1260", 0).is_none());
        assert!(dt_instant("20261115120000+1299", 0).is_none());

        // The consequence: the second dash is the range separator, so the
        // lower bound keeps its valid `-0500` zone.
        assert_eq!(
            split_dt_range("20261115120000-0500-2027"),
            ("20261115120000-0500", Some("2027"))
        );
    }

    /// A range endpoint that does not parse rejects; it is not an open end.
    ///
    /// The parsed arm cannot see this: `split_dt_range` only returns a range
    /// after both endpoints parsed, so a bound there is always readable. The
    /// arm that CAN see it is the fallback, which runs when the STORED value
    /// uses a form this parser refuses (PS3.5 Table 6.2-1 admits 4/6/8/10/12/14
    /// digit prefixes; anything else is unreadable here). That arm filters the
    /// endpoints itself and used to skip a neighbour that failed:
    ///
    /// ```text
    /// (lo.is_empty() || stored >= lo) && (hi.is_empty() || stored <= hi)
    /// ```
    ///
    /// `split_dt_range` hands back `("", "-junk")` for `--junk` and
    /// `("2026", "-0x")` never - but it DOES hand back a two-endpoint range
    /// straight from the client string when both halves parse, and that range
    /// is re-tested against the stored text lexically. The hole is the reverse
    /// direction: the stored text is unreadable AND a bound is unreadable, and
    /// the old code answered "inside the range" instead of "not comparable".
    ///
    /// PS3.18 8.3.4.2.2.1 makes a value the service cannot use a client error;
    /// a query layer that cannot read a bound must never answer with more than
    /// it was asked for.
    #[test]
    fn an_unreadable_stored_value_still_honours_every_bound() {
        let s = store();
        let mut rec = sample("1.2.840.31", "SCHEDULED", "2026-01-01");
        // A 13-digit run: not a DT prefix `parse_dt` accepts, so the stored
        // value falls into the lexical fallback.
        rec.sps_start_datetime = Some("2026111612000".to_string());
        s.insert(&rec).expect("insert");

        // Lexically "2026111612000" < "20240101", so an upper bound of
        // 2024-01-01 excludes it.
        let f = StoreFilter {
            sps_start_datetime: Some("-20240101"),
            ..Default::default()
        };
        let (rows, _) = s.search(&f).expect("search");
        assert_eq!(rows.len(), 0, "the upper bound still excludes it");

        // Lexically "2026111612000" > "20270101", so a lower bound of
        // 2027-01-01 excludes it too.
        let f = StoreFilter {
            sps_start_datetime: Some("20270101-"),
            ..Default::default()
        };
        let (rows, _) = s.search(&f).expect("search");
        assert_eq!(rows.len(), 0, "the lower bound still excludes it");

        // A range that genuinely brackets it still matches: the fallback is a
        // narrower answer, not an empty one.
        let f = StoreFilter {
            sps_start_datetime: Some("20260101-20270101"),
            ..Default::default()
        };
        let (rows, _) = s.search(&f).expect("search");
        assert_eq!(rows.len(), 1, "a bracketing range still matches");
    }

    /// A present-but-unreadable range endpoint rejects; it is not an open end.
    ///
    /// `split_dt_range` hands back a range for a leading dash whatever follows
    /// it: `"-2026111"` yields `("", Some("2026111"))`, and a 7-digit run is
    /// not a DT prefix PS3.5 Table 6.2-1 admits, so `dt_instant` refuses it.
    /// `.is_none_or(...)` collapsed that refusal into "no upper bound", so the
    /// query matched every workitem regardless of when it starts - strictly
    /// WIDER than the ask, and the difference between an empty worklist and
    /// somebody else's.
    ///
    /// PS3.18 8.3.4.2.2.1 makes a value the service cannot use a client error,
    /// and a query layer that cannot read a bound must never answer with more
    /// than it was asked for. Only an EMPTY endpoint is an open end.
    #[test]
    fn an_unreadable_range_endpoint_does_not_widen_the_range() {
        let s = store();
        let mut inside = sample("1.2.840.30", "SCHEDULED", "2026-01-01");
        inside.sps_start_datetime = Some("20260115000000".to_string());
        s.insert(&inside).expect("insert inside");

        // `-2026111`: open lower bound, upper endpoint unreadable.
        let f = StoreFilter {
            sps_start_datetime: Some("-2026111"),
            ..Default::default()
        };
        let (rows, n) = s.search(&f).expect("search");
        assert_eq!(
            (rows.len(), n),
            (0, 0),
            "an unreadable upper bound must not open the range"
        );

        // A readable range still matches: the guard rejects only what it
        // cannot read.
        let f = StoreFilter {
            sps_start_datetime: Some("20260101-20260131"),
            ..Default::default()
        };
        let (rows, _) = s.search(&f).expect("search");
        assert_eq!(rows.len(), 1, "a readable range still matches");
    }

    /// The same rule in the fallback arm, reached when the STORED value is the
    /// one this parser cannot read.
    ///
    /// A 13-digit run is not a DT prefix PS3.5 Table 6.2-1 admits, so the
    /// stored text is compared lexically - the only ordering available for
    /// something that cannot be placed in time. That arm filtered its own
    /// endpoints and skipped an unreadable one, so an unreadable bound widened
    /// the range exactly as in the parsed arm. Lexical comparison is right;
    /// dropping a bound is not.
    #[test]
    fn an_unreadable_bound_still_rejects_an_unreadable_stored_value() {
        let s = store();
        let mut rec = sample("1.2.840.31", "SCHEDULED", "2026-01-01");
        rec.sps_start_datetime = Some("2026111612000".to_string());
        s.insert(&rec).expect("insert");

        let f = StoreFilter {
            sps_start_datetime: Some("-2026111"),
            ..Default::default()
        };
        let (rows, n) = s.search(&f).expect("search");
        assert_eq!(
            (rows.len(), n),
            (0, 0),
            "an unreadable upper bound must not open the range"
        );

        // The lexical ordering alone would have ACCEPTED this one: the stored
        // text sorts before the bound. The unreadable bound is what rejects
        // it, which is the whole point - a bound this code cannot read says
        // nothing about where the stored value sits.
        let mut low = sample("1.2.840.32", "SCHEDULED", "2026-01-01");
        low.sps_start_datetime = Some("!unreadable".to_string());
        s.insert(&low).expect("insert");
        let round = s.find_by_uid("1.2.840.32").expect("find").expect("present");
        assert_eq!(
            round.sps_start_datetime.as_deref(),
            Some("!unreadable"),
            "the stored datetime must round-trip verbatim"
        );
        let (rows, n) = s.search(&f).expect("search");
        assert_eq!(
            (rows.len(), n),
            (0, 0),
            "the unreadable bound rejects regardless of lexical order"
        );
    }

    /// A trailing SPACE on a stored DT is encoding padding, not data.
    ///
    /// PS3.5 6.2 requires character-string VRs other than UI to be padded with
    /// SPACE when an odd length would otherwise result, and DT is one of them
    /// (standard/ps3.5/string-value-padding.json `spacePaddedVrs`). Without
    /// stripping it the value failed to parse, fell through to the lexical
    /// fallback, and stopped matching the same instant written in another
    /// zone - a workitem the client asked for answered "absent".
    #[test]
    fn a_trailing_space_on_a_stored_dt_does_not_change_its_instant() {
        let s = store();
        let mut rec = sample("1.2.840.40", "SCHEDULED", "2026-01-01");
        rec.sps_start_datetime = Some("20261115120000+0100 ".to_string());
        s.insert(&rec).expect("insert");

        // Same instant, written in UTC: 12:00+0100 is 11:00Z.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115110000+0000"),
            ..Default::default()
        };
        let (rows, _) = s.search(&f).expect("search");
        assert_eq!(rows.len(), 1, "the padded stored value still names 11:00Z");
        assert_eq!(rows[0].workitem_uid, "1.2.840.40");

        // A padded QUERY value is the same value: padding is the transport's,
        // not the client's intent.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115110000+0000 "),
            ..Default::default()
        };
        let (rows, _) = s.search(&f).expect("search");
        assert_eq!(rows.len(), 1, "a padded query still matches");
    }

    /// Deleting an ALREADY-absent workitem still runs the directory barrier.
    ///
    /// A previous delete can unlink the file and then fail the barrier; the
    /// retry finds nothing and used to return `Ok(false)` without flushing the
    /// directory. The retry then reported a completed deletion whose directory
    /// update was never made durable, and a crash could bring the workitem
    /// back. The barrier is therefore run on the `NotFound` path too.
    ///
    /// Unix-only because the barrier is: Windows cannot open a directory for
    /// `FlushFileBuffers`, so there is no barrier to assert there.
    #[cfg(unix)]
    #[test]
    fn deleting_an_absent_workitem_still_barriers_the_directory() {
        let s = store();
        let mut rec = sample("1.2.840.41", "SCHEDULED", "2026-01-01");
        rec.sps_start_datetime = Some("20261116120000".to_string());
        s.insert(&rec).expect("insert");

        // First delete removes the file and barriers.
        assert!(s.delete("1.2.840.41").expect("delete"), "the file existed");

        // Second delete finds nothing; it must still barrier, because the
        // unlink it is completing may be the one whose barrier failed.
        let (out, barriers) =
            crate::durable::count_barriers_in_scope(|| s.delete("1.2.840.41").expect("delete"));
        assert!(!out, "nothing was removed");
        assert_eq!(
            barriers, 1,
            "the absent-file path must run the same directory barrier"
        );
    }

    /// A day-precision query with a negative offset matches a stored instant
    /// inside that day's period in the same zone.
    #[test]
    fn a_day_precision_negative_offset_query_matches_its_period() {
        let s = store();
        let mut rec = sample("1.2.840.20", "SCHEDULED", "2026-01-01");
        rec.sps_start_datetime = Some("20261116120000-0500".to_string());
        s.insert(&rec).expect("insert");

        let f = StoreFilter {
            sps_start_datetime: Some("20261116-0500"),
            ..Default::default()
        };
        let (hits, _) = s.search(&f).expect("search");
        assert_eq!(
            hits.len(),
            1,
            "noon on the 16th -0500 is inside the day `20261116-0500`"
        );
        assert_eq!(hits[0].workitem_uid, "1.2.840.20");
    }

    /// Fractional seconds must be retained: a value at `.900000` is OUTSIDE a
    /// range `.100000-...200000` and INSIDE a whole-second single value.
    ///
    /// Dropping the fraction reduced all three to the whole second, so the
    /// stored value matched a range it sits outside (a false positive).
    #[test]
    fn a_fractional_second_is_retained_in_range_matching() {
        let s = store();
        let mut rec = sample("1.2.840.21", "SCHEDULED", "2026-01-01");
        rec.sps_start_datetime = Some("20261116120000.900000+0000".to_string());
        s.insert(&rec).expect("insert");

        // The range .100000..200000 excludes .900000.
        let f = StoreFilter {
            sps_start_datetime: Some("20261116120000.100000+0000-20261116120000.200000+0000"),
            ..Default::default()
        };
        assert!(
            s.search(&f).expect("range").0.is_empty(),
            ".900000 is outside .100000-.200000"
        );

        // A whole-second single value covers the whole second, so .900000 is
        // inside `20261116120000`.
        let f = StoreFilter {
            sps_start_datetime: Some("20261116120000"),
            ..Default::default()
        };
        assert_eq!(
            s.search(&f).expect("single").0.len(),
            1,
            ".900000 is inside the whole second `20261116120000`"
        );
    }

    /// A configured default offset applies ONLY to offset-free DT values.
    ///
    /// The `[]` arm of `parse_dt` is the one place "no zone" is decided: a
    /// full-precision value with an explicit `&ZZXX` keeps its own offset,
    /// while an offset-free value is compared in the configured local frame
    /// (default +0000 = UTC).
    #[test]
    fn a_configured_default_offset_applies_to_offset_free_values() {
        // Stored noon +0100 is 11:00 UTC.
        let rec = WorkItemRecord {
            sps_start_datetime: Some("20261115120000+0100".to_string()),
            ..Default::default()
        };

        // Offset-free 12:00 in the +0100 local frame == 12:00+0100 stored.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115120000-20261115120000"),
            default_offset_secs: 3600,
            ..Default::default()
        };
        assert!(
            matches_filter(&rec, &f),
            "offset-free 12:00 in +0100 is the stored instant"
        );

        // 11:00 local +0100 = 10:00 UTC, which is NOT the stored 11:00 UTC.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115110000"),
            default_offset_secs: 3600,
            ..Default::default()
        };
        assert!(!matches_filter(&rec, &f), "11:00+0100 != 11:00 UTC stored");

        // The shipped default (0 = UTC) preserves the existing behaviour: an
        // offset-free 11:00 range matches the stored 11:00 UTC.
        let f = StoreFilter {
            sps_start_datetime: Some("20261115110000-20261115110000"),
            ..Default::default()
        };
        assert!(
            matches_filter(&rec, &f),
            "UTC remains the default local frame"
        );
    }

    /// Two read-modify-write cycles on one record must not lose an update.
    ///
    /// The Transaction UID does not change on a successful update (PS3.18
    /// Table 11.6-1), so comparing only against it lets two cycles that read
    /// the SAME state both pass and the second overwrite the first. The
    /// revision makes the loser visible as a conflict instead.
    #[test]
    fn a_concurrent_update_cannot_silently_overwrite_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = WorkItemStore::new(dir.path().join("items"));

        let base = WorkItemRecord {
            workitem_uid: "1.2.840.1.5".to_string(),
            workflow_state: "SCHEDULED".to_string(),
            transaction_uid: Some("T-1".to_string()),
            step_label: Some("start".to_string()),
            ..Default::default()
        };
        s.insert(&base).expect("insert");

        // Bring the record past its initial revision so the stale read below
        // carries a real one - the state every real caller is in after the
        // first update.
        let seeded = s
            .find_by_uid(&base.workitem_uid)
            .expect("find")
            .expect("present");
        let mut seed = seeded;
        seed.step_label = Some("seed".to_string());
        s.cas_update(&seed, Some("T-1"), Some(seed.revision))
            .expect("seed update");

        // Both writers now read the SAME record - this is the interleaving.
        let read_a = s
            .find_by_uid(&base.workitem_uid)
            .expect("find")
            .expect("present");
        let read_b = s
            .find_by_uid(&base.workitem_uid)
            .expect("find")
            .expect("present");
        assert_eq!(read_a.revision, read_b.revision, "both read one state");

        // The first cycle merges its change and writes.
        let mut a = read_a;
        a.step_label = Some("first-change".to_string());
        s.cas_update(&a, Some("T-1"), Some(a.revision))
            .expect("first update wins");

        // The second cycle holds the SAME transaction UID, so the UID check
        // passes; its revision is now stale, which is what must stop it.
        let mut b = read_b;
        b.step_label = Some("second-change".to_string());
        let err = s.cas_update(&b, Some("T-1"), Some(b.revision)).unwrap_err();
        assert!(
            matches!(err, StoreError::CasConflict(_)),
            "a stale read must be reported as a conflict, got {err:?}"
        );

        // The first writer's change survived.
        let after = s
            .find_by_uid(&base.workitem_uid)
            .expect("find")
            .expect("present");
        assert_eq!(
            after.step_label.as_deref(),
            Some("first-change"),
            "the first change must not be lost"
        );
        assert!(after.revision > 0, "a write bumps the revision");
    }

    /// Revision 0 is an observation, not the absence of one.
    ///
    /// A freshly created record is at revision 0, so two callers that both read
    /// it present revision 0. While the check skipped zero, both passed and the
    /// second overwrote the first with no conflict - exactly the lost update
    /// the revision exists to prevent. No seed update here: this is the state a
    /// caller reaches by reading a record that has never been modified.
    #[test]
    fn a_revision_zero_read_is_still_a_precondition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = WorkItemStore::new(dir.path().join("items"));

        let base = WorkItemRecord {
            workitem_uid: "1.2.840.1.6".to_string(),
            workflow_state: "SCHEDULED".to_string(),
            transaction_uid: Some("T-0".to_string()),
            step_label: Some("start".to_string()),
            ..Default::default()
        };
        s.insert(&base).expect("insert");

        let read_a = s
            .find_by_uid(&base.workitem_uid)
            .expect("find")
            .expect("present");
        let read_b = read_a.clone();
        assert_eq!(read_a.revision, 0, "a created record is at revision 0");

        let mut a = read_a;
        a.step_label = Some("a".to_string());
        s.cas_update(&a, Some("T-0"), Some(0))
            .expect("the first revision-0 write applies");

        let mut b = read_b;
        b.step_label = Some("b".to_string());
        let err = s
            .cas_update(&b, Some("T-0"), Some(0))
            .expect_err("the second revision-0 write must lose");
        assert!(
            matches!(err, StoreError::CasConflict(_)),
            "a stale revision-0 read must conflict, got {err:?}"
        );

        let after = s
            .find_by_uid(&base.workitem_uid)
            .expect("find")
            .expect("present");
        assert_eq!(after.step_label.as_deref(), Some("a"), "the first wins");
    }

    /// The unconditional write path stays available and stays explicit.
    ///
    /// Callers that never read a record (the create path) still need to write
    /// without a precondition; naming that in the API keeps revision 0 free to
    /// mean "I observed revision 0".
    #[test]
    fn upsert_unconditional_ignores_the_revision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = WorkItemStore::new(dir.path().join("items"));

        let base = WorkItemRecord {
            workitem_uid: "1.2.840.1.7".to_string(),
            workflow_state: "SCHEDULED".to_string(),
            ..Default::default()
        };
        s.insert(&base).expect("insert");

        let mut w = base.clone();
        w.step_label = Some("blind".to_string());
        s.upsert_unconditional(&w).expect("unconditional write");

        let after = s
            .find_by_uid(&base.workitem_uid)
            .expect("find")
            .expect("present");
        assert_eq!(after.step_label.as_deref(), Some("blind"));
        assert_eq!(after.revision, 1, "the write still bumps the revision");
    }

    /// A committed write flushes the data to the medium BEFORE it is published.
    ///
    /// Observed on the barrier counter, because `sync_all` leaves no byte trace
    /// a re-read can see. Both public mutation paths are covered: `insert`
    /// (create-if-absent) and `cas_update` (rename).
    #[test]
    fn a_committed_write_flushes_before_it_publishes() {
        let s = store();
        let rec = sample("1.2.3", "SCHEDULED", "2026-01-01");

        let before = syncs_observed();
        s.insert(&rec).expect("insert");
        assert!(
            syncs_observed() > before,
            "a committed insert has to flush the record to the medium"
        );

        let mut updated = rec.clone();
        updated.workflow_state = "IN PROGRESS".to_string();
        let tuid = rec.transaction_uid.clone().expect("tuid");
        let before = syncs_observed();
        s.cas_update(&updated, Some(&tuid), Some(rec.revision))
            .expect("cas update");
        assert!(
            syncs_observed() > before,
            "a committed update has to flush the record to the medium"
        );
    }

    /// The directory entry that names a record is flushed too (Unix-only).
    #[cfg(unix)]
    #[test]
    fn a_committed_write_also_flushes_the_directory_entry() {
        let s = store();
        let rec = sample("1.2.4", "SCHEDULED", "2026-01-01");
        let before = syncs_observed();
        s.insert(&rec).expect("insert");
        assert!(
            syncs_observed() >= before + 2,
            "data barrier + directory barrier: expected at least 2, observed {}",
            syncs_observed() - before
        );
    }

    /// A successful delete persists the directory entry that named the record
    /// (Unix-only), so a crash after `remove_file` cannot restore the
    /// workitem. Observed on the barrier counter, the same proof the insert
    /// paths use.
    #[cfg(unix)]
    #[test]
    fn a_delete_persists_the_directory_entry() {
        let s = store();
        let rec = sample("1.2.4.1", "SCHEDULED", "2026-01-01");
        s.insert(&rec).expect("insert");

        let before = syncs_observed();
        assert!(s.delete("1.2.4.1").expect("delete"));
        assert!(
            syncs_observed() > before,
            "a successful delete must flush the directory entry"
        );
    }

    /// A barrier failure on the delete path is an error, not a stored
    /// "deleted" - the record was removed but its NAME may not survive a
    /// crash, so the caller must not be told the delete is durable. Reverting
    /// the added barrier makes `delete` return Ok(true) and this test fails.
    #[cfg(unix)]
    #[test]
    fn a_delete_with_a_failing_barrier_is_an_error_not_a_success() {
        let s = store();
        let rec = sample("1.2.4.2", "SCHEDULED", "2026-01-01");
        s.insert(&rec).expect("insert");

        crate::durable::fail_next_barrier_for_test(s.root());
        let err = s.delete("1.2.4.2").unwrap_err();
        assert!(matches!(err, StoreError::Io(_)), "got {err:?}");
        assert!(
            !s.path_for("1.2.4.2").expect("path").exists(),
            "the file was removed; the delete reported the barrier failure"
        );
    }

    /// A failed flush is a failed write, not a silent success.
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
    /// on the Ok arm AND on the CasConflict arm (the same fix the commitment
    /// store carries).
    ///
    /// The install order is temp-sync, hard_link, remove temp, THEN dir
    /// barrier. A barrier failure after the hard_link leaves the record at its
    /// target; a retry finds the target occupied and used to answer
    /// CasConflict WITHOUT re-running the barrier. The seam fails the NEXT
    /// barrier on the store root (workitems/), so the Ok arm surfaces Io and
    /// the CasConflict arm surfaces Io too. Reverting the CasConflict-arm
    /// barrier makes the second insert answer CasConflict (the seam is never
    /// consumed).
    #[cfg(unix)]
    #[test]
    fn a_failed_barrier_is_io_on_both_the_ok_and_cas_arms() {
        let s = store();
        let rec = sample("1.2.95", "SCHEDULED", "2026-01-01");

        // Ok arm: the link lands, then the barrier fails -> Io, target stays.
        crate::durable::fail_next_barrier_for_test(s.root());
        let err = s.insert(&rec).unwrap_err();
        assert!(matches!(err, StoreError::Io(_)), "got {err:?}");
        assert!(
            s.path_for(&rec.workitem_uid).expect("path").exists(),
            "the link landed before the barrier failed"
        );

        // CasConflict arm: the occupied target with the next barrier failing
        // must surface Io, not a silent CasConflict.
        crate::durable::fail_next_barrier_for_test(s.root());
        let err = s.insert(&rec).unwrap_err();
        assert!(
            matches!(err, StoreError::Io(_)),
            "a failing barrier on the conflict path is Io, got {err:?}"
        );

        // Healthy barrier: the occupied target is a CasConflict and the stored
        // record is left untouched.
        let first = s
            .find_by_uid(&rec.workitem_uid)
            .expect("find")
            .expect("present");
        assert!(matches!(s.insert(&rec), Err(StoreError::CasConflict(_))));
        let after = s
            .find_by_uid(&rec.workitem_uid)
            .expect("find")
            .expect("present");
        assert_eq!(
            after, first,
            "a refused conflict must not replace the record"
        );
    }
}
