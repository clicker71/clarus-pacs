// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! CAS audit background walker - restores the walker lost in W9.
//!
//! `POST /admin/v1/storage/cas-audit` sets a "running" flag and returns 202;
//! the actual store walk (verify every CAS file's SHA-256 against its
//! filename) previously lived in an axum wrapper that W9 excised, leaving the
//! endpoint permanently "running". This module is the std-only, tokio-free
//! replacement respecting the rule (no async in new code).
//!
//! Behaviour mirrors the pre-W9 worker (commit b400ec6): same state fields,
//! same order (already-running -> 409 stays in pure.rs), the walker writes
//! `files_checked` / `errors` / terminal `status` into the injected state
//! (production passes the shared `AUDIT_STATE`; tests pass a local lock so
//! parallel runs cannot race). It adds the W9-owner fix: a file that vanishes
//! between the directory listing and the hash check (e.g. the
//! erase-window) is skipped after one retry - NOT counted as an error and
//! never a panic.
//!
//! The walk is recursive over the 2-2 BLAKE3 layout `{xx}/{yy}/{hash}.dcm`
//! plus any legacy single-level buckets.

#![forbid(unsafe_code)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Instant;

use crate::handlers::admin::pure::{CORRUPTED_HASH_LIST_CAP, CasAuditState};

/// Run one CAS audit pass over `cas_root`.
///
/// Enumerates files recursively, verifies each by recomputing SHA-256 and
/// comparing it to the filename stem, and updates `state` with
/// `files_checked`, `errors` and the terminal status `"done"` (or `"error: ..."`
/// when the root itself cannot be read). Designed to run on a `std::thread`;
/// returns nothing.
///
/// 2808 P1: files whose hash is NOT referenced by any live manifest are
/// counted into `state.unreferenced` (REPORT ONLY - no deletion; orphaned
/// blobs stay until the study erase reclaims them).
///
/// The state is injected: production passes the process-global
/// `pure::AUDIT_STATE` (a single walker writes it), while tests pass a local
/// `RwLock` so parallel tests can never race on a shared global.
pub fn run_cas_audit(cas_root: PathBuf, manifest_dir: PathBuf, state: &RwLock<CasAuditState>) {
    let referenced = crate::erase_worker::collect_referenced(&manifest_dir).unwrap_or_default();
    let started = Instant::now();
    let mut checked: u64 = 0;
    let mut errors: u64 = 0;
    let mut unreferenced: u64 = 0;
    // 0810: the two failure classes, split from the single `errors` count, plus
    // the corrupted-hash list (derived from the FILE NAME, never a re-read).
    let mut corrupted: u64 = 0;
    let mut unreadable: u64 = 0;
    let mut corrupted_hashes: Vec<String> = Vec::new();

    let mut dirs = vec![cas_root];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                set_status(state, format!("error: {e}"));
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.is_file() {
                // 2808 P1: report-only unreferenced counter (no deletion).
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && !referenced.contains(stem)
                {
                    unreferenced += 1;
                }
                match verify_cas_file(&path) {
                    VerifyResult::Ok => {}
                    VerifyResult::Mismatch => {
                        errors += 1;
                        corrupted += 1;
                        // The hash is the file NAME stem (the address the blob
                        // claims) - never re-read the tampered content.
                        if corrupted_hashes.len() < CORRUPTED_HASH_LIST_CAP
                            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                        {
                            corrupted_hashes.push(stem.to_string());
                        }
                        crate::clog_warn!(
                            path = %path.display(),
                            "CAS audit: hash mismatch"
                        );
                    }
                    VerifyResult::ReadError => {
                        errors += 1;
                        unreadable += 1;
                        crate::clog_warn!(
                            path = %path.display(),
                            "CAS audit: failed to read file"
                        );
                    }
                    VerifyResult::Gone => {}
                }
                checked += 1;
                update_progress(state, checked, errors, started);
            }
        }
    }

    // 0810: publish the gauges the viewer + Prometheus read, from the SAME
    // totals (single source of truth: the audit state).
    crate::metrics::CAS_BLOBS_CORRUPTED.store(corrupted, std::sync::atomic::Ordering::Relaxed);
    crate::metrics::CAS_BLOBS_UNREADABLE.store(unreadable, std::sync::atomic::Ordering::Relaxed);

    if let Ok(mut st) = state.write() {
        st.status = "done".to_string();
        st.files_checked = checked;
        st.errors = errors;
        st.unreferenced = unreferenced;
        st.corrupted = corrupted;
        st.unreadable = unreadable;
        st.corrupted_hashes = corrupted_hashes;
    }
    crate::clog_info!(
        "CAS audit complete: {} files checked, {} errors ({} corrupted, {} unreadable), {} unreferenced",
        checked,
        errors,
        corrupted,
        unreadable,
        unreferenced
    );
}

/// Outcome of verifying a single CAS file.
enum VerifyResult {
    /// Content hash matched the filename.
    Ok,
    /// Content hash did not match the filename.
    Mismatch,
    /// The file could not be read.
    ReadError,
    /// The file disappeared between listing and verification (erase-window).
    Gone,
}

/// Compute BLAKE3 of `path` and compare it with the filename stem.
///
/// A `NotFound` (and variants) is retried once after a short pause; if the file
/// is still gone it is `Gone` - never an error, matching the erase-window fix.
fn verify_cas_file(path: &Path) -> VerifyResult {
    let expected = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return VerifyResult::ReadError,
    };

    let data = match read_with_retry(path) {
        ReadOutcome::Data(d) => d,
        ReadOutcome::Gone => return VerifyResult::Gone,
        ReadOutcome::Error => return VerifyResult::ReadError,
    };

    let hash = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(&data));
    if hash == expected {
        VerifyResult::Ok
    } else {
        VerifyResult::Mismatch
    }
}

