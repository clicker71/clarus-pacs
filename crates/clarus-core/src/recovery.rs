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
use crate::idx::rollover::DailyIndexSet;
use crate::manifest::read_manifest;
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
    // One DailyIndexSet per reconciliation pass so appenders stay open only
    // for the duration of the scan; the set handles nightly rollover if any
    // manifest's date differs from today.
    let today = crate::idx::rollover::day_key_from_system_time(std::time::SystemTime::now());
    let mut set = DailyIndexSet::new(idx_dir.to_path_buf(), today);

    let mut entries: Vec<(String, [u8; 8], String)> = Vec::new();
    for entry in std::fs::read_dir(manifests_dir)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().into_owned();
        let Some(study_uid) = fname.strip_suffix(".manifest").map(String::from) else {
            continue;
        };
        report.manifests_scanned += 1;

        // Decode the manifest; a corrupt one is counted, not fatal to others.
        let path = manifests_dir.join(&fname);
        let manifest = match read_manifest(&path) {
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
        // Period the daily cycle before this manifest's date so the rollover
        // machinery opens the correct bucket file for the study's day.
        set.rollover_if_needed(day);
        entries.push((modality_str, day, study_uid));
    }

    // Append each missing study once, de-duplicated.
    for (modality_str, day, uid) in entries {
        // Check presence to avoid duplicate rows on repeated reconciles.
        let bucket = modality::read_study_uids(idx_dir, &modality_str, &day).unwrap_or_default();
        if bucket.iter().any(|u| u == &uid) {
            continue;
        }
        // Move the daily-rollover set to this study's day before opening the
        // writer, so entries of different days land in their own bucket files.
        set.rollover_if_needed(day);
        let mut row = [0u8; 128];
        let filled = modality::format_row(&uid, &mut row);
        if set
            .modality_writer(&modality_str)
            .and_then(|w| w.append(filled))
            .is_ok()
        {
            report.entries_added += 1;
        } else {
            // The row is still missing: the caller must keep reporting the tier
            // as stale instead of reading `entries_added == 0` as "consistent".
            report.append_failed += 1;
        }
    }

    Ok(report)
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
    let mut set = DailyIndexSet::new(idx_dir.to_path_buf(), day);
    set.rollover_if_needed(day);
    let mut row = [0u8; 128];
    let filled = crate::idx::modality::format_row(study_uid, &mut row);
    set.modality_writer(modality)
        .and_then(|w| w.append(filled))
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
}
