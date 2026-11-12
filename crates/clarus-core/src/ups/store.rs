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
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "workitem store I/O error: {e}"),
            StoreError::Decode(e) => write!(f, "workitem store decode error: {e}"),
            StoreError::CasConflict(uid) => write!(f, "workitem CAS conflict: {uid}"),
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
#[derive(Debug, Clone)]
pub struct WorkItemStore {
    /// Directory holding `{uid}.cbor` files.
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
    /// (0040,0405) Scheduled Procedure Step Start DateTime - a REQUIRED
    /// matching key (PS3.4 Table CC.2.5-3, "Single Value or Range").
    ///
    /// A `-`-separated value is a RANGE across the stored DT, which is what the
    /// standard's matchType states; a single value is an exact match.
    pub sps_start_datetime: Option<&'a str>,
    /// (0074,1200) Scheduled Procedure Step Priority - a REQUIRED matching key
    /// (Single Value).
    pub sps_priority: Option<&'a str>,
    /// (0010,0010) Patient's Name - a REQUIRED matching key.
    pub patient_name: Option<&'a str>,
    /// (0008,0060) Modality - vendor extended, matched case-insensitively
    /// (a modality is a DICOM CS value, so its case is not significant).
    pub modality: Option<&'a str>,
    pub workflow_state: Option<&'a str>,
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
    pub fn path_for(&self, uid: &str) -> PathBuf {
        self.root.join(format!("{uid}.cbor"))
    }

    /// Read one workitem by UID. Returns `Ok(None)` when the file does not
    /// exist (mirrors the SQLite `find_workitem_by_uid` `QueryReturnedNoRows`
    /// -> `Ok(None)` contract).
    pub fn find_by_uid(&self, uid: &str) -> Result<Option<WorkItemRecord>, StoreError> {
        let path = self.path_for(uid);
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
    /// The existence check and the write are ONE critical section under the
    /// uid's stripe lock, and the file itself is created with `hard_link` from
    /// the temp file: a `link` that finds the target already present fails with
    /// `AlreadyExists`, so "create only if absent" holds even against a writer
    /// this process cannot see (another process, another container). The lock
    /// alone would only serialise THIS process's threads.
    pub fn insert(&self, rec: &WorkItemRecord) -> Result<(), StoreError> {
        let stripe = uid_lock(&rec.workitem_uid);
        let _guard = stripe.lock().unwrap_or_else(|e| e.into_inner());
        if self.find_by_uid(&rec.workitem_uid)?.is_some() {
            return Err(StoreError::CasConflict(format!(
                "workitem already exists: {}",
                rec.workitem_uid
            )));
        }
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
    pub fn cas_update(
        &self,
        rec: &WorkItemRecord,
        expected_tuid: Option<&str>,
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
        self.write_atomic(rec)
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
        let path = self.path_for(uid);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
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
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
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
            let rec = match self.find_by_uid(stem) {
                Ok(Some(r)) => r,
                _ => continue, // corrupt/missing -> skip, mirror scan resilience
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
        let path = self.path_for(&rec.workitem_uid);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.write_tmp(rec)?;
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
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
        let path = self.path_for(&rec.workitem_uid);
        let bytes = rec
            .encode()
            .map_err(|e| StoreError::Decode(e.to_string()))?;
        let tmp = tmp_sibling(&path);
        if let Err(e) = std::fs::write(&tmp, &bytes) {
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
        let path = self.path_for(&rec.workitem_uid);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.write_tmp(rec)?;
        let linked = std::fs::hard_link(&tmp, &path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(StoreError::CasConflict(
                format!("workitem already exists: {}", rec.workitem_uid),
            )),
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
    // (0040,0405) ScheduledProcedureStepStartDateTime: "Single Value or Range"
    // per Table CC.2.5-3. An empty value matches everything (the zero-length
    // key convention); `<lo>-<hi>` is an inclusive range; anything else is one
    // instant compared exactly. DT values sort lexically in DICOM's fixed
    // `YYYYMMDDHHMMSS.FFFFFF` layout, so string comparison IS time comparison.
    if let Some(v) = f.sps_start_datetime
        && !v.is_empty()
    {
        let Some(stored) = rec.sps_start_datetime.as_deref() else {
            // A workitem with no start datetime does not match a datetime query.
            return false;
        };
        let matched = match v.split_once('-') {
            Some((lo, hi)) => (lo.is_empty() || stored >= lo) && (hi.is_empty() || stored <= hi),
            None => stored == v,
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

/// Build `{path}.tmp` - the sibling used by the atomic rename.
fn tmp_sibling(path: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn store() -> WorkItemStore {
        let dir = tempfile::tempdir().expect("tempdir");
        WorkItemStore::new(dir.path().join("workitems"))
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

    #[test]
    fn cas_update_honours_transaction_uid() {
        let s = store();
        let rec = sample("1.2.3", "SCHEDULED", "2026-01-01");
        s.insert(&rec).expect("insert");

        // Correct TUid -> update applies.
        let expected = rec.transaction_uid.clone().unwrap();
        let mut updated = rec.clone();
        updated.workflow_state = "IN PROGRESS".to_string();
        s.cas_update(&updated, Some(&expected)).expect("cas ok");
        assert_eq!(
            s.find_by_uid("1.2.3")
                .expect("find")
                .unwrap()
                .workflow_state,
            "IN PROGRESS"
        );

        // Wrong TUid -> conflict.
        let mut again = updated.clone();
        again.workflow_state = "COMPLETED".to_string();
        assert!(matches!(
            s.cas_update(&again, Some("WRONG")),
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
}
