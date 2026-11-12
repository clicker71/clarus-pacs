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

/// Process-wide counter for temp-file names (see `write_atomic`).
///
/// The pid alone is not enough: one process writes many records concurrently,
/// so two Commits could share a temp name and interleave into a torn file.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
/// `Clone` just carries the root path, so the store is shared across handler
/// threads with no interior lock - each write is atomic and each file is
/// touched by exactly one transaction.
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
        let bytes = rec.encode().map_err(StoreError::Decode)?;
        self.write_if_absent(&rec.transaction_uid, &bytes)
    }

    /// Create the record only if no file exists at its path.
    ///
    /// `hard_link` is the affordable atomic create-if-absent: it fails with
    /// `AlreadyExists` when the target is taken, in the kernel, so a second
    /// writer cannot slip between a check and an install.
    fn write_if_absent(&self, transaction_uid: &str, bytes: &[u8]) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.root)?;
        let target = self.path_for(transaction_uid);
        let tmp = self.tmp_path(&target);
        if let Err(e) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::Io(e));
        }
        let linked = std::fs::hard_link(&tmp, &target);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Err(StoreError::Duplicate(transaction_uid.to_string()))
            }
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// Delete one result. Returns `true` when a file was removed.
    pub fn delete(&self, transaction_uid: &str) -> Result<bool, StoreError> {
        match std::fs::remove_file(self.path_for(transaction_uid)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// A unique temporary path beside `target`.
    ///
    /// The name carries the pid and a counter rather than being a fixed
    /// `{uid}.tmp`: two processes writing the same UID would otherwise share a
    /// temp file and could interleave into a torn record.
    fn tmp_path(&self, target: &Path) -> std::path::PathBuf {
        self.root.join(format!(
            "{}.{}.{}.tmp",
            target
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("commitment"),
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
            match store.delete(&uid) {
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
    // Fixed width: 2026-09-23T10:00:00Z is 20 bytes.
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(civil_days_from_epoch(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
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
        CommitmentOutcome, CommitmentRecord, InstanceResult, RequestForm,
    };

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
}