/// Result of attempting to read a CAS file.
enum ReadOutcome {
    Data(Vec<u8>),
    /// The file is gone (NotFound after one retry) - the erase-window case.
    Gone,
    /// Some other read failure.
    Error,
}

/// Read a file, retrying once on any transient/NotFound failure. A file that
/// still cannot be opened after the retry is reported as `Gone` only when it
/// really does not exist; other failures are `Error`.
fn read_with_retry(path: &Path) -> ReadOutcome {
    let first = std::fs::read(path);
    match first {
        Ok(d) => ReadOutcome::Data(d),
        Err(first_err) => {
            // One short retry: the file may have been mid-rename or the audit
            // is racing the erase-window.
            std::thread::sleep(std::time::Duration::from_millis(10));
            match std::fs::read(path) {
                Ok(d) => ReadOutcome::Data(d),
                Err(_) if first_err.kind() == std::io::ErrorKind::NotFound => ReadOutcome::Gone,
                Err(e) => {
                    crate::clog_warn!(path = %path.display(), error = %e, "CAS audit: read failure");
                    ReadOutcome::Error
                }
            }
        }
    }
}

fn set_status(state: &RwLock<CasAuditState>, status: String) {
    if let Ok(mut st) = state.write() {
        st.status = status;
    }
}

fn update_progress(state: &RwLock<CasAuditState>, checked: u64, errors: u64, started: Instant) {
    if let Ok(mut st) = state.write() {
        st.files_checked = checked;
        st.errors = errors;
        // 1410: live ETA from measured throughput; only once the walk has
        // started and the total is known. Production sets `files_total` at
        // audit start; tests leave it 0, so ETA stays None there.
        if checked > 0 && st.files_total > 0 {
            let elapsed = started.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                let rate = checked as f64 / elapsed;
                let remaining = st.files_total.saturating_sub(checked) as f64 / rate;
                st.eta_seconds = Some(remaining.ceil() as u64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a CAS file whose BLAKE3 matches its own filename (valid entry).
    fn write_valid_cas(dir: &Path, content: &[u8]) -> PathBuf {
        let name = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(content));
        let path = dir.join(format!("{name}.dcm"));
        std::fs::write(&path, content).expect("write valid cas");
        path
    }

    #[test]
    fn walker_running_to_done_with_valid_and_corrupt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bucket = tmp.path().join("ab").join("cd");
        std::fs::create_dir_all(&bucket).expect("buckets");

        write_valid_cas(&bucket, b"ok-content");
        // Corrupt entry: filename does not match content hash.
        let corrupt =
            bucket.join("0139f07e8d5e8d5c41f32a9e6c3d9f1e0d5b8a2c7e4f6a9d1b3c5e7f9a0b2d4e6f.dcm");
        std::fs::write(&corrupt, b"tampered").expect("write corrupt");

        // Local state container: parallel tests never race on a shared global.
        let state = RwLock::new(CasAuditState {
            status: "running".to_string(),
            files_checked: 0,
            files_total: 2,
            errors: 0,
            unreferenced: 0,
            corrupted: 0,
            unreadable: 0,
            corrupted_hashes: Vec::new(),
            started_at: None,
            eta_seconds: None,
        });
        run_cas_audit(
            tmp.path().to_path_buf(),
            tmp.path().join("manifests"),
            &state,
        );

        let st = state.read().expect("lock");
        assert_eq!(st.status, "done");
        assert_eq!(st.files_checked, 2);
        assert_eq!(st.errors, 1);
        // 0810: the one bad file is CORRUPTED (hash mismatch), not unreadable.
        assert_eq!(st.corrupted, 1);
        assert_eq!(st.unreadable, 0);
        assert_eq!(st.corrupted_hashes.len(), 1);
        assert_eq!(
            st.corrupted_hashes[0],
            "0139f07e8d5e8d5c41f32a9e6c3d9f1e0d5b8a2c7e4f6a9d1b3c5e7f9a0b2d4e6f"
        );
        // No live manifests -> both files are unreferenced (report only).
        assert_eq!(st.unreferenced, 2);
    }

    #[test]
    fn vanished_file_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let f = write_valid_cas(tmp.path(), b"vanishing");

        // First read succeeds -> Ok.
        assert!(matches!(verify_cas_file(&f), VerifyResult::Ok));
        // Delete it; the retry path must report Gone, not an error.
        std::fs::remove_file(&f).expect("remove");
        assert!(matches!(verify_cas_file(&f), VerifyResult::Gone));
    }

    #[test]
    fn empty_cas_root_is_done_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Local state container: parallel tests never race on a shared global.
        let state = RwLock::new(CasAuditState {
            status: "running".to_string(),
            files_checked: 0,
            files_total: 0,
            errors: 0,
            unreferenced: 0,
            corrupted: 0,
            unreadable: 0,
            corrupted_hashes: Vec::new(),
            started_at: None,
            eta_seconds: None,
        });
        run_cas_audit(
            tmp.path().to_path_buf(),
            tmp.path().join("manifests"),
            &state,
        );
        let st = state.read().expect("lock");
        assert_eq!(st.status, "done");
        assert_eq!(st.files_checked, 0);
        assert_eq!(st.errors, 0);
    }
}
