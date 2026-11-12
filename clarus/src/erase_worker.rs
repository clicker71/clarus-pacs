// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Lazy erase worker - the non-blocking file eraser.
//!
//! Deleting a study via `POST /admin/v1/instances/delete` only *tombstones* it:
//! the admin thread renames `{study}.manifest -> {study}.del` (instant
//! visibility, readers filter `.manifest`) and removes the SQLite rows. It
//! NEVER touches the CAS files. Physical erasure is this background
//! reconciler's job, on a cold path, so the admin never blocks on disk I/O.
//!
//! # Algorithm - idempotent reconciler
//! 1. Collect every `*.del` tombstone in the manifests dir.
//! 2. One pass over all *live* `*.manifest` files, building a `HashSet` of
//!    referenced CAS hashes (BLAKE3) - the refcount, computed
//!    on demand; a batched scan amortizes O(N manifests) over many erases.
//! 3. For each tombstone instance hash not referenced by any live manifest,
//!    unlink the BLAKE3 CAS path.
//!    `NotFound` is success (already erased). On a retryable OS error (e.g. a
//!    Windows sharing violation from an open handle) retry with exponential
//!    backoff 1s -> 2s -> ... cap 60s; if it still fails, keep the tombstone
//!    and retry on the next pass - the gauge stays high.
//! 4. Once all files of a tombstone are erased, lift it (`remove_file(.del)`),
//!    closing the lifecycle so the UID can be re-ingested.
//!
//! # Crash-replay (Q3)
//! The tombstones ARE the durable queue: a crash after rename leaves `.del` on
//! disk (still-deleted visibility), the worker re-runs on restart. A crash
//! mid-unlink is idempotent (`NotFound` ignored). A crash after unlink before
//! lifting `.del` is also idempotent - the refcount is 0 again, unlink is a
//! no-op, and the tombstone is lifted.
//!
//! # Tier handling
//! Only filesystem-tier files (hot, and cold when `cold_cas` is present) are
//! unlinked here. S3-tier unlink is deferred to a separate F-stage task; a
//! non-hot/non-cold instance is skipped with a WARN (the tombstone is lifted
//! once its hot/cold files are gone - visibility is already correct; the S3
//! object lifecycle is the bucket's job).

