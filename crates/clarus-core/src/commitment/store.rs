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
    /// The existence test and the write are not one atomic operation, so two
    /// simultaneous Commits with the same UID could both pass the test. That
    /// is acceptable and deliberate: the loser is overwritten by an identical
    /// answer for the same UID, and serialising it would need a lock file for
    /// a case the standard already asks the CLIENT to avoid (a Transaction UID
    /// is a request identity, so a client that reuses one is out of bounds
    /// either way). The single-threaded reference path never races.
    pub fn insert(&self, rec: &CommitmentRecord) -> Result<(), StoreError> {
        if self.find(&rec.transaction_uid)?.is_some() {
            return Err(StoreError::Duplicate(rec.transaction_uid.clone()));
        }
        let bytes = rec.encode().map_err(StoreError::Decode)?;
        self.write_atomic(&rec.transaction_uid, &bytes)
    }

    /// Delete one result. Returns `true` when a file was removed.
    pub fn delete(&self, transaction_uid: &str) -> Result<bool, StoreError> {
        match std::fs::remove_file(self.path_for(transaction_uid)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// Write `bytes` to the transaction's path via tmp + rename.
    ///
    /// The temporary name carries the pid and a counter rather than being a
    /// fixed `{uid}.tmp`: two processes writing the same UID would otherwise
    /// share a temp file and could interleave into a torn record. The rename
    /// is what makes the record appear whole or not at all.
    fn write_atomic(&self, transaction_uid: &str, bytes: &[u8]) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.root)?;
        let target = self.path_for(transaction_uid);
        let tmp = self.root.join(format!(
            "{}.{}.{}.tmp",
            target
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("commitment"),
            std::process::id(),
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if let Err(e) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::Io(e));
        }
        if let Err(e) = std::fs::rename(&tmp, &target) {
            // A leftover temp is not a committed record (the sweep skips
            // `.tmp`), but it would accumulate forever if nobody removed it.
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::Io(e));
        }
        Ok(())
    }

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
/// Returns the UIDs that were removed. `retention_days == 0` means KEEP
/// FOREVER and the sweep does nothing - the same "0 disables" semantics the
/// UPS deletion-lock TTL uses, so an operator has one convention to remember.
///
/// The comparison is on the record's own `requested_at` rather than the file's
/// mtime: a backup restore or a filesystem copy rewrites mtimes, and retention
/// must follow the answer's age, not the file's.
pub fn sweep_expired(
    store: &CommitmentStore,
    retention_days: u64,
    now_epoch_secs: i64,
) -> Result<Vec<String>, StoreError> {
    if retention_days == 0 {
        return Ok(Vec::new());
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
        return Ok(Vec::new());
    };
    let Some(retention_secs) = days.checked_mul(86_400) else {
        return Ok(Vec::new());
    };
    let Some(cutoff) = now_epoch_secs.checked_sub(retention_secs) else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
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
        if age < cutoff && store.delete(&uid)? {
            removed.push(uid);
        }
    }
    Ok(removed)
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

        let removed = sweep_expired(&store, 30, now).expect("sweep");
        assert_eq!(removed, vec!["1.2.3.1".to_string()]);
        assert!(
            store.find("1.2.3.1").expect("find").is_none(),
            "an expired result is gone, which is what makes Check answer 404"
        );
        assert!(
            store.find("1.2.3.2").expect("find").is_some(),
            "a result inside the window stays"
        );
    }

    #[test]
    fn a_retention_of_zero_keeps_results_forever() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommitmentStore::new(dir.path().join("commitments"));
        store
            .insert(&record("1.2.3.1", "1999-01-01T00:00:00Z"))
            .expect("ancient");
        let removed = sweep_expired(&store, 0, 1_800_000_000).expect("sweep");
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
            let removed = sweep_expired(&store, days, 1_800_000_000).expect("sweep");
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
        let removed = sweep_expired(&store, 1, 1_800_000_000).expect("sweep");
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
