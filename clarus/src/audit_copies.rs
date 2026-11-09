// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! 3008: sidecar snapshot for `GET /admin/v1/storage/audit-copies`.
//!
//! The endpoint serves ONLY sidecar facts from `manifests/`: `{study}.audited`
//! (hidden-PHI alibi copies) and `{study}.orphans` (replaced-version blobs).
//! This module NEVER opens CAS: no walk, no stat, no open - the scanner takes
//! only `manifest_dir`. Copies written before the sidecar mechanisms have no
//! lines and are invisible (no fallback scan, no `legacy_*` fields).
//!
//! A std background thread (`audit-copies`) rebuilds the snapshot immediately
//! at startup and then every 60 s; the GET handler only reads the prebuilt
//! snapshot under an RwLock. ap101: NOT REQUIRED - no line of this change sits
//! on the hot path.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// One sidecar-recorded CAS blob: its blake3 hex render and storage tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditCopiesEntry {
    pub hash: String,
    pub tier: u8,
}

/// Sidecar facts of one study (either or both lists may be empty).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditCopiesStudy {
    pub study_uid: String,
    /// `{study}.audited` lines: hidden-PHI alibi copies.
    pub tracked: Vec<AuditCopiesEntry>,
    /// `{study}.orphans` lines: replaced-version blobs.
    pub orphans: Vec<AuditCopiesEntry>,
}

/// The prebuilt snapshot the endpoint renders. `generated_at = None` means
/// NO scan ever completed (the startup "pending" shape); a successful scan
/// over an empty manifests/ dir is `ready` with zeros and a timestamp.
#[derive(Debug, Clone, Default)]
pub struct AuditCopiesSnapshot {
    pub generated_at: Option<String>,
    pub total_studies: u64,
    pub total_tracked: u64,
    pub total_orphans: u64,
    pub studies: Vec<AuditCopiesStudy>,
}

/// Build the snapshot from `manifest_dir` only. Deterministic output:
/// studies sorted by study_uid, entries sorted by hash. A sidecar line that
/// cannot be split into `hex tier` or whose tier does not parse as u8 is
/// skipped with a warning (the same tolerance as the erase-worker drains).
pub fn build_audit_copies_snapshot(manifest_dir: &Path) -> std::io::Result<AuditCopiesSnapshot> {
    let mut studies: Vec<AuditCopiesStudy> = Vec::new();
    for entry in std::fs::read_dir(manifest_dir)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().into_owned();
        let suffix = if fname.ends_with(".audited") {
            ".audited"
        } else if fname.ends_with(".orphans") {
            ".orphans"
        } else {
            continue;
        };
        let study_uid = fname.trim_end_matches(suffix).to_string();
        let text = std::fs::read_to_string(entry.path())?;
        // Both sidecar files of one study merge into ONE study entry.
        let study = match studies.iter_mut().find(|s| s.study_uid == study_uid) {
            Some(s) => s,
            None => {
                studies.push(AuditCopiesStudy {
                    study_uid: study_uid.clone(),
                    ..Default::default()
                });
                studies.last_mut().expect("study just pushed")
            }
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((hex, tier)) = line.split_once(' ') else {
                crate::clog_warn!(line = %line, "audit-copies: malformed sidecar line");
                continue;
            };
            let Ok(tier) = tier.parse::<u8>() else {
                crate::clog_warn!(line = %line, "audit-copies: malformed sidecar tier");
                continue;
            };
            // Dedup defensively: a duplicate line counts once in the snapshot.
            let list = if suffix == ".audited" {
                &mut study.tracked
            } else {
                &mut study.orphans
            };
            if !list.iter().any(|e| e.hash == hex) {
                list.push(AuditCopiesEntry {
                    hash: hex.to_string(),
                    tier,
                });
            }
        }
    }
    studies.sort_unstable_by(|a, b| a.study_uid.cmp(&b.study_uid));
    let mut total_tracked: u64 = 0;
    let mut total_orphans: u64 = 0;
    for s in &mut studies {
        s.tracked.sort_unstable_by(|a, b| a.hash.cmp(&b.hash));
        s.orphans.sort_unstable_by(|a, b| a.hash.cmp(&b.hash));
        total_tracked += s.tracked.len() as u64;
        total_orphans += s.orphans.len() as u64;
    }
    Ok(AuditCopiesSnapshot {
        generated_at: Some(crate::handlers::admin::pure::chrono_now()),
        total_studies: studies.len() as u64,
        total_tracked,
        total_orphans,
        studies,
    })
}

/// Rebuild the global snapshot from `manifest_dir`. On success replaces the
/// state (write lock); on error warns and KEEPS the previous snapshot (never
/// zeroes a ready snapshot, never fakes `generated_at`).
pub fn refresh(manifest_dir: &Path) {
    match build_audit_copies_snapshot(manifest_dir) {
        Ok(snapshot) => {
            let mut state = crate::handlers::admin::pure::AUDIT_COPIES_STATE
                .write()
                .expect("audit-copies state lock poisoned");
            *state = snapshot;
        }
        Err(e) => {
            crate::clog_warn!(
                error = %e,
                "audit-copies scanner failed; keeping the previous snapshot"
            );
        }
    }
}

