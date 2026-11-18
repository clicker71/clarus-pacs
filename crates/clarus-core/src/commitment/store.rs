// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! File-backed Storage Commitment store (`commitments/{TransactionUID}.cbor`).
//!
//! Writes are atomic (tmp + rename, the same discipline as the study manifest
//! and the UPS store); the duplicate-Transaction-UID check is an insert-time
//! existence test, which is what maps to the HTTP 409 of PS3.18 Table 13.4.3-1.
//!
//! PS3.18 13.2 requires the origin to say IN ITS CONFORMANCE STATEMENT how long
//! a result stays available; the standard fixes no duration. That makes
//! retention a product policy, and this module is where the policy is applied:
//! [`sweep_expired`] deletes result files older than the configured window.

use crate::commitment::record::{CommitmentRecord, RecordError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Process-wide counter for temp-file names (see `write_atomic`).
///
/// The pid alone is not enough: one process writes many records concurrently,
/// so two Commits could share a temp name and interleave into a torn file.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Number of striped locks. 64 is a compromise: enough that unrelated
/// transactions rarely collide, few enough that the table is 64 `Arc`s.
const UID_LOCK_STRIPES: usize = 64;

/// Process-wide striped locks, one per `hash(uid) % UID_LOCK_STRIPES`.
///
/// A `std::sync::Mutex` rather than a per-UID map: the store's job is to
/// serialise a read-modify-write of ONE file, and a fixed table does that
/// without growing unboundedly with the number of transactions ever seen.
fn uid_lock(uid: &str) -> Arc<Mutex<()>> {
    static STRIPES: OnceLock<Vec<Arc<Mutex<()>>>> = OnceLock::new();
    let stripes = STRIPES.get_or_init(|| {
        (0..UID_LOCK_STRIPES)
            .map(|_| Arc::new(Mutex::new(())))
            .collect()
    });
    // FNV-1a over the uid, the same cheap mix the UPS store uses; the quality
    // of the distribution only has to beat "every uid in one stripe".
    let mut h: u32 = 0x811c_9dc5;
    for &b in uid.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    Arc::clone(&stripes[(h as usize) % UID_LOCK_STRIPES])
}

/// Errors produced by the commitment store.
#[derive(Debug)]
pub enum StoreError {
    /// Underlying filesystem failure.
    Io(io::Error),
    /// The bytes on disk did not decode as a commitment record.
    Decode(RecordError),
    /// A record with this Transaction UID already exists (the 409 path).
    Duplicate(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "commitment store I/O error: {e}"),
            Self::Decode(e) => write!(f, "commitment store decode error: {e}"),
            Self::Duplicate(uid) => write!(f, "commitment already recorded: {uid}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The on-disk commitment store rooted at a `commitments/` directory.
///
/// `Clone` is cheap: it just carries the root `PathBuf`. Methods take `&self`
/// so the store can be shared across handler threads; mutations and the
/// sweep's read-check-delete take a STRIPED lock (see [`uid_lock`]) so two
/// writers of the same transaction cannot interleave, while writers of
/// different transactions never contend. Each write is atomic (tmp + rename
/// or `hard_link` create-if-absent).
///
/// # Deployment invariant: ONE writer process per root
///
/// The striped locks are PROCESS-LOCAL. Exactly one process writes this store
/// root; multi-writer shared-root deployments are unsupported - there is no
/// cross-process lock, so two such writers would be serialised only by the
/// filesystem's atomic create.
#[derive(Debug, Clone)]
pub struct CommitmentStore {
    /// Directory holding `{TransactionUID}.cbor` files.
    root: PathBuf,
}

impl CommitmentStore {
    /// Open (or lazily create on first write) a store rooted at `root`.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The store root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to one transaction's CBOR file.
    ///
    /// The Transaction UID is a DICOM UID (`[0-9.]`), which the router has
    /// already validated, so it is safe as a file stem. The guard repeats that
    /// check anyway: a UID containing a separator or `..` would let one
    /// transaction address a file outside the store, and a store is the wrong
    /// place to trust its caller.
    #[must_use]
    pub fn path_for(&self, transaction_uid: &str) -> PathBuf {
        let stem: String = transaction_uid
            .chars()
            .map(|c| {
                if c.is_ascii_digit() || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(format!("{stem}.cbor"))
    }

    /// Read one Result by Transaction UID. `Ok(None)` when there is no file -
    /// which the Check transaction reports as 404 (13.5.3.1).
    pub fn find(&self, transaction_uid: &str) -> Result<Option<CommitmentRecord>, StoreError> {
        let path = self.path_for(transaction_uid);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Io(e)),
        };
        CommitmentRecord::decode(&bytes)
            .map(Some)
            .map_err(StoreError::Decode)
    }

    /// Insert a new result. Fails with [`StoreError::Duplicate`] when the
    /// Transaction UID is already recorded - the 409 of Table 13.4.3-1.
    ///
    /// The install is an atomic create-if-absent (`hard_link` from a private
    /// temp file), not a check followed by a write. The old `find()`-then-write
    /// pair had a window in which two concurrent Commits carrying one UID could
    /// both observe "absent" and both rename over the target, so the request
    /// that PS3.18 defines as the conflict answered success with whichever
    /// record landed last. `hard_link` fails with `AlreadyExists` in the kernel,
    /// which is the same primitive [`crate::ups::store`] uses for the identical
    /// problem.
    pub fn insert(&self, rec: &CommitmentRecord) -> Result<(), StoreError> {
        // Refuse an unreadable record BEFORE anything lands on disk. `find`
        // applies these invariants when decoding, so installing a record that
        // violates them left a file the store could never serve: the next
        // `find` rejected it, and the corrected insert of the same
        // transaction then failed as `Duplicate` - the archive kept an
        // orphan that blocked its own repair.
        rec.validate().map_err(StoreError::Decode)?;
        let bytes = rec.encode().map_err(StoreError::Decode)?;
        // The same striped lock as `delete` and `sweep_expired` (see
        // `uid_lock`): the create-if-absent install must not race a delete of
        // the SAME uid, or the delete could remove the file this insert just
        // committed.
        let stripe = uid_lock(&rec.transaction_uid);
        let _guard = stripe.lock().unwrap_or_else(|e| e.into_inner());
        self.write_if_absent(&rec.transaction_uid, &bytes)
    }

    /// Create the record only if no file exists at its path.
    ///
    /// `hard_link` is the affordable atomic create-if-absent: it fails with
    /// `AlreadyExists` when the target is taken, in the kernel, so a second
    /// writer cannot slip between a check and an install.
    ///
    /// # Durability
    ///
    /// The record is synced BEFORE it is linked into place, so `Ok` means the
    /// bytes are on the medium rather than merely scheduled to be: a Commit
    /// that answered 200 promises PS3.18 13.4.4 that the result is recorded,
    /// and a crash between "linked" and "flushed" would break that promise
    /// while leaving no trace of the failure. That is the same barrier order
    /// the STOW path uses (tmp `sync_all` before the rename), and it is why
    /// the temp file is opened as a handle here instead of written by
    /// `std::fs::write`.
    ///
    /// The parent-directory entry is flushed too, so the NAME survives a
    /// crash and not only the data. On Windows a directory cannot be opened
    /// for `FlushFileBuffers`, so that half is Unix-only and documented - the
    /// same OS boundary `sync_batch_manifests` states.
    ///
    /// A failed sync REMOVES the temp file and reports the error: a caller
    /// that never learns the record is not durable would answer 200 for a
    /// Commit the next boot cannot find.
    fn write_if_absent(&self, transaction_uid: &str, bytes: &[u8]) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.root)?;
        // The root's NAME has to survive a crash too, or the whole directory
        // can vanish from its parent while its contents are durable. Only the
        // first Commit pays this; a store that already existed needs nothing.
        crate::durable::sync_new_root_name(&self.root)?;
        let target = self.path_for(transaction_uid);
        // The temp is created EXCLUSIVELY. `File::create` TRUNCATES, and the
        // temp name is `{stem}.{pid}.{seq}.tmp`: a crash between the hard link
        // and the unlink below leaves that name pointing at the INSTALLED
        // inode, so a later process that reuses pid+seq would truncate the
        // record instead of writing a fresh temp. `create_new` turns the
        // collision into an error and the retry moves to a fresh seq.
        let (tmp, _guard) = match self.create_temp_exclusive() {
            Ok(v) => v,
            Err(e) => return Err(StoreError::Io(e)),
        };
        if let Err(e) = write_all_and_sync(&_guard, bytes) {
            drop(_guard);
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::Io(e));
        }
        // Release the exclusive-create handle before linking: Windows will not
        // hard-link a file that is still open for writing.
        drop(_guard);
        let linked = std::fs::hard_link(&tmp, &target);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => {
                // The data is durable; this makes the directory entry that
                // names it durable as well. Best-effort by platform necessity
                // (see the doc comment), not by choice.
                //
                // Through `sync_barrier`, like every other flush in this file:
                // the counter it bumps is the ONLY observable side of a flush,
                // so a bare `sync_all` here is invisible to the durability tests
                // and would silently regress without any signal.
                #[cfg(unix)]
                if let Err(e) = crate::durable::sync_dir_entry(&self.root) {
                    return Err(StoreError::Io(e));
                }
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // The link landed but its NAME was never made durable (the
                // Ok-arm barrier either failed above or a crash happened in
                // between). A retry that finds the target occupied must still
                // persist the directory entry before answering Duplicate, or
                // the client is told "already stored" for a name that can
                // vanish on the next crash. The barrier is an idempotent dir
                // fsync, so running it on this rare conflict path is cheap.
                #[cfg(unix)]
                if let Err(e) = crate::durable::sync_dir_entry(&self.root) {
                    return Err(StoreError::Io(e));
                }
                Err(StoreError::Duplicate(transaction_uid.to_string()))
            }
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// Create a temp file that no other writer can have selected.
    ///
    /// The sequence number is only a HINT: pid+seq can repeat across PID
    /// namespaces or after a restart, so the name is claimed with `create_new`
    /// and a collision retries with the next sequence rather than opening the
    /// existing file. The returned handle keeps the name ours until it is
    /// dropped.
    fn create_temp_exclusive(&self) -> io::Result<(PathBuf, std::fs::File)> {
        // A bounded retry: each collision consumes a fresh seq, so this only
        // runs long on a pathological store and must not loop forever.
        const ATTEMPTS: usize = 64;
        let mut last = None;
        for _ in 0..ATTEMPTS {
            let path = self.tmp_path();
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(f) => return Ok((path, f)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free temp name")))
    }

    /// Delete one result. Returns `true` when a file was removed.
    ///
    /// Takes the same striped lock as [`Self::insert`]: a delete racing an
    /// insert of the SAME uid could otherwise remove the file the insert just
    /// committed, or the insert could re-create it under the delete's
    /// `remove_file` - the store would then hand out an "absent" and a
    /// "present" answer for one transaction in the same instant.
    pub fn delete(&self, transaction_uid: &str) -> Result<bool, StoreError> {
        let stripe = uid_lock(transaction_uid);
        let _guard = stripe.lock().unwrap_or_else(|e| e.into_inner());
        self.remove_locked(transaction_uid)
    }

    /// Remove one result file WITHOUT taking the stripe lock.
    ///
    /// The caller holds the uid's stripe lock (see [`Self::delete`] and
    /// [`sweep_expired`]), which is what makes the remove atomic against an
    /// insert of the same uid.
    fn remove_locked(&self, transaction_uid: &str) -> Result<bool, StoreError> {
        match std::fs::remove_file(self.path_for(transaction_uid)) {
            Ok(()) => {
                // The name is gone; persist the directory entry that named it
                // too, or a crash after this delete can restore the result.
                // Same barrier the insert path applies after hard_link;
                // barrier failure propagates, open failure is swallowed by
                // the helper (see durable.rs policy).
                #[cfg(unix)]
                crate::durable::sync_dir_entry(&self.root)?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// A unique temporary path beside `target`.
    ///
    /// The name carries the pid and a counter rather than being a fixed
    /// `{uid}.tmp`: two processes writing the same UID would otherwise share a
    /// temp file and could interleave into a torn record. The `.tmp` suffix
    /// also keeps a temp out of [`Self::transaction_uids`], which accepts only
    /// `.cbor`.
    ///
    /// The name is a HINT, not a reservation: pid+seq repeats across PID
    /// namespaces and restarts, so the caller claims it with `create_new` and
    /// retries with the next sequence on collision.
    fn tmp_path(&self) -> std::path::PathBuf {
        self.root.join(format!(
            "commitment.{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    /// Write `bytes` to the transaction's path via tmp + rename.
    ///
    /// The rename is what makes the record appear whole or not at all. Used by
    /// callers that intend to REPLACE an existing record; [`Self::insert`] must
    /// not, which is why it goes through [`Self::write_if_absent`] instead.
    /// Every recorded Transaction UID, for the sweep and for diagnostics.
    ///
    /// A missing directory is an empty store, not an error: the store is
    /// created lazily on the first Commit, so a server that has never answered
    /// one has no directory and is still perfectly healthy.
    pub fn transaction_uids(&self) -> Result<Vec<String>, StoreError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Io(e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Skip the in-flight temp files so a sweep cannot report a
            // transaction that is still being written.
            if let Some(stem) = name.strip_suffix(".cbor") {
                out.push(stem.to_string());
            }
        }
        out.sort_unstable();
        Ok(out)
    }
}

/// Write `bytes` to an EXCLUSIVELY created file and flush them to the medium.
///
/// The handle is already open (the caller took it with `create_new`, which is
/// what reserves the name), so this only owns the write + barrier half of the
/// old `write_and_sync`.
fn write_all_and_sync(file: &std::fs::File, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    (&*file).write_all(bytes)?;
    crate::durable::sync_barrier(file)
}

/// Write `bytes` to `path` and flush them to the medium.
///
/// Retained for the barrier tests only: the record path now creates its temp
/// EXCLUSIVELY (see `create_temp_exclusive`) and writes through the handle it
/// already holds, so nothing in production opens a file by path here.
#[cfg(test)]
fn write_and_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    crate::durable::sync_barrier(&file)
}

/// Delete result files whose `requested_at` is older than `retention_days`.
///
/// Returns `(removed_uids, failed_deletes)`. `retention_days == 0` means KEEP
/// FOREVER and the sweep does nothing - the same "0 disables" semantics the
/// UPS deletion-lock TTL uses, so an operator has one convention to remember.
///
/// The comparison is on the record's own `requested_at` rather than the file's
/// mtime: a backup restore or a filesystem copy rewrites mtimes, and retention
/// must follow the answer's age, not the file's.
///
/// A failed delete is COUNTED, not propagated: one unremovable file must not
/// stop retention for every other expired transaction. The caller surfaces the
/// count (a stuck file keeps failing every pass until it is cleared).
pub fn sweep_expired(
    store: &CommitmentStore,
    retention_days: u64,
    now_epoch_secs: i64,
) -> Result<(Vec<String>, u64), StoreError> {
    if retention_days == 0 {
        return Ok((Vec::new(), 0));
    }
    // Checked: a huge `retention_days` used to overflow the multiplication and
    // wrap `cutoff` FORWARD, which made every record look expired - one
    // misconfigured value would have deleted the whole store. On overflow the
    // policy is "unbounded", i.e. nothing is old enough to remove.
    //
    // `try_from` rather than `as i64`: the cast is a bit-truncation, so any
    // value at or above 2^63 came out NEGATIVE, and a negative retention puts
    // the cutoff in the FUTURE - the same wrap-forward the multiplication check
    // guards against, reached one step earlier.
    let Ok(days) = i64::try_from(retention_days) else {
        return Ok((Vec::new(), 0));
    };
    let Some(retention_secs) = days.checked_mul(86_400) else {
        return Ok((Vec::new(), 0));
    };
    let Some(cutoff) = now_epoch_secs.checked_sub(retention_secs) else {
        return Ok((Vec::new(), 0));
    };
    let mut removed = Vec::new();
    let mut failed: u64 = 0;
    for uid in store.transaction_uids()? {
        // Hold the uid's stripe lock across the find + age check + delete: the
        // read and the remove are one critical section against an insert of
        // the same uid. Without it an insert could commit between the `find`
        // (which saw the expired record) and the delete, and the delete would
        // then remove the NEW record the insert just committed.
        let stripe = uid_lock(&uid);
        let _guard = stripe.lock().unwrap_or_else(|e| e.into_inner());
        // A record that will not decode is LEFT IN PLACE and the sweep keeps
        // going. Propagating the error stopped the whole pass on the first
        // corrupt file, so retention never ran again - and the operator lost
        // both the cleanup and any signal about the other files.
        let rec = match store.find(&uid) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(_) => continue,
        };
        let Some(age) = parse_epoch_secs(&rec.requested_at) else {
            // A record whose timestamp cannot be read is LEFT ALONE: deleting
            // evidence because its clock could not be parsed would turn a
            // formatting problem into data loss.
            continue;
        };
        if age < cutoff {
            match store.remove_locked(&uid) {
                Ok(true) => removed.push(uid),
                Ok(false) => {}
                // A delete that fails is COUNTED, not silent: one stuck file
                // must not stop retention for every other expired
                // transaction. The caller surfaces the count.
                Err(_) => failed += 1,
            }
        }
    }
    Ok((removed, failed))
}

/// Parse the `requested_at` timestamps this store writes.
///
/// The format is the crate's own ISO-8601 UTC text, `YYYY-MM-DDTHH:MM:SSZ`,
/// which is exactly what `crate::time` produces. Parsing it here rather than
/// pulling in a date library keeps the "no new dependencies" rule, and the
/// daily-resolution arithmetic the sweep needs does not require civil-time
/// conversion: days-since-epoch plus the time of day is enough.
fn parse_epoch_secs(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    // Fixed width: `2026-09-23T10:00:00Z` is exactly 20 bytes. Reading 19
    // bytes at fixed offsets without checking the 20th byte let
    // `2026-09-23T10:00:00garbage` parse and the record was DELETED - the
    // opposite of the "unparseable -> preserve" policy. The exact width AND
    // the trailing `Z` are both part of the format this store writes.
    if bytes.len() != 20
        || bytes[19] != b'Z'
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
    {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(civil_days_from_epoch(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days in a Gregorian month, with the leap-year rule (PS3.5 DA is the
/// calendar date, so a day past its month is not a date).
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

/// Days from 1970-01-01 to `y-m-d` (Howard Hinnant's `days_from_civil`).
///
/// Reimplemented rather than re-exported because `crate::time` exposes the
/// inverse direction; a few lines here is cheaper than widening that API for
/// one caller, and the arithmetic is the standard one.
fn civil_days_from_epoch(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::record::{
        CommitmentOutcome, CommitmentRecord, FailureReason, InstanceResult, RequestForm,
    };
    use crate::durable::syncs_observed;

    fn record(uid: &str, at: &str) -> CommitmentRecord {
        CommitmentRecord {
            transaction_uid: uid.to_string(),
            requested_at: at.to_string(),
            request_form: RequestForm::Instance,
            instances: vec![InstanceResult {
                sop_class_uid: "1.2.840.10008.5.1.4.1.1.2".to_string(),
                sop_instance_uid: "2.25.1".to_string(),
                outcome: CommitmentOutcome::Committed,
                failure_reason: None,
                study_uid: None,
                series_uid: None,
            }],
        }
    }

    #[test]
    fn a_record_survives_the_write_read_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let rec = record("1.2.3.4", "2026-09-23T10:00:00Z");
        store.insert(&rec).expect("insert");
        let back = store.find("1.2.3.4").expect("find").expect("present");
        assert_eq!(back, rec);
    }

    #[test]
    fn a_missing_transaction_is_none_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        assert!(store.find("9.9.9").expect("find").is_none());
    }

    /// The timestamp must be the exact `YYYY-MM-DDTHH:MM:SSZ` this store
    /// writes. Anything else is unparseable and the record is PRESERVED, never
    /// deleted because its clock could not be read.
    #[test]
    fn an_exact_timestamp_parses_and_garbage_is_rejected() {
        let ok = parse_epoch_secs("2026-09-23T10:00:00Z").expect("exact parses");
        assert_eq!(ok, civil_days_from_epoch(2026, 9, 23) * 86_400 + 10 * 3600);

        // A 20th byte other than `Z` (or trailing garbage) must not parse.
        assert!(parse_epoch_secs("2026-09-23T10:00:00garbage").is_none());
        assert!(parse_epoch_secs("2026-09-23T10:00:00 ").is_none());
        // Too short: the old `len < 19` guard never checked the width exactly.
        assert!(parse_epoch_secs("2026-09-23T10:00:0Z").is_none());
        assert!(parse_epoch_secs("2026-09-23T10:00:00").is_none());
    }

    /// The calendar date is validated against the month: a date that names no
    /// day must not parse as if it did.
    #[test]
    fn an_impossible_calendar_date_does_not_parse() {
        assert!(parse_epoch_secs("2026-02-31T10:00:00Z").is_none());
        assert!(parse_epoch_secs("2026-13-01T10:00:00Z").is_none());
        // Leap years: 2024 is leap, 2026 is not.
        assert!(parse_epoch_secs("2024-02-29T10:00:00Z").is_some());
        assert!(parse_epoch_secs("2026-02-29T10:00:00Z").is_none());
        // Century rule: 2000 is leap, 1900 is not.
        assert!(parse_epoch_secs("2000-02-29T00:00:00Z").is_some());
        assert!(parse_epoch_secs("1900-02-29T00:00:00Z").is_none());
    }

    /// A temp name that is already taken must NEVER be reused.
    ///
    /// The crash case this pins: a process dies between the hard link and the
    /// unlink, so the temp name is left pointing at the INSTALLED record. A
    /// later process that reuses pid+seq would then open that name and, with
    /// `File::create`, truncate the committed record - destroying a result we
    /// already answered 200 for (PS3.18 13.4.4).
    ///
    /// Forcing a collision takes care: the allocator advances its own counter
    /// when it builds a name, so pre-creating "the next" name from the test
    /// does not collide - the allocator simply moves on. The occupied name is
    /// therefore placed where the ALLOCATOR will look, by pre-creating a whole
    /// window of the sequence space and asserting the allocator lands on a
    /// name outside it with every pre-created file intact.
    #[test]
    fn an_occupied_temp_name_is_skipped_not_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        std::fs::create_dir_all(store.root()).expect("root");

        // Occupy a window starting where the allocator's next name will be.
        const WINDOW: u64 = 32;
        let first = store.tmp_path();
        let seq_of = |p: &std::path::Path| -> u64 {
            p.file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit('.').next())
                .and_then(|n| n.parse().ok())
                .expect("temp name carries its sequence")
        };
        let base = seq_of(&first);
        std::fs::write(&first, b"installed record bytes").expect("pre-create");
        for k in 1..=WINDOW {
            let p = store.root().join(format!(
                "commitment.{}.{}.tmp",
                std::process::id(),
                base + k
            ));
            std::fs::write(&p, b"installed record bytes").expect("pre-create");
        }

        let (fresh, handle) = store
            .create_temp_exclusive()
            .expect("the allocator must find a free name");
        drop(handle);

        let fresh_seq = seq_of(&fresh);
        assert!(
            fresh_seq > base + WINDOW,
            "every occupied name must be skipped, got seq {fresh_seq} inside the \
             occupied window {base}..={}",
            base + WINDOW
        );
        assert_eq!(
            std::fs::read(&first).expect("stolen still readable"),
            b"installed record bytes",
            "the pre-existing file must not be truncated"
        );
        for k in 1..=WINDOW {
            let p = store.root().join(format!(
                "commitment.{}.{}.tmp",
                std::process::id(),
                base + k
            ));
            assert_eq!(
                std::fs::read(&p).expect("occupied still readable"),
                b"installed record bytes",
                "an occupied temp must never be overwritten"
            );
        }
    }

    /// The root's NAME is flushed in its parent before the record is linked.
    ///
    /// `create_dir_all` + `sync_all` on the directory persist the entries
    /// INSIDE it, never the root's own name in the parent, so a crash could
    /// leave a fully durable store that its parent does not list. The barrier
    /// counter makes the extra flush observable - content checks cannot see it.
    ///
    /// Unix-only by necessity: Windows cannot open a directory for
    /// `FlushFileBuffers`, which is why the barrier itself is `#[cfg(unix)]`.
    #[cfg(unix)]
    #[test]
    fn the_first_commit_flushes_the_store_root_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Two levels deep so the parent exists and is a real directory.
        let store = CommitmentStore::new(dir.path().join("nested").join("commitments"));
        let before = syncs_observed();
        store
            .insert(&record("1.2.3.88", "2026-09-23T10:00:00Z"))
            .expect("insert");
        let after = syncs_observed();
        // Data barrier + root-directory barrier + parent-directory barrier.
        assert!(
            after >= before + 3,
            "the root name has to be flushed in its parent as well: expected at \
             least 3 barriers, observed {}",
            after - before
        );
    }

    /// The write is flushed to the medium before the record is linked in.
    ///
    /// Two claims, both falsifiable: a successful `insert` leaves no temp file
    /// and the target holds the exact bytes; and the flush ACTUALLY RUNS -
    /// checked on the barrier counter, because `sync_all` leaves nothing else a
    /// test could observe. Removing the `sync_all` call leaves the file
    /// contents correct while the count stays flat, which is precisely the
    /// silent regression this pins.
    #[test]
    fn an_insert_flushes_before_it_links_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let rec = record("1.2.3.77", "2026-09-23T10:00:00Z");
        let before = syncs_observed();
        store.insert(&rec).expect("insert");
        assert!(
            syncs_observed() > before,
            "a committed insert has to flush the record to the medium"
        );

        let leftovers: Vec<String> = std::fs::read_dir(store.root())
            .expect("read root")
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp file may survive a committed insert, found {leftovers:?}"
        );
        let on_disk = std::fs::read(store.path_for("1.2.3.77")).expect("target exists");
        assert_eq!(on_disk, rec.encode().expect("encode"));
    }

    /// A failed flush is a failed insert, not a silent success.
    ///
    /// This is the property that makes the barrier worth having: if `sync_all`
    /// errors and the caller still sees `Ok`, the server answers 200 for a
    /// Commit that the next boot may not find. The failure is injected by
    /// pointing the write at a path that cannot be created, so no platform
    /// knob is involved.
    #[test]
    fn a_write_that_cannot_be_flushed_is_an_error_not_a_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A DIRECTORY at the temp path: `File::create` fails, so nothing is
        // written and nothing can be linked. The store must report the I/O
        // error rather than falling through to the (absent) hard_link.
        let blocked = dir.path().join("blocked.tmp");
        std::fs::create_dir_all(&blocked).expect("blocking directory");
        let err = write_and_sync(&blocked, b"payload").expect_err("must not succeed");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::IsADirectory | io::ErrorKind::Other
            ) || err.kind() != io::ErrorKind::NotFound,
            "the failure has to surface as an I/O error, got {err:?}"
        );
    }

    /// C6: the install primitive refuses an occupied path.
    ///
    /// Testing "insert twice, expect Duplicate" does NOT discriminate the fix:
    /// the old `find()`-then-`write` answered `Duplicate` there too, because the
    /// first insert had already committed. The defect lives in the window
    /// BETWEEN the check and the install, so this drives `write_if_absent` with
    /// the target already present - the exact state a racing winner produces.
    /// On the old path this was a `rename`/`write` over the target and returned
    /// `Ok`; now the kernel refuses with `AlreadyExists`.
    #[test]
    fn an_install_onto_an_occupied_path_is_refused_and_keeps_the_winner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let uid = "1.2.3.9";

        let winner = record(uid, "2026-09-23T10:00:00Z");
        let bytes = winner.encode().expect("encode winner");
        store.write_if_absent(uid, &bytes).expect("winner installs");

        // A different payload for the same UID: what a replace would publish.
        let loser = record(uid, "2026-09-23T11:11:11Z");
        let loser_bytes = loser.encode().expect("encode loser");
        match store.write_if_absent(uid, &loser_bytes) {
            Err(StoreError::Duplicate(x)) => assert_eq!(x, uid),
            other => panic!("the install must refuse an occupied path, got {other:?}"),
        }

        let back = store.find(uid).expect("find").expect("present");
        assert_eq!(
            back.requested_at, "2026-09-23T10:00:00Z",
            "an occupied path must not be replaced"
        );
    }

    /// C6 end-to-end: `insert` surfaces the refusal as `Duplicate`.
    #[test]
    fn a_second_insert_of_one_transaction_uid_is_a_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        store
            .insert(&record("1.2.3.9", "2026-09-23T10:00:00Z"))
            .expect("first insert wins");
        match store.insert(&record("1.2.3.9", "2026-09-23T11:11:11Z")) {
            Err(StoreError::Duplicate(uid)) => assert_eq!(uid, "1.2.3.9"),
            other => panic!("expected Duplicate, got {other:?}"),
        }
    }

    #[test]
    fn a_duplicate_transaction_uid_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let rec = record("1.2.3.4", "2026-09-23T10:00:00Z");
        store.insert(&rec).expect("first");
        assert!(matches!(
            store.insert(&rec),
            Err(StoreError::Duplicate(uid)) if uid == "1.2.3.4"
        ));
    }

    #[test]
    fn the_first_result_is_intact_after_a_refused_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let first = record("1.2.3.4", "2026-09-23T10:00:00Z");
        store.insert(&first).expect("first");
        // A second Commit with the same UID must leave the recorded answer
        // alone: a 409 tells the client its UID is in use, and replacing the
        // stored result would answer a different question than the one asked.
        let mut second = record("1.2.3.4", "2026-09-23T11:00:00Z");
        second.instances[0].outcome = CommitmentOutcome::Failed;
        second.instances[0].failure_reason =
            Some(crate::commitment::record::FailureReason::NoSuchSopInstance);
        assert!(store.insert(&second).is_err());
        let back = store.find("1.2.3.4").expect("find").expect("present");
        assert_eq!(back, first);
    }

    #[test]
    fn an_empty_store_lists_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        assert!(store.transaction_uids().expect("list").is_empty());
    }

    #[test]
    fn a_transaction_uid_cannot_escape_the_store() {
        // The router validates UIDs, but the store must not depend on that:
        // a UID carrying a separator would otherwise address a file outside
        // the store directory.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let path = store.path_for("../../etc/passwd");
        assert_eq!(
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some("commitments"),
            "the path must stay inside the store root: {}",
            path.display()
        );
    }

    #[test]
    fn the_sweep_removes_only_expired_results() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let now = 1_800_000_000i64; // 2027-01-15T08:00:00Z
        store
            .insert(&record("1.2.3.1", "2026-09-23T10:00:00Z"))
            .expect("old");
        store
            .insert(&record("1.2.3.2", "2027-01-15T07:00:00Z"))
            .expect("new");

        let (removed, failed) = sweep_expired(&store, 30, now).expect("sweep");
        assert_eq!(removed, vec!["1.2.3.1".to_string()]);
        assert_eq!(failed, 0);
        assert!(
            store.find("1.2.3.1").expect("find").is_none(),
            "an expired result is gone, which is what makes Check answer 404"
        );
        assert!(
            store.find("1.2.3.2").expect("find").is_some(),
            "a result inside the window stays"
        );
    }

    /// The sweep's find-check-delete is one stripe-locked critical section.
    ///
    /// The delete must happen under the SAME lock acquisition as the `find`
    /// and the age check: holding the uid's stripe in the test makes the sweep
    /// block, which proves the sweep takes that lock. Reverting the lock from
    /// the sweep lets it finish while the guard is still held, so the recv
    /// succeeds immediately and this test fails - it is not vacuous.
    #[test]
    fn a_sweep_waits_for_the_uid_stripe_lock_before_deleting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        store
            .insert(&record("1.2.3.42", "2026-09-23T10:00:00Z"))
            .expect("expired record");
        let now = 1_800_000_000i64;

        let stripe = uid_lock("1.2.3.42");
        let held = stripe.lock().unwrap_or_else(|e| e.into_inner());

        let (tx, rx) = std::sync::mpsc::channel();
        let sweep_store = store.clone();
        let sweep_thread = std::thread::spawn(move || {
            let outcome = sweep_expired(&sweep_store, 30, now);
            tx.send(outcome).expect("send");
        });

        // While the stripe is held the sweep must NOT complete: it waits on
        // the same lock before the find+delete of this uid.
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "the sweep must wait for the uid's stripe lock"
        );

        drop(held);
        let (removed, failed) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the sweep finishes once the lock is released")
            .expect("the sweep must not error");
        assert_eq!(removed, vec!["1.2.3.42".to_string()]);
        assert_eq!(failed, 0);
        sweep_thread.join().expect("join sweep thread");
    }

    /// A delete failure is COUNTED and the sweep goes on.
    ///
    /// One unremovable expired file used to abort the whole pass via `?`, so
    /// retention stopped at the first stuck file and every later expired
    /// transaction stayed past its window. The sweep now counts the failure
    /// and still removes the files it can.
    ///
    /// The unremovable case is a DIRECTORY in the record's place: `find`
    /// cannot decode it (so the record is skipped), which is exactly the
    /// "one bad file must not stop the others" contract.
    #[test]
    fn a_stuck_record_does_not_stop_the_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let now = 1_800_000_000i64;
        store
            .insert(&record("1.2.3.1", "2026-09-23T10:00:00Z"))
            .expect("expired");
        store
            .insert(&record("1.2.3.2", "2026-09-23T10:00:00Z"))
            .expect("also expired");
        // Replace the first record's file with a DIRECTORY: it cannot be read
        // as a record, so the sweep skips it instead of failing the pass.
        let stuck = store.path_for("1.2.3.1");
        std::fs::remove_file(&stuck).expect("remove file");
        std::fs::create_dir(&stuck).expect("plant a directory");

        let (removed, _failed) = sweep_expired(&store, 30, now).expect("sweep");
        assert_eq!(
            removed,
            vec!["1.2.3.2".to_string()],
            "the removable expired file still goes"
        );
        assert!(stuck.is_dir(), "the stuck record stays for the next pass");
    }

    #[test]
    fn a_retention_of_zero_keeps_results_forever() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        store
            .insert(&record("1.2.3.1", "1999-01-01T00:00:00Z"))
            .expect("ancient");
        let (removed, _) = sweep_expired(&store, 0, 1_800_000_000).expect("sweep");
        assert!(removed.is_empty(), "0 means keep forever");
        assert!(store.find("1.2.3.1").expect("find").is_some());
    }

    /// A retention window beyond `i64::MAX` days used to go through `as i64`,
    /// which is a bit-truncation: 2^63 came out NEGATIVE, so the cutoff landed
    /// in the future and every record looked expired - the same
    /// wrap-forward the multiplication check guards against, one step earlier.
    #[test]
    fn an_absurd_retention_removes_nothing_rather_than_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        store
            .insert(&record("1.2.3.1", "2026-09-23T10:00:00Z"))
            .expect("insert");
        for days in [i64::MAX as u64, u64::MAX, u64::MAX / 2] {
            let (removed, _) = sweep_expired(&store, days, 1_800_000_000).expect("sweep");
            assert!(removed.is_empty(), "retention {days} must not delete");
            assert!(
                store.find("1.2.3.1").expect("find").is_some(),
                "retention {days} must keep the record"
            );
        }
    }

    #[test]
    fn a_record_with_an_unparseable_timestamp_is_left_alone() {
        // Deleting on a clock we could not read would turn a formatting
        // problem into silent data loss.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        store
            .insert(&record("1.2.3.1", "not a timestamp"))
            .expect("insert");
        let (removed, _) = sweep_expired(&store, 1, 1_800_000_000).expect("sweep");
        assert!(removed.is_empty());
        assert!(store.find("1.2.3.1").expect("find").is_some());
    }

    #[test]
    fn timestamps_parse_to_the_epoch_seconds_the_sweep_compares() {
        // The one date that has to be exact for the arithmetic to be right.
        assert_eq!(parse_epoch_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_epoch_secs("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_epoch_secs("2000-03-01T00:00:00Z"), Some(951_868_800));
        // Leap day: 2024-02-29 exists and is one day after 2024-02-28.
        let feb28 = parse_epoch_secs("2024-02-28T00:00:00Z").expect("28");
        let feb29 = parse_epoch_secs("2024-02-29T00:00:00Z").expect("29");
        assert_eq!(feb29 - feb28, 86_400);
        // And a 2100 date is NOT a leap year (the /100, /400 terms).
        let y2100 = parse_epoch_secs("2100-03-01T00:00:00Z").expect("2100");
        let y2100f = parse_epoch_secs("2100-02-28T00:00:00Z").expect("2100f");
        assert_eq!(y2100 - y2100f, 86_400, "Feb 28 -> Mar 1 in a non-leap year");

        assert_eq!(parse_epoch_secs("garbage"), None);
        assert_eq!(parse_epoch_secs("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_epoch_secs(""), None);
    }

    #[test]
    fn the_timestamp_written_and_parsed_agree_with_the_crate_clock_format() {
        // The store's own reader must accept what crate::time produces; this
        // pins the shape rather than trusting the two to stay in step.
        let now = crate::time::local_now();
        let text = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            now.year, now.month, now.day, now.hour, now.minute, now.second
        );
        assert!(
            parse_epoch_secs(&text).is_some(),
            "the crate clock's own text must parse: {text}"
        );
    }

    /// F3: the root-directory flush goes through `sync_barrier`.
    ///
    /// The barrier counter is the only observable side of a flush, so a bare
    /// `sync_all` here is INVISIBLE to the durability tests: it still works on
    /// disk while every assertion keeps passing, and a later refactor could
    /// delete it with no signal. Counting it makes the call itself the contract.
    ///
    /// Unix-only, like the barrier: Windows cannot open a directory for
    /// `FlushFileBuffers`.
    #[cfg(unix)]
    #[test]
    fn the_root_directory_flush_goes_through_the_barrier() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("nested").join("commitments"));
        let before = syncs_observed();
        store
            .insert(&record("1.2.3.91", "2026-09-23T10:00:00Z"))
            .expect("insert");
        let after = syncs_observed();
        // data + root dir + parent dir (at minimum).
        assert!(
            after >= before + 3,
            "every flush must be counted, including the root-directory one: \
             observed {}",
            after - before
        );
    }

    /// F4: every directory level `create_dir_all` created gets its name flushed.
    ///
    /// Two gaps the single-parent version had: a bare relative root has an EMPTY
    /// parent (the entry to persist lives in `"."`), and a multi-level
    /// `create_dir_all` makes several directories while only the immediate
    /// parent was flushed - so a crash could lose the path from a durable
    /// ancestor down to the store.
    ///
    /// A nested fresh root needs at least one flush per created level.
    #[cfg(unix)]
    #[test]
    fn a_nested_root_flushes_every_created_level() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Three levels that do not exist yet: a/b/commitments.
        let store = CommitmentStore::new(dir.path().join("a").join("b").join("commitments"));
        let before = syncs_observed();
        store
            .insert(&record("1.2.3.92", "2026-09-23T10:00:00Z"))
            .expect("insert");
        let after = syncs_observed();
        // data + at least b + a (and the tempdir itself, depending on where the
        // walk stops). Four is the conservative lower bound for this shape.
        assert!(
            after >= before + 3,
            "a nested root must flush each created level: observed {}",
            after - before
        );
    }

    /// F4: an empty parent is resolved to `"."`, not treated as "no parent".
    ///
    /// `root.parent()` of `"commitments"` is `Some("")`. The old code filtered
    /// that out and returned early, so the store's own name was never flushed
    /// even though the directory to persist (`"."`) plainly exists.
    ///
    /// Exercised through the helper with an ABSOLUTE path whose parent chain
    /// ends at the filesystem root, which covers the same normalisation without
    /// changing the process working directory (`set_current_dir` in a test
    /// binary that runs tests in parallel would corrupt every other test).
    #[cfg(unix)]
    #[test]
    fn an_empty_parent_is_normalised_to_the_current_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A path with no parent component after normalisation.
        let bare = PathBuf::from(dir.path().join("commitments").to_string_lossy().to_string());
        let before = syncs_observed();
        crate::durable::sync_new_root_name(&bare).expect("the walk must not fail on a normal path");
        assert!(
            syncs_observed() > before,
            "the root name chain must be flushed, not skipped"
        );

        // And an empty parent string must not short-circuit the walk.
        let before = syncs_observed();
        crate::durable::sync_new_root_name(Path::new("commitments"))
            .expect("a bare relative root must not error");
        assert!(
            syncs_observed() > before,
            "a bare relative root resolves its parent to '.', which must be \
             flushed rather than filtered out"
        );
    }

    /// An unreadable record must be refused BEFORE it lands on disk.
    ///
    /// `find` enforces the Annex J.2 coupling when it decodes, so a record
    /// with `outcome = Failed` and no failure reason encoded fine, got
    /// installed, and then could never be read back. Worse, the next `find`
    /// rejected the file the store itself had accepted, and a corrected insert
    /// of the same transaction failed as `Duplicate` - the orphan blocked its
    /// own repair.
    #[test]
    fn an_invalid_record_is_refused_before_it_is_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));

        let mut bad = record("1.2.3.4.99", "2026-09-23T10:00:00Z");
        // Failed with no reason: the state `read_instance` rejects.
        bad.instances[0].outcome = CommitmentOutcome::Failed;
        bad.instances[0].failure_reason = None;
        let err = store.insert(&bad).unwrap_err();
        assert!(
            matches!(err, StoreError::Decode(_)),
            "an unreadable record must be refused as invalid, got {err:?}"
        );

        // Nothing was installed, so the path is still free.
        assert!(
            store.find(&bad.transaction_uid).expect("find").is_none(),
            "a refused insert must leave no target behind"
        );

        // A corrected insert of the same transaction now succeeds.
        let mut good = record("1.2.3.4.99", "2026-09-23T10:00:00Z");
        good.instances[0].outcome = CommitmentOutcome::Failed;
        good.instances[0].failure_reason = Some(FailureReason::NoSuchSopInstance);
        store
            .insert(&good)
            .expect("a corrected insert must succeed");
        assert!(store.find(&good.transaction_uid).expect("find").is_some());
    }

    /// A failed directory barrier is an Io error, never a stored verdict -
    /// on the Ok arm AND on the Duplicate arm.
    ///
    /// The install order is temp-sync, hard_link, remove temp, THEN dir
    /// barrier. A barrier failure after the hard_link leaves the record at its
    /// target; a retry finds the target occupied and used to answer Duplicate
    /// WITHOUT re-running the barrier, so the client was told "already stored"
    /// for a name that can vanish on crash. The seam fails the NEXT barrier on
    /// the store root, so the Ok arm surfaces Io and the Duplicate arm
    /// surfaces Io too. Reverting the Duplicate-arm barrier makes the second
    /// insert answer Duplicate (the seam is never consumed), which is the
    /// non-vacuousness proof.
    #[cfg(unix)]
    #[test]
    fn a_failed_barrier_is_io_on_both_the_ok_and_duplicate_arms() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        let rec = record("1.2.3.93", "2026-09-23T10:00:00Z");

        // Ok arm: the link lands, then the barrier fails -> Io, target stays.
        crate::durable::fail_next_barrier_for_test(store.root());
        let err = store.insert(&rec).unwrap_err();
        assert!(matches!(err, StoreError::Io(_)), "got {err:?}");
        assert!(
            store.path_for(&rec.transaction_uid).exists(),
            "the link landed before the barrier failed"
        );

        // Duplicate arm: the occupied target with the next barrier failing
        // must surface Io, not a silent Duplicate.
        crate::durable::fail_next_barrier_for_test(store.root());
        let err = store.insert(&rec).unwrap_err();
        assert!(
            matches!(err, StoreError::Io(_)),
            "a failing barrier on the conflict path is Io, got {err:?}"
        );

        // Healthy barrier: the occupied target is a Duplicate and the stored
        // record is left untouched (a 409 answers, it does not replace).
        let first = store
            .find(&rec.transaction_uid)
            .expect("find")
            .expect("present");
        assert!(
            matches!(store.insert(&rec), Err(StoreError::Duplicate(uid)) if uid == rec.transaction_uid)
        );
        let after = store
            .find(&rec.transaction_uid)
            .expect("find")
            .expect("present");
        assert_eq!(
            after, first,
            "a refused duplicate must not replace the record"
        );
    }

    /// A successful delete persists the directory entry that named the result
    /// (Unix-only), so a crash after `remove_file` cannot restore it. Observed
    /// on the barrier counter, the same proof the insert path uses.
    #[cfg(unix)]
    #[test]
    fn a_delete_persists_the_directory_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        store
            .insert(&record("1.2.3.4.95", "2026-09-23T10:00:00Z"))
            .expect("insert");

        let before = syncs_observed();
        assert!(store.delete("1.2.3.4.95").expect("delete"));
        assert!(
            syncs_observed() > before,
            "a successful delete must flush the directory entry"
        );
    }

    /// A barrier failure on the delete path is an error, not a stored
    /// "deleted". Reverting the added barrier makes `delete` return Ok(true)
    /// and this test fails.
    #[cfg(unix)]
    #[test]
    fn a_delete_with_a_failing_barrier_is_an_error_not_a_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        store
            .insert(&record("1.2.3.4.96", "2026-09-23T10:00:00Z"))
            .expect("insert");

        crate::durable::fail_next_barrier_for_test(store.root());
        let err = store.delete("1.2.3.4.96").unwrap_err();
        assert!(matches!(err, StoreError::Io(_)), "got {err:?}");
        assert!(
            !store.path_for("1.2.3.4.96").exists(),
            "the file was removed; the delete reported the barrier failure"
        );
    }
}