use crate::state::AppState;
use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::manifest::{TIER_COLD, TIER_HOT, read_manifest};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Test hook: when `true`, the worker skips the reconciliation pass (but keeps
/// the loop alive). Used by `erase_smoke` to hold deletion while asserting that
/// the tombstone itself already hides the study (visibility gate), then to
/// release it and confirm the files are erased.
pub static ERASE_PAUSE: AtomicBool = AtomicBool::new(false);

/// Poll interval for the reconciler loop.
const SCAN_INTERVAL: Duration = Duration::from_millis(500);

/// Per-file retry budget: 1s -> 2s -> 4s ... capped at 60s per delay.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Next delay before the erase-worker's next pass, given whether the last
/// pass had retryable errors and the delay used on the previous iteration.
///
/// Error passes escalate `SCAN_INTERVAL -> 1s -> 2s -> ...` capped at
/// [`MAX_BACKOFF`] (60s), so a permanently-unreachable manifests dir does not
/// flood the `WARN`/`DEBUG` lines every 500ms. A clean pass (0 errors) reverts
/// to the nominal [`SCAN_INTERVAL`]. Pure - tested without real sleeping.
fn next_outer_delay(had_errors: bool, prev: Duration) -> Duration {
    if had_errors {
        (prev * 2).min(MAX_BACKOFF)
    } else {
        SCAN_INTERVAL
    }
}

/// Report of one reconciliation pass.
#[derive(Debug, Default, Clone)]
pub struct EraseStats {
    /// Tombstones (`.del`) found on the manifests dir.
    pub tombstones: u64,
    /// Files physically removed (blake3 + legacy copies).
    pub files_removed: u64,
    /// Bytes freed (sum of removed `.dcm` sizes).
    pub bytes_freed: u64,
    /// Files this PASS unlinked. Differs from `files_removed` across passes:
    /// that one accumulates for the whole run, while an `erase_complete`
    /// audit row answers for one tombstone. Counting the tombstone's listed
    /// files instead reported files that were already gone as reclaimed.
    pub files_unlinked: u64,
    /// Tombstones lifted (`.del` removed) this pass.
    pub lifted: u64,
    /// Tombstones/failures left to retry.
    pub errors: u64,
}

/// A single tombstone, opened from its `.del` path (which retains the manifest
/// bytes), plus every instance's (blake3, storage tier, file_size) and the
/// audit batch_id sidecar (if present) used to close the audit loop.
struct Tombstone {
    del_path: std::path::PathBuf,
    batch_path: Option<std::path::PathBuf>,
    batch_id: Option<String>,
    files: Vec<(String, u8, u64)>,
}

/// Run one full reconciliation pass over the manifests dir. Cold-path only.
pub fn erase_once(state: &AppState) -> EraseStats {
    let manifest_dir = Path::new(&state.config.btree.manifest_dir);
    let mut stats = EraseStats::default();

    let (tombstones, undecodable) = match collect_tombstones(manifest_dir) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A fresh store (first boot or a wiped data dir) has no manifests
            // dir yet - that is an empty reconcile, not an error: no WARN and
            // no backoff escalation.
            set_pending_gauge(0);
            return EraseStats::default();
        }
        Err(e) => {
            crate::clog_warn!(error = %e, "Erase worker: cannot read manifests dir");
            set_pending_gauge(0);
            // A missing/unreachable manifests dir must escalate the outer-loop
            // backoff (SCAN_INTERVAL -> 1s -> 2s -> ... capped), not constantly
            // reset it to 500ms. err != 0 drives next_outer_delay.
            stats.errors = 1;
            return stats;
        }
    };
    stats.tombstones = tombstones.len() as u64 + undecodable;
    crate::metrics::ERASE_PENDING.store(stats.tombstones, Ordering::Relaxed);
    // An undecodable `.del` keeps its PAS files on disk; the pass must be
    // reported as failing so the outer loop backs off instead of claiming a
    // clean reconcile (PS3.15 E.1.1 - the delete must reach the media).
    stats.errors += undecodable;

    if tombstones.is_empty() {
        return stats;
    }

    // Refcount: all hashes referenced by any live manifest.
    let referenced = match collect_referenced(manifest_dir) {
        Ok(r) => r,
        Err(e) => {
            crate::clog_warn!(error = %e, "Erase worker: cannot scan live manifests");
            stats.errors = 1;
            return stats;
        }
    };

    for ts in &tombstones {
        let unlinked_before = stats.files_unlinked;
        let all_removed = ts.files.iter().all(|(blake3, tier, size)| {
            unlink_pair(state, *tier, blake3, *size, &referenced, &mut stats)
        });

        // 2808 P1: reclaim blobs orphaned by update-in-place (recorded in the
        // per-study sidecar) when the study itself is erased. Refcount-checked
        // exactly like the tombstone-listed files; a retryable failure keeps
        // the tombstone (and the sidecar) for the next pass.
        let orphans_drained = all_removed
            && drain_orphan_sidecar(state, manifest_dir, &ts.del_path, &referenced, &mut stats);

        // 3008: the COERCE audit copies of the same study are reclaimed the
        // same way - they are never manifest-referenced, so the per-study
        // `{study}.audited` sidecar is their only reclaim path.
        let audited_drained = orphans_drained
            && drain_audit_sidecar(state, manifest_dir, &ts.del_path, &referenced, &mut stats);

        if all_removed && orphans_drained && audited_drained {
            // The figures for THIS tombstone, not the pass totals. `stats`
            // accumulates across every tombstone in the pass, so the audit row
            // used to attribute the whole batch to whichever tombstone closed
            // last - a row that names a batch but reports numbers from other
            // batches answers the wrong question and cannot be reconciled.
            // The count is what this pass actually unlinked: a file that was
            // already gone (an earlier pass, a manual unlink) satisfied
            // `all_removed` without contributing to the reclaim, and reporting
            // the tombstone's listed length credited this erase with work it
            // did not do. The bytes keep coming from the listed sizes, which
            // is the storage the study occupied either way.
            let ts_files = stats.files_unlinked - unlinked_before;
            let ts_bytes: u64 = ts.files.iter().map(|(_, _, size)| size).sum();

            // Close the audit loop: `erase_complete {batch_id, files, bytes}`
            // links back to the `delete` row the admin wrote before the
            // tombstone. Best-effort - a missing sidecar only loses the
            // correlation, never blocks the erase.
            if let Some(batch_id) = &ts.batch_id {
                let _ = crate::audit_log::append_record(
                    Path::new(&state.config.btree.audit_dir),
                    &crate::audit_log::AuditEvent::EraseComplete {
                        batch_id: batch_id.clone(),
                        files: ts_files,
                        bytes: ts_bytes,
                    },
                );
            }
            if let Some(batch_path) = &ts.batch_path {
                let _ = std::fs::remove_file(batch_path);
            }
            match std::fs::remove_file(&ts.del_path) {
                Ok(()) => {
                    stats.lifted += 1;
                    // Operational mirror of the audit `erase_complete` row:
                    // tombstone name carries the study UID; the reason lives
                    // in the audit file behind the batch_id.
                    crate::clog_event!(
                        crate::clog::Level::Info,
                        "ERASE_COMPLETE",
                        "",
                        "study erase complete tombstone={} files={} bytes={}",
                        ts.del_path.display(),
                        ts_files,
                        ts_bytes
                    );
                }
                Err(e) => {
                    crate::clog_warn!(
                        error = %e, tombstone = %ts.del_path.display(),
                        "Erase worker: failed to lift tombstone (will retry)"
                    );
                    stats.errors += 1;
                }
            }
        }
    }

    stats
}