/// Spawn the `audit-copies` scanner thread: first pass immediately, then a
/// refresh every 60 s. std-only; reads manifests/ only.
pub fn spawn(manifest_dir: PathBuf) {
    std::thread::Builder::new()
        .name("audit-copies".into())
        .spawn(move || {
            refresh(&manifest_dir);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                refresh(&manifest_dir);
            }
        })
        .expect("Failed to spawn audit-copies scanner thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidecar(root: &Path, study: &str, suffix: &str, text: &str) {
        std::fs::write(root.join(format!("{study}{suffix}")), text).expect("write sidecar");
    }

    #[test]
    fn empty_manifests_dir_is_ready_with_zeros() {
        let temp = tempfile::tempdir().expect("temp dir");
        let snap = build_audit_copies_snapshot(temp.path()).expect("build");
        assert!(snap.generated_at.is_some(), "ready carries a timestamp");
        assert_eq!(snap.total_studies, 0);
        assert_eq!(snap.total_tracked, 0);
        assert_eq!(snap.total_orphans, 0);
        assert!(snap.studies.is_empty());
    }

    #[test]
    fn audited_sidecar_valid_blank_and_malformed() {
        let temp = tempfile::tempdir().expect("temp dir");
        let a = "ab".repeat(32);
        let b = "cd".repeat(32);
        sidecar(
            temp.path(),
            "1.2.3.1",
            ".audited",
            &format!("{a} 1\n\nno pair\n{a} 1\n{b} 2\n"),
        );
        let snap = build_audit_copies_snapshot(temp.path()).expect("build");
        assert_eq!(snap.total_studies, 1);
        assert_eq!(snap.total_tracked, 2, "malformed skipped, dup deduped");
        assert_eq!(snap.total_orphans, 0);
        let s = &snap.studies[0];
        assert_eq!(s.study_uid, "1.2.3.1");
        assert_eq!(s.tracked.len(), 2);
        assert_eq!(s.tracked[0].hash, a);
        assert_eq!(s.tracked[0].tier, 1);
        assert_eq!(s.tracked[1].hash, b);
        assert_eq!(s.tracked[1].tier, 2);
    }

    #[test]
    fn orphans_sidecar_parsed() {
        let temp = tempfile::tempdir().expect("temp dir");
        let a = "ef".repeat(32);
        let b = "12".repeat(32);
        sidecar(
            temp.path(),
            "1.2.3.2",
            ".orphans",
            &format!("{a} 3\n{b} 1\n"),
        );
        let snap = build_audit_copies_snapshot(temp.path()).expect("build");
        assert_eq!(snap.total_studies, 1);
        assert_eq!(snap.total_tracked, 0);
        assert_eq!(snap.total_orphans, 2);
        let s = &snap.studies[0];
        assert_eq!(s.orphans[0].hash, b, "sorted by hash");
        assert_eq!(s.orphans[1].hash, a);
    }

    #[test]
    fn both_sidecars_one_study_sorted() {
        let temp = tempfile::tempdir().expect("temp dir");
        let t1 = "11".repeat(32);
        let t2 = "22".repeat(32);
        let o1 = "33".repeat(32);
        sidecar(
            temp.path(),
            "1.2.3.3",
            ".audited",
            &format!("{t2} 1\n{t1} 1\n"),
        );
        sidecar(temp.path(), "1.2.3.3", ".orphans", &format!("{o1} 1\n"));
        let snap = build_audit_copies_snapshot(temp.path()).expect("build");
        assert_eq!(snap.total_studies, 1);
        assert_eq!(snap.total_tracked, 2);
        assert_eq!(snap.total_orphans, 1);
        let s = &snap.studies[0];
        assert_eq!(s.tracked[0].hash, t1, "tracked sorted by hash");
        assert_eq!(s.tracked[1].hash, t2);
        assert_eq!(s.orphans[0].hash, o1);
    }

    #[test]
    fn two_studies_sorted_by_uid() {
        let temp = tempfile::tempdir().expect("temp dir");
        sidecar(
            temp.path(),
            "1.2.3.20",
            ".audited",
            "abababababababababababababababababababababababababababababababab 1\n",
        );
        sidecar(
            temp.path(),
            "1.2.3.10",
            ".audited",
            "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd 1\n",
        );
        let snap = build_audit_copies_snapshot(temp.path()).expect("build");
        assert_eq!(snap.total_studies, 2);
        assert_eq!(snap.studies[0].study_uid, "1.2.3.10");
        assert_eq!(snap.studies[1].study_uid, "1.2.3.20");
    }

    #[test]
    fn missing_manifests_dir_refresh_keeps_pending() {
        // A failed FIRST scan must leave the global state in the startup
        // "pending" shape (generated_at None) and must not panic.
        {
            let mut state = crate::handlers::admin::pure::AUDIT_COPIES_STATE
                .write()
                .expect("lock");
            *state = AuditCopiesSnapshot::default();
        }
        refresh(Path::new("Z:/no/such/manifests/3008"));
        let state = crate::handlers::admin::pure::AUDIT_COPIES_STATE
            .read()
            .expect("lock");
        assert!(state.generated_at.is_none(), "still pending");
        assert_eq!(state.total_studies, 0);
        assert!(state.studies.is_empty());
    }
}
