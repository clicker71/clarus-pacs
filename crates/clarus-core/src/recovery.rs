// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Lazy recovery - reconcile `manifests/` with the `.idx` accelerators (C-008).
//!
//! The manifest directory is the source of truth for study attributes. The
//! `.idx` files (`dates/`, `modality/`) are accelerators: a stale or missing
//! `.idx` widens QIDO to a cold manifest scan but never loses a study. This
//! module walks `manifests/` and rebuilds the accelerator entries that are
//! missing, so the B-tree tier converges to `clarus_idx_stale_objects = 0`.
//!
//! # What is rebuilt
//!
//! The **modality** index rows carry study UIDs directly and are fully
//! rebuildable from a study-level manifest (modality + study date). The date
//! index stores *instance* BLAKE3 hashes, which a study-level manifest does not
//! carry - its rebuild requires the full-hierarchy manifest. The plan's
//! invariant is preserved: correctness never depends on index freshness, so
//! leaving the date index to the full-hierarchy path is safe.
//!
//! # Staleness metric
//!
//! [`reconcile_idx`] returns a [`ReconcileReport`]. A caller sets
//! `clarus_idx_stale_objects` to
//!
//! ```text
//! append_failed + corrupt_entries_needing_repair
//! ```
//!
//! NOT to `entries_added`, and not to `entries_added + append_failed` either.
//! The question that counter answers is "how much repair is still NEEDED", so
//! an entry that this pass successfully appended is gone from it (the work was
//! done), while an entry that was missing and whose append failed is still
//! outstanding. Counting `entries_added` inverted the meaning: a pass that
//! repaired everything reported the full backlog, and the next clean pass
//! reported `0` only because there was nothing left to find - the two were
//! indistinguishable from the number alone.
//!
//! `corrupt` is part of the backlog too: a manifest that cannot be decoded has
//! not had its row rebuilt and will be re-found on every pass. It is reported
//! separately from `append_failed` because the operator's remedy differs
//! (fix or refetch the manifest, not the index).

use crate::idx::modality;
use crate::idx::modality::ModalityIndex;
use crate::manifest::read_manifest_published;
use std::io;
use std::path::{Path, PathBuf};

/// The result of one reconciliation pass over `manifests/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    /// Total number of `.manifest` files scanned.
    pub manifests_scanned: u64,
    /// Number of studies whose modality index entry was missing and rebuilt.
    pub entries_added: u64,
    /// Number of studies whose index row was MISSING but could not be written.
    ///
    /// Without this the staleness metric was `entries_added`, so a pass where
    /// every append failed reported `0` - the same value as a pass that found
    /// nothing to do. The tier looked consistent while it was as stale as
    /// before. Callers add this to the stale count.
    pub append_failed: u64,
    /// Number of manifests that could not be decoded.
    pub corrupt: u64,
}

/// Errors produced by [`reconcile_idx`].
#[derive(Debug)]
pub enum ReconcileError {
    /// The manifests directory does not exist (nothing to reconcile).
    NoManifestDir(PathBuf),
    /// Filesystem error while reading a directory.
    Io(io::Error),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReconcileError::NoManifestDir(p) => write!(f, "manifests dir missing: {p:?}"),
            ReconcileError::Io(e) => write!(f, "recovery I/O error: {e}"),
        }
    }
}

impl std::error::Error for ReconcileError {}

impl From<io::Error> for ReconcileError {
    fn from(e: io::Error) -> Self {
        ReconcileError::Io(e)
    }
}

/// The `(modality, day)` pair naming one modality-index bucket.
///
/// Both halves are owned because the walk carries them out of `entries` while
/// the bucket's writer is open.
type BucketKey = (String, [u8; 8]);