/// Unlink the blake3 CAS path for one instance, honoring its storage tier.
/// Returns true when the instance is fully erased (or nothing is left to
/// erase); false keeps the tombstone for a later retry.
fn unlink_pair(
    state: &AppState,
    tier: u8,
    blake3: &str,
    file_size: u64,
    referenced: &HashSet<String>,
    stats: &mut EraseStats,
) -> bool {
    // A still-referenced hash must never be removed (dedup safety).
    if referenced.contains(blake3) {
        return true; // referenced by a live study - nothing to remove here
    }

    // S3-tier object deletion is a separate F-stage task - skip, not error.
    let Some(cas) = cas_for_tier(state, tier) else {
        crate::clog_warn!(tier, blake3 = %blake3, "Erase worker: physical erase deferred for tier (S3/unknown)");
        return true;
    };

    unlink_one(cas, blake3, file_size, stats)
}

/// The CAS root for a storage tier, or `None` when erasure is deferred (S3
/// objects are deleted by a separate F-stage task, never by this worker).
fn cas_for_tier(state: &AppState, tier: u8) -> Option<&ContentAddressedStorage> {
    match tier {
        TIER_HOT => Some(&state.cas),
        TIER_COLD => state.cold_cas.as_ref(),
        _ => None, // TIER_S3 and unknown tiers: deferred.
    }
}

