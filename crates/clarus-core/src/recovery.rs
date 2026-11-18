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
//! [`reconcile_idx`] returns how many accelerator entries it added plus how
//! many manifests it scanned. A caller sets `clarus_idx_stale_objects` to the
//! number of *added* entries; `0` after a pass means the tier is consistent.

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

/// Walk `manifests/` and rebuild missing modality-index entries.
///
/// For every study manifest, if its study UID is not already present in
/// `modality/{MOD}/YYYYMMDD.idx` (where `YYYYMMDD` comes from the manifest's
/// `StudyDate`), it is appended. Returns a [`ReconcileReport`] of what was
/// scanned and added.
///
/// A missing `manifests/` directory is an error (the caller decides whether
/// that is fatal); an empty directory yields a zero report.
pub fn reconcile_idx(
    manifests_dir: &Path,
    idx_dir: &Path,
) -> Result<ReconcileReport, ReconcileError> {
    if !manifests_dir.is_dir() {
        return Err(ReconcileError::NoManifestDir(manifests_dir.to_path_buf()));
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
    // The bucket is read ONCE per (modality, day) and kept in memory for the
    // pass. Reading it per entry made reconciliation quadratic in the number
    // of studies in a bucket - a day's modality file with thousands of rows
    // was re-read for every study on that day, which is exactly the O(N^2)
    // this walk exists to avoid. The memo is updated after a successful
    // append so a second manifest naming the same UID cannot add it twice.
    let mut buckets: std::collections::HashMap<
        (String, [u8; 8]),
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
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
    let mut current: Option<((String, [u8; 8]), ModalityIndex)> = None;
    for (modality_str, day, uid) in entries {
        let key = (modality_str.clone(), day);
        let bucket = buckets.entry(key.clone()).or_insert_with(|| {
            #[cfg(test)]
            BUCKET_READS.with(|c| c.set(c.get() + 1));
            modality::read_study_uids(idx_dir, &key.0, &key.1)
                .unwrap_or_default()
                .into_iter()
                .collect()
        });
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
            bucket.insert(uid);
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

/// Public wrapper around [`u64_to_day_key`] so the server-side runtime fixer
/// can convert a manifest's `StudyDate` (0008,0020) into the day key used by
/// the modality-idx bucket files without re-implementing the conversion.
pub fn day_key_from_yyyymmdd(date_yyyymmdd: u64) -> [u8; 8] {
    u64_to_day_key(date_yyyymmdd)
}

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
    let bucket = crate::idx::modality::read_study_uids(idx_dir, modality, &day).unwrap_or_default();
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
}