/// Walk `manifests/` and rebuild missing modality-index entries.
///
/// For every study manifest, if its study UID is not already present in
/// `modality/{MOD}/YYYYMMDD.idx` (where `YYYYMMDD` comes from the manifest's
/// `StudyDate`), it is appended. Returns a [`ReconcileReport`] of what was
/// scanned, added, and left needing repair.
///
/// The report separates work DONE (`entries_added`) from work STILL NEEDED
/// (`append_failed`, `corrupt`); see the module docs for how a caller derives
/// the staleness gauge from it.
///
/// A missing `manifests/` directory is an error (the caller decides whether
/// that is fatal); an empty directory yields a zero report.
pub fn reconcile_idx(
    manifests_dir: &Path,
    idx_dir: &Path,
) -> Result<ReconcileReport, ReconcileError> {
    // `is_dir()` folds EVERY failure into `false`, so an archive the process
    // cannot traverse reported `NoManifestDir` - "there is nothing to
    // reconcile" - when the truth was "this archive cannot be read". The two
    // are opposite diagnoses for an operator. Opening the directory directly
    // and mapping only `NotFound` keeps the absence case and surfaces the
    // access failure.
    match std::fs::read_dir(manifests_dir) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ReconcileError::NoManifestDir(manifests_dir.to_path_buf()));
        }
        // A path that exists but is not a directory is also "nothing to
        // reconcile" in the caller's sense, and `read_dir` reports it as an
        // error of its own kind; keep the original variant for it.
        Err(e) if e.kind() == io::ErrorKind::NotADirectory => {
            return Err(ReconcileError::NoManifestDir(manifests_dir.to_path_buf()));
        }
        Err(e) => return Err(ReconcileError::Io(e)),
    }

    let mut report = ReconcileReport::default();

    let mut entries: Vec<(String, [u8; 8], String)> = Vec::new();
    for entry in std::fs::read_dir(manifests_dir)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().into_owned();
        let Some(study_uid) = fname.strip_suffix(".manifest").map(String::from) else {
            continue;
        };
        report.manifests_scanned += 1;

        // Decode the manifest; a corrupt one is counted, not fatal to others.
        let Some(published) = crate::manifest::PublishedManifest::new(manifests_dir, &study_uid)
        else {
            continue;
        };
        let manifest = match read_manifest_published(&published) {
            Ok(m) => m,
            Err(_) => {
                report.corrupt += 1;
                continue;
            }
        };

        // Extract modality + study date for the accelerator row.
        let mut modality_str = String::new();
        let mut day: [u8; 8] = [b'0'; 8];
        for (t, v) in &manifest.study.attrs {
            match (*t, v) {
                (crate::query::tag::MODALITY, crate::manifest::AttrValue::Text(s)) => {
                    modality_str = s.clone();
                }
                (crate::query::tag::STUDY_DATE, crate::manifest::AttrValue::UInt(d)) => {
                    day = u64_to_day_key(*d);
                }
                _ => {}
            }
        }
        if modality_str.is_empty() {
            // No modality -> no modality accelerator row; skip (still scanned).
            continue;
        }
        entries.push((modality_str, day, study_uid));
    }

    // Group the append pass by (modality, day) so a SINGLE writer can serve
    // the whole pass. `read_dir` hands entries back in filesystem order, so
    // buckets interleave; holding one `ModalityIndex` (one `File`) per touched
    // bucket until return would open one descriptor per bucket on a large
    // historical rebuild - far past the process FD limit (architectural class
    // C: unbounded resource). Row order inside a bucket is irrelevant
    // (`read_study_uids` is collected into a `HashSet`), so sorting changes no
    // observable result.
    entries.sort_unstable_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    // Append each missing study once, de-duplicated.
    //
    // The bucket is read ONCE per (modality, day), when the walk enters that
    // group. Reading it per entry made reconciliation quadratic in the number
    // of studies in a bucket - a day's modality file with thousands of rows
    // was re-read for every study on that day, which is exactly the O(N^2)
    // this walk exists to avoid.
    //
    // The single live writer: opened for one (modality, day) and dropped when
    // the bucket changes. A `DailyIndexSet` is NOT usable here: it carries a
    // single active day and treats every earlier day as immutable, so walking
    // manifests out of date order rolled it forward past a study's day and the
    // later attempt to roll back was refused - the row went to the newest
    // bucket instead of its own. That study was then never found on the next
    // pass and re-appended forever. Reconciliation is not a time-ordered
    // writer: it must reach any day it was asked about, so it opens the day
    // files directly - but, with `entries` sorted by (modality, day), it never
    // needs more than one open at a time.
    let mut current: Option<(BucketKey, ModalityIndex)> = None;
    // Only the bucket being worked on keeps its membership set. `entries` is
    // sorted by (modality, day), so a completed group is never revisited and
    // holding every earlier bucket's decoded UIDs only grew the map for the
    // length of the pass - a rebuild over a large archive held one `HashSet`
    // per day of every modality in RAM at once.
    let mut live: Option<(BucketKey, std::collections::HashSet<String>)> = None;
    let mut i = 0;
    while i < entries.len() {
        let (modality_str, day, uid) = entries[i].clone();
        let key = (modality_str, day);
        if live.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
            // Read the new bucket ONCE. A read failure is NOT an empty bucket:
            // `unwrap_or_default()` made both repair paths proceed as though no
            // UID were present, so the fixer could append a duplicate row and
            // the batch counted writes made without checking membership. Only
            // "the file does not exist" is a legitimate empty bucket.
            #[cfg(test)]
            BUCKET_READS.with(|c| c.set(c.get() + 1));
            let members: std::collections::HashSet<String> =
                match modality::read_study_uids(idx_dir, &key.0, &key.1) {
                    Ok(v) => v.into_iter().collect(),
                    // A bucket file that does not exist yet is genuinely empty.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Default::default(),
                    Err(_) => {
                        // Every remaining study in this bucket is un-reconciled
                        // this pass, and the caller must keep reporting the tier
                        // stale. `entries` is sorted by (modality, day), so they
                        // are the whole contiguous run from `i`.
                        let mut run = 0usize;
                        while i + run < entries.len()
                            && entries[i + run].0 == key.0
                            && entries[i + run].1 == key.1
                        {
                            run += 1;
                        }
                        report.append_failed += run as u64;
                        i += run;
                        continue;
                    }
                };
            live = Some((key.clone(), members));
        }
        i += 1;
        let bucket = &mut live
            .as_mut()
            .expect("a live bucket is loaded for this key")
            .1;
        // Check presence to avoid duplicate rows on repeated reconciles.
        if bucket.contains(&uid) {
            continue;
        }
        // Open (or reuse) the writer for this study's OWN day, so a study dated
        // earlier lands in its own bucket file regardless of scan order. The
        // previous writer is dropped here when the bucket key changes - its
        // `ModalityIndex` owns the `File`, so replacement closes the old one.
        if current.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
            current = match modality::open_appender_no_repair(idx_dir, &key.0, &key.1) {
                Ok(w) => Some((key.clone(), w)),
                Err(_) => {
                    report.append_failed += 1;
                    continue;
                }
            };
        }
        let writer = &mut current
            .as_mut()
            .expect("a writer is open for this bucket")
            .1;
        let mut row = [0u8; 128];
        let filled = modality::format_row(&uid, &mut row);
        if writer.append(filled).is_ok() {
            report.entries_added += 1;
            // Only a row that actually landed joins the membership set.
            live.as_mut()
                .expect("a live bucket is loaded for this key")
                .1
                .insert(uid);
        } else {
            // The row is still missing: the caller must keep reporting the tier
            // as stale instead of reading `entries_added == 0` as "consistent".
            report.append_failed += 1;
        }
    }

    Ok(report)
}