/// Remove the CAS path for `hash`, counting a success against `stats`. The
/// legacy sha256 copy shares the blake3's size, so `file_size` is added only
/// for the primary (blake3) unlink (pass `0` for the legacy copy).
/// `NotFound` is success (already erased). Returns false on retryable error
/// (open handle), leaving the tombstone for the next pass.
fn unlink_one(
    cas: &ContentAddressedStorage,
    hash: &str,
    file_size: u64,
    stats: &mut EraseStats,
) -> bool {
    let path = cas.cas_path(hash);
    let mut backoff = Duration::from_secs(1);
    let start = Instant::now();
    loop {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                // The primary (blake3) unlink carries the size; the legacy
                // sha256 copy passes 0 because it shares the same blob's size
                // and counting it twice would inflate the freed total.
                stats.files_removed += 1;
                stats.bytes_freed += file_size;
                stats.files_unlinked += 1;
                return true;
            }
            // NotFound means "already erased", which is success for the
            // tombstone but NOT a file this pass removed: the audit row and
            // the operator log report what the erase actually reclaimed, and
            // an object that vanished earlier (a previous pass, or a manual
            // unlink) is not part of that answer.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(e) => {
                crate::clog_warn!(
                    hash = %hash,
                    path = %path.display(),
                    error = %e,
                    backoff_ms = backoff.as_millis(),
                    "Erase worker: unlink failed (open handle?), backing off"
                );
                if start.elapsed() + backoff > MAX_BACKOFF {
                    return false;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// The audit-copy sidecar path `manifests/{study_uid}.audited` (3008).
///
/// Every COERCE alibi audit copy written for an INSERTED instance appends
/// its `hex tier` line here (no hot-path deletion); the erase worker drains
/// it when the study tombstone is lifted, reclaiming alibi blobs that no
/// manifest ever references. The `.audited` suffix is invisible to every
/// `.manifest`-suffixed reader.
pub(crate) fn audit_copy_sidecar_path(manifests_dir: &Path, study_uid: &str) -> PathBuf {
    manifests_dir.join(format!("{study_uid}.audited"))
}

/// Drain the per-study audit-copy sidecar (3008): every `hex tier` line is
/// unlinked refcount-checked - the same `referenced` guard as the orphan
/// sidecar, so a copy whose hash equals a LIVE manifest blob of another
/// study is kept. Returns false on a retryable failure (keeps the tombstone
/// alive for the next pass); a missing sidecar is success.
fn drain_audit_sidecar(
    state: &AppState,
    manifest_dir: &Path,
    del_path: &Path,
    referenced: &HashSet<String>,
    stats: &mut EraseStats,
) -> bool {
    let study_uid = match del_path.file_stem().and_then(|s| s.to_str()) {
        Some(uid) => uid,
        None => return true,
    };
    let sidecar = audit_copy_sidecar_path(manifest_dir, study_uid);
    let Ok(text) = std::fs::read_to_string(&sidecar) else {
        return true; // no audit copies recorded - nothing to drain
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((hex, tier)) = line.split_once(' ') else {
            crate::clog_warn!(line = %line, "erase worker: malformed audit sidecar line");
            continue;
        };
        let Ok(tier) = tier.parse::<u8>() else {
            crate::clog_warn!(line = %line, "erase worker: malformed audit sidecar tier");
            continue;
        };
        if !unlink_pair(state, tier, hex, 0, referenced, stats) {
            return false;
        }
    }
    // All recorded audit copies are gone (or unreferenced-and-unlinked); the
    // sidecar goes with the tombstone. NotFound is fine (first erase pass).
    match std::fs::remove_file(&sidecar) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            crate::clog_warn!(error = %e, "erase worker: failed to remove audit sidecar");
            false
        }
    }
}

/// Read every `*.del` tombstone in `manifest_dir`, decoding each into its
/// instance hash pairs. A `.del` we cannot decode is NOT returned: an empty
/// file list would make `erase_once`'s `files.iter().all(..)` vacuously true
/// and lift the tombstone, leaving the PAS blobs on disk with no record of
/// them - a silent deletion-compliance failure. The count comes back with the
/// result so the caller can mark the pass as failed and retry.
fn collect_tombstones(manifest_dir: &Path) -> std::io::Result<(Vec<Tombstone>, u64)> {
    let mut out = Vec::new();
    let mut undecodable = 0u64;
    for entry in std::fs::read_dir(manifest_dir)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().into_owned();
        if !fname.ends_with(".del") {
            continue;
        }
        let del_path = entry.path();
        let m = match read_manifest(&del_path) {
            Ok(m) => m,
            Err(e) => {
                crate::clog_warn!(
                    error = %e, tombstone = %del_path.display(),
                    "Erase worker: cannot decode tombstone manifest (kept in place)"
                );
                undecodable += 1;
                continue;
            }
        };
        let files: Vec<(String, u8, u64)> = m
            .series
            .iter()
            .flat_map(|s| &s.instances)
            .map(|i| {
                (
                    clarus_adapters::hex_encode(&i.blake3),
                    i.storage_tier,
                    i.file_size,
                )
            })
            .collect();
        // Audit batch sidecar (written by the admin at tombstone time).
        let study = fname.trim_end_matches(".del");
        let batch_path = manifest_dir.join(format!("{study}.batch"));
        let batch_id = std::fs::read_to_string(&batch_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // Even a manifest with zero instances yields a liftable tombstone.
        out.push(Tombstone {
            del_path,
            batch_path: if batch_id.is_some() {
                Some(batch_path)
            } else {
                None
            },
            batch_id,
            files,
        });
    }
    Ok((out, undecodable))
}

/// Drain the per-study orphan sidecar (2808 P1): every `hex tier` line is
/// unlinked refcount-checked. Returns false on a retryable failure (keeps the
/// tombstone alive for the next pass); a missing sidecar is success.
fn drain_orphan_sidecar(
    state: &AppState,
    manifest_dir: &Path,
    del_path: &Path,
    referenced: &HashSet<String>,
    stats: &mut EraseStats,
) -> bool {
    let study_uid = match del_path.file_stem().and_then(|s| s.to_str()) {
        Some(uid) => uid,
        None => return true,
    };
    let sidecar = clarus_core::stow::orphan_sidecar_path(manifest_dir, study_uid);
    let Ok(text) = std::fs::read_to_string(&sidecar) else {
        return true; // no orphans recorded - nothing to drain
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((hex, tier)) = line.split_once(' ') else {
            crate::clog_warn!(line = %line, "erase worker: malformed orphan sidecar line");
            continue;
        };
        let Ok(tier) = tier.parse::<u8>() else {
            crate::clog_warn!(line = %line, "erase worker: malformed orphan sidecar tier");
            continue;
        };
        if !unlink_pair(state, tier, hex, 0, referenced, stats) {
            return false;
        }
    }
    // All recorded orphans are gone (or unreferenced-and-unlinked); the
    // sidecar can go with the tombstone. NotFound is fine (first erase pass).
    match std::fs::remove_file(&sidecar) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            crate::clog_warn!(error = %e, "erase worker: failed to remove orphan sidecar");
            false
        }
    }
}

/// One pass over live manifests, returning the set of referenced CAS hashes.
pub(crate) fn collect_referenced(manifest_dir: &Path) -> std::io::Result<HashSet<String>> {
    Ok(collect_referenced_counted(manifest_dir)?.0)
}

/// [`collect_referenced`] plus the logical instance count: every manifest
/// instance record, no hash dedup. The count is the "honest instances"
/// number surfaced on `/health/status` (3008 viewer split of the OBJ gauge).
pub(crate) fn collect_referenced_counted(
    manifest_dir: &Path,
) -> std::io::Result<(HashSet<String>, u64)> {
    let mut set = HashSet::new();
    let mut instances: u64 = 0;
    for entry in std::fs::read_dir(manifest_dir)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().into_owned();
        if !fname.ends_with(".manifest") {
            continue;
        }
        let path = entry.path();
        // A manifest that cannot be decoded stops the whole scan instead of
        // being skipped: returning a set that is missing one study's hashes
        // makes every blob of that study look unreferenced, and the erase pass
        // would then unlink images of a study still in the archive. Deferring
        // costs an erase pass; guessing costs an instance.
        let m = read_manifest(&path).map_err(|e| {
            std::io::Error::other(format!(
                "erase worker: unreadable live manifest {}: {e}",
                path.display()
            ))
        })?;
        for series in &m.series {
            instances += series.instances.len() as u64;
            for i in &series.instances {
                set.insert(clarus_adapters::hex_encode(&i.blake3));
            }
        }
    }
    Ok((set, instances))
}

fn set_pending_gauge(n: u64) {
    crate::metrics::ERASE_PENDING.store(n, Ordering::Relaxed);
}

// -- 3008 orphan GC (background only, R6) ---------------------------------