// Count of bucket reads performed by reconciliation on THIS thread.
//
// Reconciliation must read each `(modality, day)` bucket ONCE per pass. The
// count is the only observable of that rule: the RESULT is identical whether
// the file is read once or ten thousand times, so a test that only checks the
// rows cannot tell the two apart.
//
// Thread-local rather than global: cargo runs the test binary's tests in
// parallel, and a global counter would also count another test's reads.
#[cfg(test)]
thread_local! {
    static BUCKET_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
/// Format an integer `YYYYMMDD` as an 8-byte day key.
///
/// Callers must already have established that `d` has at most eight decimal
/// digits; [`day_key_from_yyyymmdd`] is the checked entry point.
fn u64_to_day_key(mut d: u64) -> [u8; 8] {
    let mut out = [b'0'; 8];
    let mut i = 8;
    while i > 0 {
        i -= 1;
        out[i] = b'0' + (d % 10) as u8;
        d /= 10;
        if d == 0 {
            break;
        }
    }
    out
}

/// Convert a manifest `StudyDate` (0008,0020) into a bucket day key.
///
/// ERRORS on a value that is not exactly eight decimal digits. The old
/// signature returned `[u8; 8]` unconditionally and simply kept the LOW eight
/// digits, so `120260101` produced the key `20260101` - the same bucket as a
/// completely different date. Reconciliation feeds stored `UInt` dates through
/// here, so a corrupt or misencoded date silently filed a row under someone
/// else's day and the lookups for both were then wrong.
///
/// A MISSING date is the caller's decision (`Option`), not this function's:
/// callers that have no date must not call it at all.
pub fn day_key_from_yyyymmdd(date_yyyymmdd: u64) -> Result<[u8; 8], InvalidStudyDate> {
    if date_yyyymmdd == 0 || date_yyyymmdd > 99_999_999 {
        return Err(InvalidStudyDate(date_yyyymmdd));
    }
    Ok(u64_to_day_key(date_yyyymmdd))
}

/// A `StudyDate` that cannot be a `YYYYMMDD` day key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidStudyDate(pub u64);

impl std::fmt::Display for InvalidStudyDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StudyDate {} is not an eight-digit YYYYMMDD key", self.0)
    }
}