/// One orphan-GC pass: read `*.orphans` sidecars of LIVE studies, unlink each
/// recorded blob refcount-checked via [`unlink_pair`] (same retry/backoff),
/// remove a sidecar once fully drained, keep it on a retryable failure.
/// NEVER touches `*.audited` (hidden-PHI alibis wait for study erase) and
/// never touches `.del` tombstones.
///
/// Returns `Some((files_removed, pending_after_pass))` where the pending
/// count is a FRESH recount of the remaining sidecar lines (drift
/// correction), or `None` when the pass aborted (manifests dir unreadable) -
/// in that case the caller must leave the gauge untouched.
pub fn gc_orphans(state: &AppState, manifest_dir: &Path) -> Option<(u64, u64)> {
    // A skipped pass must NEVER fall back to an empty reference set: every
    // recorded blob would look unreferenced and be unlinked. Abort instead and
    // let the caller keep the gauge where it is.
    let referenced = match collect_referenced(manifest_dir) {
        Ok(set) => set,
        Err(e) => {
            crate::clog_warn!(
                manifest_dir = %manifest_dir.display(),
                error = %e,
                "orphan GC: reference scan failed, pass skipped"
            );
            return None;
        }
    };
    let entries = std::fs::read_dir(manifest_dir).ok()?;
    let mut stats = EraseStats::default();
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if !fname.ends_with(".orphans") {
            continue;
        }
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // unreadable: kept for the next pass
        };
        let mut fully_drained = true;
        let mut remaining: Vec<&str> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((hex, tier)) = line.split_once(' ') else {
                crate::clog_warn!(line = %line, "orphan GC: malformed sidecar line");
                continue;
            };
            let Ok(tier) = tier.parse::<u8>() else {
                crate::clog_warn!(line = %line, "orphan GC: malformed sidecar tier");
                continue;
            };
            // A hash referenced by a LIVE manifest is shared with another
            // study - keep the line (the sidecar stays until the blob is
            // truly reclaimable), never unlink shared bytes.
            if referenced.contains(hex) {
                remaining.push(line);
                fully_drained = false;
                continue;
            }
            if !unlink_pair(state, tier, hex, 0, &referenced, &mut stats) {
                remaining.push(line);
                fully_drained = false;
            }
        }
        if fully_drained {
            let _ = std::fs::remove_file(&path);
        } else {
            // Rewrite the sidecar with ONLY the not-yet-reclaimed lines so the
            // end-of-pass recount matches reality (already-unlinked lines do
            // not linger on disk).
            let mut rewrite = String::new();
            for l in &remaining {
                rewrite.push_str(l);
                rewrite.push('\n');
            }
            let _ = std::fs::write(&path, rewrite);
        }
    }
    // Fresh recount of the lines still recorded (corrects any drift; 0 after
    // a clean pass).
    let mut pending: u64 = 0;
    for entry in std::fs::read_dir(manifest_dir).ok()?.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if !fname.ends_with(".orphans") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            pending += text.lines().filter(|l| !l.trim().is_empty()).count() as u64;
        }
    }
    Some((stats.files_removed, pending))
}

/// One scheduled GC pass plus the gauge correction (the erase-worker loop's
/// per-interval unit). `Some` only when the pass completed; the gauge is
/// stored with the freshly counted pending total (0 after a clean pass).
pub fn run_orphan_gc(state: &AppState, manifest_dir: &Path) -> Option<(u64, u64)> {
    let (removed, pending) = gc_orphans(state, manifest_dir)?;
    crate::metrics::ORPHANS_PENDING.store(pending, Ordering::Relaxed);
    Some((removed, pending))
}

/// Spawn the background erase-worker thread (modeled on the journal-flush and
/// lazy-recovery std threads in main.rs). Runs forever; honors `ERASE_PAUSE`.
///
/// The outer loop applies an exponential backoff while passes keep failing
/// (errors > 0), resetting to the nominal [`SCAN_INTERVAL`] on a clean pass -
/// otherwise a permanently-unreachable manifests dir would WARN once per
/// 500ms. The per-file backoff inside `unlink_one` is unchanged.
pub fn spawn_erase_worker(state: AppState) {
    std::thread::Builder::new()
        .name("erase-worker".into())
        .spawn(move || {
            crate::clog_info!("Erase worker started (cold path, non-blocking)");
            let mut last_delay = SCAN_INTERVAL;
            // 3008: scheduled orphan GC ([btree] orphan_gc_interval_sec, 0 =
            // off). First pass one interval after startup; honors ERASE_PAUSE
            // exactly like the erase pass.
            let gc_interval = Duration::from_secs(state.config.btree.orphan_gc_interval_sec);
            let mut last_gc = Instant::now();
            let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
            loop {
                if !ERASE_PAUSE.load(Ordering::Relaxed) {
                    let stats = erase_once(&state);
                    if stats.errors > 0 {
                        crate::clog_debug!(
                            tombstones = stats.tombstones,
                            removed = stats.files_removed,
                            errors = stats.errors,
                            "Erase worker: pass complete with retries pending"
                        );
                    }
                    last_delay = next_outer_delay(stats.errors > 0, last_delay);
                    if !gc_interval.is_zero() && last_gc.elapsed() >= gc_interval {
                        if let Some((removed, pending)) = run_orphan_gc(&state, &manifest_dir) {
                            crate::clog_debug!(
                                removed = removed,
                                pending = pending,
                                "Orphan GC pass complete"
                            );
                        }
                        last_gc = Instant::now();
                    }
                }
                std::thread::sleep(last_delay);
            }
        })
        .expect("Failed to spawn erase worker thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_pass_keeps_scan_interval() {
        // A normal empty pass (0 errors) must never back off.
        assert_eq!(
            next_outer_delay(false, SCAN_INTERVAL),
            SCAN_INTERVAL,
            "clean pass from nominal interval stays nominal"
        );
    }

    #[test]
    fn first_error_escalates_to_one_second() {
        // First failing pass from the nominal interval -> 1s (500ms * 2).
        assert_eq!(
            next_outer_delay(true, SCAN_INTERVAL),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn error_backoff_doubles_then_caps_at_max() {
        let mut prev = next_outer_delay(true, SCAN_INTERVAL); // 1s
        let mut seen = vec![prev];
        for _ in 0..8 {
            prev = next_outer_delay(true, prev);
            seen.push(prev);
        }
        // Doubling 1s -> 2s -> 4s ... until the 60s cap, then stays capped.
        assert_eq!(seen[0], Duration::from_secs(1));
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[2], Duration::from_secs(4));
        assert_eq!(seen[5], Duration::from_secs(32));
        assert_eq!(seen[6], Duration::from_secs(60), "cap at MAX_BACKOFF");
        assert_eq!(seen[7], Duration::from_secs(60), "stays capped");
        assert_eq!(seen[8], Duration::from_secs(60), "stays capped");
    }

    #[test]
    fn clean_pass_resets_after_backoff() {
        // A clean pass reverts immediately to SCAN_INTERVAL, even after the
        // loop had escalated into the minutes-range backoff.
        assert_eq!(
            next_outer_delay(false, Duration::from_secs(30)),
            SCAN_INTERVAL
        );
        assert_eq!(next_outer_delay(false, MAX_BACKOFF), SCAN_INTERVAL);
    }

    #[test]
    fn missing_manifests_dir_is_not_found_classification() {
        // Fresh-store classification: erase_once treats a missing manifests
        // dir as an empty reconcile (no WARN, no backoff escalation).
        let err = match collect_tombstones(std::path::Path::new(
            "__no_such_manifests_dir_for_erase_test__",
        )) {
            Ok(_) => panic!("expected NotFound for a missing manifests dir"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// A live manifest that cannot be decoded must fail the scan, not thin it.
    ///
    /// A skipped manifest answers "no blob is referenced by this study", so
    /// the erase pass would unlink the stored pixels of a study the archive
    /// still lists - the delete was asked for one study, the unlink would take
    /// another. The scan has to refuse instead.
    #[test]
    fn an_unreadable_live_manifest_fails_the_reference_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("1.2.3.study.manifest"), b"\xFF\xFF\xFF\xFF")
            .expect("corrupt manifest");
        std::fs::write(dir.path().join("1.2.4.study.manifest"), b"\xFF\xFF\xFF\xFF")
            .expect("corrupt manifest");

        let err = collect_referenced_counted(dir.path())
            .expect_err("a corrupt live manifest must fail the scan");
        assert!(
            err.to_string().contains("unreadable live manifest"),
            "the error names the cause, got: {err}"
        );
        assert!(
            collect_referenced(dir.path()).is_err(),
            "the set-returning form must not answer with a partial set"
        );
    }

    /// An empty store is still a successful scan (no tombstones, nothing
    /// referenced) - the refusal above must not turn "nothing stored" into an
    /// error.
    #[test]
    fn an_empty_manifest_directory_is_a_complete_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (set, instances) =
            collect_referenced_counted(dir.path()).expect("an empty store scans cleanly");
        assert!(set.is_empty());
        assert_eq!(instances, 0);
    }
}