impl std::error::Error for InvalidStudyDate {}

/// Append one study's modality-idx row if it is missing (runtime heal).
///
/// This is the single-study sibling of [`reconcile_idx`], used by the server's
/// background fixer thread: given a study that a lookup confirmed exists in
/// `manifests/` but is absent from `modality/{MOD}/{YYYYMMDD}.idx`, append the
/// `study_uid\n` row so the accelerator catches up. The append is idempotent by
/// the same read-bucket-then-append contract `reconcile_idx` uses (and, like
/// there, is append-only - it never rewrites a bucket).
///
/// Returns `Ok(true)` when the row was missing and has been appended, `Ok(false)`
/// when it was already present (nothing to do), and `Err` on any I/O failure.
pub fn heal_modality_row_if_missing(
    idx_dir: &Path,
    modality: &str,
    day: [u8; 8],
    study_uid: &str,
) -> std::io::Result<bool> {
    if modality.is_empty() {
        // No modality -> no modality accelerator row (mirrors reconcile_idx).
        return Ok(false);
    }
    // A read failure is NOT an empty bucket. `unwrap_or_default()` made the
    // fixer proceed as though no UID were present and append a duplicate row -
    // the one thing this function exists to prevent. Only "the file does not
    // exist yet" is a legitimate empty bucket; anything else is reported to the
    // caller so the tier stays reported stale rather than silently duplicated.
    let bucket = match crate::idx::modality::read_study_uids(idx_dir, modality, &day) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    if bucket.iter().any(|u| u == study_uid) {
        return Ok(false);
    }
    // Append WITHOUT repairing a torn tail: this runs on a study's own day,
    // which is often already sealed (rolled over) and may be mmapped by the
    // read path. Only the rollover writer, on its own current day, may cut a
    // torn tail; the fixer must leave every existing byte in place.
    let mut writer = modality::open_appender_no_repair(idx_dir, modality, &day)
        .map_err(std::io::Error::other)?;
    let mut row = [0u8; 128];
    let filled = crate::idx::modality::format_row(study_uid, &mut row);
    writer
        .append(filled)
        .map(|_| true)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stow::StudyManifestAttrs;
    use crate::stow::write_study_manifest;

    fn write_study(manifests: &Path, uid: &str, modality: &str, date: &str) {
        let mut a = StudyManifestAttrs {
            patient_name: Some("ivanov".into()),
            patient_id: Some("P1".into()),
            study_date: Some(date.into()),
            modality: modality.into(),
            study_instance_uid: uid.into(),
            ..Default::default()
        };
        write_study_manifest(manifests, &mut a).expect("write");
    }

    #[test]
    fn reconciles_missing_modality_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&manifests).expect("mkdir");

        write_study(&manifests, "1.2.1", "CT", "20260101");
        write_study(&manifests, "1.2.2", "MR", "20260202");

        let report = reconcile_idx(&manifests, &idx).expect("reconcile");
        assert_eq!(report.manifests_scanned, 2);
        assert_eq!(report.entries_added, 2);

        // A second pass adds nothing (idempotent) and reports 0 stale.
        let report2 = reconcile_idx(&manifests, &idx).expect("reconcile 2");
        assert_eq!(report2.entries_added, 0);

        // The buckets now contain the studies.
        let ct = modality::read_study_uids(&idx, "CT", b"20260101").expect("ct");
        assert!(ct.contains(&"1.2.1".to_string()));
        let mr = modality::read_study_uids(&idx, "MR", b"20260202").expect("mr");
        assert!(mr.contains(&"1.2.2".to_string()));
    }

    #[test]
    fn missing_manifest_dir_is_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        assert!(matches!(
            reconcile_idx(&missing, &dir.path().join("idx")),
            Err(ReconcileError::NoManifestDir(_))
        ));
    }

    /// An untraversable manifests directory is `Io`, not `NoManifestDir`.
    ///
    /// The old check was `is_dir()`, which reports `false` for a permission
    /// failure exactly as it does for an absent path, so an archive nobody
    /// could read was diagnosed as "there is nothing to reconcile". The two
    /// call for opposite operator actions.
    ///
    /// The unreadable path must be the *parent chain*, not the manifests
    /// directory itself: `stat` on a mode-000 directory succeeds (only its
    /// contents are hidden), so `is_dir()` still returns `true` and the old
    /// code reached `read_dir` and produced the same `Io` - a fixture that
    /// left the bug untested. Making the GRANDPARENT untraversable means
    /// `stat` itself fails, which is precisely where `is_dir()` collapses the
    /// error into `false`.
    ///
    /// `chmod` does not bind for root, so the precondition is asserted rather
    /// than assumed; the test skips loudly when it cannot be set up.
    #[cfg(unix)]
    #[test]
    fn unreadable_manifest_dir_is_io_not_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let inner = dir.path().join("inner");
        let manifests = inner.join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");

        let no_access = std::fs::Permissions::from_mode(0o000);
        let full = std::fs::Permissions::from_mode(0o755);

        // Precondition: the mode must actually deny traversal to this process.
        std::fs::set_permissions(&inner, no_access).expect("chmod");
        let blocked = std::fs::metadata(&manifests).is_err();
        if !blocked {
            std::fs::set_permissions(&inner, full).expect("chmod");
            eprintln!("skipped: process can traverse a mode-000 directory (running as root)");
            return;
        }

        let res = reconcile_idx(&manifests, &dir.path().join("idx"));
        // Restore before asserting so a failure does not leave an unreadable
        // directory behind for `TempDir`'s cleanup.
        std::fs::set_permissions(&inner, full).expect("chmod");

        match res {
            Err(ReconcileError::Io(e)) => {
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            }
            Err(ReconcileError::NoManifestDir(p)) => {
                panic!("an unreadable dir was reported as absent: {p:?}")
            }
            Ok(r) => panic!("an unreadable dir produced a report: {r:?}"),
        }
    }

    /// Interleaved (modality, day) order must still place each study in its OWN
    /// bucket, and the pass must read each bucket exactly once - proving the
    /// sort + single-writer refactor did not break per-day placement or the
    /// read-once contract across multiple buckets.
    #[test]
    fn interleaved_days_land_in_their_own_bucket_and_read_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&manifests).expect("mkdir");

        // Same modality, interleaved days (the read_dir order may interleave).
        write_study(&manifests, "1.2.1", "CT", "20260101");
        write_study(&manifests, "1.2.2", "CT", "20260102");
        write_study(&manifests, "1.2.3", "CT", "20260101");
        write_study(&manifests, "1.2.4", "CT", "20260103");

        BUCKET_READS.with(|c| c.set(0));
        let report = reconcile_idx(&manifests, &idx).expect("reconcile");
        assert_eq!(report.entries_added, 4);
        // Three distinct (CT, day) buckets -> read exactly once each.
        assert_eq!(
            BUCKET_READS.with(|c| c.get()),
            3,
            "three distinct buckets must each be read once"
        );

        let day1 = modality::read_study_uids(&idx, "CT", b"20260101").expect("day1");
        assert!(day1.contains(&"1.2.1".to_string()));
        assert!(day1.contains(&"1.2.3".to_string()));
        assert!(!day1.contains(&"1.2.2".to_string()));
        let day2 = modality::read_study_uids(&idx, "CT", b"20260102").expect("day2");
        assert!(day2.contains(&"1.2.2".to_string()));
        let day3 = modality::read_study_uids(&idx, "CT", b"20260103").expect("day3");
        assert!(day3.contains(&"1.2.4".to_string()));
    }

    /// Each bucket must be read ONCE per reconciliation pass.
    ///
    /// Reading it per manifest made the pass quadratic in the studies sharing a
    /// modality and day: the same file was decoded again for every row it
    /// already contained. The rows produced are identical either way, so the
    /// read count is the only signal.
    #[test]
    fn each_bucket_is_read_once_per_pass() {
        // The read counter is process-global, so a parallel test reconciling
        // its own tree would advance it too. Serialise this one test.
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&manifests).expect("mkdir");

        // Six studies sharing ONE modality and day: one bucket, six rows.
        for i in 1..=6 {
            write_study(&manifests, &format!("1.2.{i}"), "CT", "20260101");
        }

        BUCKET_READS.with(|c| c.set(0));
        let report = reconcile_idx(&manifests, &idx).expect("reconcile");
        assert_eq!(report.entries_added, 6);
        let reads = BUCKET_READS.with(|c| c.get());
        assert_eq!(
            reads, 1,
            "one bucket must be read once, not once per manifest (was {reads})"
        );

        // A second pass adds nothing and still reads the bucket once.
        BUCKET_READS.with(|c| c.set(0));
        let report2 = reconcile_idx(&manifests, &idx).expect("reconcile 2");
        assert_eq!(report2.entries_added, 0);
        assert_eq!(BUCKET_READS.with(|c| c.get()), 1);
    }

    /// A fixer append to a past (already sealed) day must grow the file by a
    /// whole row and never shrink it.
    ///
    /// The old path routed through `DailyIndexSet::modality_writer`, whose
    /// `open_appender` truncates a torn tail. A sealed day's file may be
    /// mmapped (`idx/preload.rs`), so truncating it faults; the heal must
    /// append after the tail and leave every existing byte in place.
    #[test]
    fn heal_on_a_sealed_day_grows_by_a_row_and_never_shrinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx = dir.path().join("idx");
        let day = *b"20250101";

        // Seed one row, then tear the tail (no trailing newline).
        {
            let mut w = modality::open_appender(&idx, "CT", &day).expect("open");
            let mut row = [0u8; 128];
            w.append(modality::format_row("1.2.1", &mut row))
                .expect("seed");
        }
        let path = modality::file_path(&idx, "CT", &day);
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(b"1.2.torn").expect("torn write");
        }
        let before = std::fs::metadata(&path).expect("meta").len();
        assert_eq!(
            before,
            b"1.2.1\n1.2.torn".len() as u64,
            "fixture must start torn"
        );

        let healed = heal_modality_row_if_missing(&idx, "CT", day, "1.2.2").expect("heal");
        assert!(healed, "a missing row must be appended");

        let data = std::fs::read(&path).expect("read");
        assert_eq!(
            data, b"1.2.1\n1.2.torn\n1.2.2\n",
            "the heal must establish a newline boundary, not fuse the torn \\
             fragment onto the next row"
        );
        assert!(
            data.len() as u64 > before,
            "the heal must never shrink a sealed day file"
        );

        // Both the healed row and the torn fragment stay readable as separate
        // UIDs: a fused `1.2.torn1.2.2` named neither.
        let uids = modality::read_study_uids(&idx, "CT", &day).expect("read uids");
        assert!(
            uids.iter().any(|u| u == "1.2.2"),
            "the healed row must be readable: {uids:?}"
        );
        assert!(
            uids.iter().any(|u| u == "1.2.torn"),
            "the torn fragment must be readable: {uids:?}"
        );
    }

    /// A bucket that exists but cannot be READ is not an empty bucket.
    ///
    /// `unwrap_or_default()` mapped every read error onto "no UID present", so
    /// an unreadable `.idx` made the pass append the row again - duplicating
    /// every study in the bucket - and reported a clean tier. The heal must
    /// propagate the read failure instead.
    ///
    /// The bucket is made unreadable by occupying its path with a DIRECTORY,
    /// which fails `read` with a non-`NotFound` kind on every platform.
    ///
    /// The assertion is on error PROVENANCE, not on `is_err()` or on the kind
    /// alone: the append path collides with the very same directory and fails
    /// with the same `PermissionDenied` kind (on Windows) or `IsADirectory`
    /// (on unix), so a weaker assertion passes with the guard removed - it
    /// would be observing the append's failure, not the read's. The heal wraps
    /// an append failure with `io::Error::other`, whose message begins
    /// `modality index I/O error:`, while a propagated read failure is
    /// returned bare. Asserting the message does NOT carry that prefix pins
    /// the failure to the read.
    #[test]
    fn heal_reports_a_read_failure_instead_of_appending_a_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx = dir.path().join("idx");
        let day = *b"20260101";

        let path = modality::file_path(&idx, "CT", &day);
        std::fs::create_dir_all(&path).expect("mkdir");

        let err = heal_modality_row_if_missing(&idx, "CT", day, "1.2.1")
            .expect_err("an unreadable bucket must be reported");
        let msg = err.to_string();
        assert_ne!(
            err.kind(),
            io::ErrorKind::NotFound,
            "an existing-but-unreadable bucket must not be reported as absent"
        );
        assert!(
            !msg.contains("modality index I/O error"),
            "the failure must come from the BUCKET READ, not from an appended \
             duplicate hitting the same path: {msg}"
        );
        // Nothing was written: the path is still the directory we planted.
        assert!(
            path.is_dir(),
            "the heal must not have replaced the unreadable bucket"
        );
        assert_eq!(
            std::fs::read_dir(&path).expect("readdir").count(),
            0,
            "no row may be written into the unreadable bucket's path"
        );
    }

    /// An absent bucket IS an empty bucket: the heal must still append.
    ///
    /// This is the other half of the pair above - distinguishing read failure
    /// from absence is only useful if absence keeps working.
    #[test]
    fn heal_treats_an_absent_bucket_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx = dir.path().join("idx");
        let day = *b"20260101";

        let healed = heal_modality_row_if_missing(&idx, "CT", day, "1.2.1").expect("heal");
        assert!(healed);
        let uids = modality::read_study_uids(&idx, "CT", &day).expect("read");
        assert_eq!(uids, vec!["1.2.1".to_string()]);

        // Idempotent on the second call.
        let again = heal_modality_row_if_missing(&idx, "CT", day, "1.2.1").expect("heal 2");
        assert!(!again, "a present row must not be appended twice");
        let uids = modality::read_study_uids(&idx, "CT", &day).expect("read 2");
        assert_eq!(uids, vec!["1.2.1".to_string()]);
    }

    /// A day key must be exactly eight digits, not merely "the last eight".
    ///
    /// The old signature returned `[u8; 8]` with no error channel and kept the
    /// low digits, so `120260101` produced the key for `20260101` - a corrupt
    /// manifest silently filed its study under an unrelated day.
    #[test]
    fn day_key_rejects_over_wide_dates_instead_of_truncating() {
        assert_eq!(
            day_key_from_yyyymmdd(20260101).expect("valid"),
            *b"20260101"
        );
        assert_eq!(
            day_key_from_yyyymmdd(99999999).expect("valid max"),
            *b"99999999"
        );
        assert_eq!(
            day_key_from_yyyymmdd(101).expect("valid short"),
            *b"00000101"
        );

        // `120260101` truncates to `20260101` under the old behaviour: the same
        // key as a different, perfectly valid date. It must be refused.
        let err = day_key_from_yyyymmdd(120260101).expect_err("over-wide must fail");
        assert_eq!(err, InvalidStudyDate(120260101));
        assert_ne!(
            day_key_from_yyyymmdd(120260101),
            day_key_from_yyyymmdd(20260101),
            "a truncated key would collide with the valid date"
        );

        assert_eq!(
            day_key_from_yyyymmdd(100000000).expect_err("nine digits"),
            InvalidStudyDate(100000000)
        );
        assert!(day_key_from_yyyymmdd(u64::MAX).is_err());
        assert!(
            day_key_from_yyyymmdd(0).is_err(),
            "no date is not a day key"
        );
    }
}
