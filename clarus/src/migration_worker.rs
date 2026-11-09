// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Storage tier migration worker (FR-017b).
//!
//! Three independent background std::threads:
//! 1. **Planned cold migration** - cron-driven, moves old studies to NAS/HDD.
//! 2. **Planned S3 migration** - cron-driven, moves old studies to cloud.
//! 3. **Emergency migration** - interval-driven, frees space when disk is full.
//!
//! # Migration order (nginx-way: fast local first, slow remote second)
//! - Planned: cold_path -> S3 (cold is faster, so migrate there first).
//! - Emergency: cold_path -> S3 (fallback if cold is unavailable).
//!
//! # Safety
//! - Never deletes hot file until cold copy is verified (size check).
//! - If target is unreachable, batch is aborted - hot files are NOT deleted.
//! - If hot file is already gone (ref_count=0, GC'd), just updates DB tier.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::manifest::{AttrValue, read_manifest, write_manifest};
use clarus_core::query::tag;
use clarus_core::stow::acquire;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::state::AppState;
use crate::storage_tiers::CronSchedule;

/// Rewrite `storage_tier` in every live manifest whose instance references the
/// blob (matched by BLAKE3, the CAS key). The physical file relocation
/// (copy->verify) happens in the caller; this only updates the manifest
/// metadata so reads (WADO/QIDO) resolve the new tier.
fn set_storage_tier(state: &AppState, blake3: &str, tier: &str) {
    let tier_byte = match tier {
        "cold" => clarus_core::manifest::TIER_COLD,
        "s3" => clarus_core::manifest::TIER_S3,
        _ => clarus_core::manifest::TIER_COLD,
    };
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let mut matched = 0usize;
    let Ok(rd) = std::fs::read_dir(manifest_dir) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Ok(m) = read_manifest(&ent.path()) else {
            continue;
        };
        let mut touched = false;
        for inst in m.series.iter().flat_map(|s| s.instances.iter()) {
            let b3 = clarus_adapters::hex_encode(&inst.blake3);
            if b3 == blake3 && inst.storage_tier != tier_byte {
                touched = true;
                break;
            }
        }
        if touched {
            // Serialize against a concurrent STOW commit into the same study.
            let _lease = acquire();
            // TOCTOU fix: the pre-lease copy is
            // stale - re-read UNDER the lease and apply the tier change to
            // the FRESH copy so a concurrent commit survives.
            let Ok(mut fresh) = read_manifest(&ent.path()) else {
                continue;
            };
            for inst in fresh.series.iter_mut().flat_map(|s| s.instances.iter_mut()) {
                let b3 = clarus_adapters::hex_encode(&inst.blake3);
                if b3 == blake3 && inst.storage_tier != tier_byte {
                    inst.storage_tier = tier_byte;
                    matched += 1;
                }
            }
            // B3: drop the cached entry BEFORE the rewrite; reinstall after.
            let study_uid = name.trim_end_matches(".manifest");
            let (cache_v, _taken) = clarus_core::manifest_cache::begin_write(study_uid);
            if let Err(e) = write_manifest(&ent.path(), &fresh) {
                crate::clog_warn!(blake3, tier, error = %e, "Migration: manifest tier rewrite failed");
            } else {
                clarus_core::manifest_cache::end_write(
                    study_uid,
                    cache_v,
                    std::sync::Arc::new(fresh),
                    std::fs::metadata(ent.path())
                        .map(|md| md.len())
                        .unwrap_or(0),
                    manifest_dir,
                );
            }
        }
    }
    if matched == 0 {
        crate::clog_debug!(
            blake3,
            tier,
            "Migration: no manifest instance matched for retier"
        );
    }
}

// -- Public entry point --------------------------------------------------

/// Spawn all migration workers as independent std::threads.
///
/// Called once at server startup. Returns immediately - all work is
/// done in background threads. The schedule is read from `state.config.migration`.
pub fn spawn_migration_workers(state: AppState) {
    let schedule = state.config.migration.clone();

    if !schedule.is_any_enabled() {
        crate::clog_debug!("No migration tiers configured - worker not started.");
        return;
    }

    crate::clog_info!(
        emergency_free_gb = schedule.emergency_free_gb,
        emergency_check_secs = schedule.emergency_check_secs,
        cold_enabled = schedule
            .cold
            .as_ref()
            .map(|b| b.is_enabled())
            .unwrap_or(false),
        s3_enabled = {
            #[cfg(feature = "s3")]
            {
                schedule
                    .s3
                    .as_ref()
                    .map(|b| b.is_enabled())
                    .unwrap_or(false)
            }
            #[cfg(not(feature = "s3"))]
            {
                false
            }
        },
        "Migration workers starting"
    );

    // -- 1. Planned cold migration (NAS/HDD) ----------------------
    if let Some(ref cold) = schedule.cold
        && cold.is_enabled()
    {
        let state = state.clone();
        let min_age_days = cold.min_age_days;
        let cron = cold.schedule.clone().unwrap(); // safe: is_enabled() checks this
        std::thread::Builder::new()
            .name("migration-cold".into())
            .spawn(move || {
                planned_cold_loop(state, cron, min_age_days);
            })
            .expect("Failed to spawn cold migration thread");
    }

    // -- 2. Planned S3 migration (cloud) --------------------------
    #[cfg(feature = "s3")]
    if let Some(ref s3) = schedule.s3
        && s3.is_enabled()
    {
        let state = state.clone();
        let min_age_days = s3.min_age_days;
        let cron = s3.schedule.clone().unwrap();
        std::thread::Builder::new()
            .name("migration-s3".into())
            .spawn(move || {
                planned_s3_loop(state, cron, min_age_days);
            })
            .expect("Failed to spawn S3 migration thread");
    }

    // -- 3. Emergency migration (disk-full protection) ------------
    if schedule.emergency_free_gb > 0 {
        let state = state.clone();
        let free_gb_threshold = schedule.emergency_free_gb;
        let check_secs = schedule.emergency_check_secs;
        std::thread::Builder::new()
            .name("migration-emergency".into())
            .spawn(move || {
                emergency_loop(state, free_gb_threshold, check_secs);
            })
            .expect("Failed to spawn emergency migration thread");
    }
}

// -- Planned cold migration loop -----------------------------------------

/// Cron-driven cold migration: move old studies hot -> NAS/HDD.
fn planned_cold_loop(state: AppState, cron: CronSchedule, min_age_days: u32) {
    crate::clog_info!(%cron, min_age_days, "Planned cold migration worker started");

    loop {
        let now = chrono::Local::now();
        match cron.next_after(&now) {
            Some(next) => {
                let sleep_secs = (next - now).num_seconds().max(1) as u64;
                crate::clog_debug!(
                    next_run = %next,
                    sleep_secs,
                    "Cold migration: sleeping until next cron tick"
                );
                std::thread::sleep(Duration::from_secs(sleep_secs));
            }
            None => {
                crate::clog_error!("Cold cron schedule has no future match - sleeping 1 hour");
                std::thread::sleep(Duration::from_secs(3600));
                continue;
            }
        }

        crate::clog_info!(min_age_days, "Cold migration: cron tick - starting batch");
        if let Err(e) = migrate_batch_cold(&state, min_age_days) {
            crate::clog_warn!("Cold migration batch failed: {e}. Will retry on next cron tick.");
        }
    }
}

// -- Planned S3 migration loop -------------------------------------------

/// Cron-driven S3 migration: move old studies hot -> S3 cloud.
/// If cold_path is also configured, reads from cold_path (not hot) for efficiency.
#[cfg(feature = "s3")]
fn planned_s3_loop(state: AppState, cron: CronSchedule, min_age_days: u32) {
    crate::clog_info!(%cron, min_age_days, "Planned S3 migration worker started");

    loop {
        let now = chrono::Local::now();
        match cron.next_after(&now) {
            Some(next) => {
                let sleep_secs = (next - now).num_seconds().max(1) as u64;
                crate::clog_debug!(
                    next_run = %next,
                    sleep_secs,
                    "S3 migration: sleeping until next cron tick"
                );
                std::thread::sleep(Duration::from_secs(sleep_secs));
            }
            None => {
                crate::clog_error!("S3 cron schedule has no future match - sleeping 1 hour");
                std::thread::sleep(Duration::from_secs(3600));
                continue;
            }
        }

        crate::clog_info!(min_age_days, "S3 migration: cron tick - starting batch");
        if let Err(e) = migrate_batch_s3(&state, min_age_days) {
            crate::clog_warn!("S3 migration batch failed: {e}. Will retry on next cron tick.");
            crate::metrics::record_s3_migration_error();
        }
    }
}

// -- Emergency migration loop ---------------------------------------------

/// Interval-driven emergency migration: free space when hot disk is low.
///
/// Ignores `min_age_days` - migrates the OLDEST studies first, regardless
/// of age, to free space as quickly as possible.
///
/// Priority: cold_path -> S3 (fallback).
fn emergency_loop(state: AppState, free_gb_threshold: u32, check_secs: u64) {
    crate::clog_info!(
        free_gb_threshold,
        check_secs,
        "Emergency migration worker started"
    );

    loop {
        std::thread::sleep(Duration::from_secs(check_secs));

        let hot_path = state.cas.root_dir();
        let free_gb = match get_free_space_gb(hot_path) {
            Ok(gb) => gb,
            Err(e) => {
                crate::clog_warn!("Cannot check free disk space: {e}");
                continue;
            }
        };

        if free_gb >= free_gb_threshold as u64 {
            crate::clog_debug!(
                free_gb,
                free_gb_threshold,
                "Disk space OK — no emergency migration needed"
            );
            continue;
        }

        crate::clog_warn!(
            free_gb,
            free_gb_threshold,
            "DISK SPACE LOW — starting emergency migration"
        );

        // Emergency: ignore age, get oldest studies first.
        if let Err(e) = migrate_batch_emergency(&state, free_gb_threshold) {
            crate::clog_error!("Emergency migration batch failed: {e}");
        }
    }
}

// -- Free space check -----------------------------------------------------

/// Get free disk space in GB for the given path.
/// Delegates to `crate::disk_utils::available_disk_mb`.
fn get_free_space_gb(path: &std::path::Path) -> Result<u64, String> {
    let mb = crate::disk_utils::available_disk_mb(path);
    if mb <= 0.0 {
        return Err("Cannot determine free disk space".to_string());
    }
    Ok((mb / 1024.0) as u64)
}

// -- Migration batch: cold (NAS/HDD) --------------------------------------

/// Migrate old studies from hot CAS to cold (file-system) CAS, ONE PATIENT AT
/// A TIME.
///
/// # OWNER INVARIANT 12.09 - patient atomicity
///
/// A patient is never left half-migrated. The batch unit is the PATIENT (all
/// HOT blobs across all of that patient's studies), and the phases below run
/// to completion for the whole batch before the next one starts:
///
/// 1. COPY    - every blob of the patient, streamed chunk by chunk.
/// 2. VERIFY  - every copied blob, before any metadata is touched.
/// 3. FLIP    - the tier of every matching instance, in one pass.
/// 4. DELETE  - the hot copies, only after the WHOLE patient is flipped.
///
/// The order is the invariant: DELETE is last, so no window exists in which a
/// blob has no readable copy. A failure in COPY or VERIFY aborts the patient -
/// cold partials are removed, hot and the tiers are untouched. A crash between
/// FLIP and DELETE leaves bytes in BOTH tiers, which `resolve_file` already
/// tolerates (it probes the declared tier then the opposite one), and the next
/// tick finishes the already-started patient idempotently.
fn migrate_batch_cold(state: &AppState, min_age_days: u32) -> Result<(), String> {
    let has_cold = state.cold_cas.is_some();
    if !has_cold {
        return Err("No cold storage configured (cold_path not set)".to_string());
    }

    let tick_start = std::time::Instant::now();
    let mut tick_patients = 0u64;
    let mut tick_migrated = 0u64;
    let mut tick_skipped = 0u64;
    let mut tick_errors = 0u64;

    let cutoff_date = cutoff_date_str(min_age_days);

    loop {
        // The cap counts PATIENTS. It cannot count blobs: truncating a patient
        // batch is exactly the partial migration the invariant forbids.
        let batches = collect_migration_patients(state, &cutoff_date, 50)?;
        if batches.is_empty() {
            break;
        }

        let chunk_start = std::time::Instant::now();
        let mut chunk_migrated = 0u64;
        let mut chunk_skipped = 0u64;
        // Patients that aborted in THIS chunk. `collect_migration_patients`
        // re-derives the same candidates from the same cutoff, so without this
        // set an aborted patient is handed out again on the very next round -
        // the loop rewrote it every chunk until the age cutoff moved.
        let mut chunk_aborted: Vec<String> = Vec::new();

        for batch in &batches {
            match migrate_patient_to_cold(state, batch) {
                PatientOutcome::Migrated(n) => {
                    chunk_migrated += n;
                    tick_patients += 1;
                }
                PatientOutcome::Skipped(n) => {
                    chunk_skipped += n;
                }
                PatientOutcome::Aborted(reason) => {
                    crate::clog_warn!(
                        patient = %batch.patient_id,
                        blobs = batch.blobs.len(),
                        reason = %reason,
                        "Cold migration: patient aborted - hot and tiers untouched"
                    );
                    tick_errors += 1;
                    chunk_aborted.push(batch.patient_id.clone());
                }
            }
        }

        // Nothing progressed in this pass: stop rather than spin. A patient
        // that aborted refused for a reason this tick cannot fix, so the next
        // collect would hand out the same set and rewrite it forever - which is
        // what happened while aborts advanced no counter at all.
        if chunk_migrated == 0 && chunk_skipped == 0 {
            if !chunk_aborted.is_empty() {
                crate::clog_debug!(
                    aborted = chunk_aborted.len(),
                    "Cold migration: chunk made no progress past aborts - ending tick"
                );
            }
            break;
        }

        tick_migrated += chunk_migrated;
        tick_skipped += chunk_skipped;
        crate::clog_debug!(
            patients = batches.len(),
            chunk_migrated,
            chunk_skipped,
            elapsed = chunk_start.elapsed().as_secs(),
            total = tick_migrated,
            "Cold migration chunk"
        );
    }

    // Cron tick complete - one line INFO
    let elapsed = tick_start.elapsed().as_secs();
    crate::clog_info!(
        migrated = tick_migrated,
        skipped = tick_skipped,
        errors = tick_errors,
        patients = tick_patients,
        elapsed,
        "Cold migration: {} instances, {} skipped, {} errors, {} patients - {}s",
        tick_migrated,
        tick_skipped,
        tick_errors,
        tick_patients,
        elapsed,
    );
    if tick_errors > 0 {
        crate::clog_warn!(
            errors = tick_errors,
            "Cold migration: {} patient(s) FAILED - check disk/NAS connectivity",
            tick_errors,
        );
    }
    Ok(())
}

/// What one patient's migration did, for the tick's accounting.
enum PatientOutcome {
    /// Every blob copied, verified, flipped and its hot copy removed.
    Migrated(u64),
    /// Every blob was already cold or its hot file was gone: nothing to do.
    Skipped(u64),
    /// The batch did not complete - hot and tiers are untouched.
    Aborted(String),
}

/// Migrate ONE patient: COPY all -> VERIFY all -> FLIP all -> DELETE hot.
///
/// Returns the number of blobs that changed tier (Migrated) or were already
/// done (Skipped). The function is the only place the phase ORDER is encoded,
/// and the order is the invariant - see the batch documentation above.
///
/// # Idempotency
/// Re-running on a patient that was interrupted after FLIP but before DELETE
/// works without special handling: the COPY phase re-reads the hot file (still
/// present, because DELETE never ran), VERIFY passes, FLIP finds the instances
/// already COLD and matches nothing, and DELETE finally removes the hot file.
/// Migrate ONE patient: COPY all -> VERIFY all -> FLIP all -> DELETE hot.
///
/// Returns the number of blobs that changed tier (Migrated) or were already
/// done (Skipped). The function is the only place the phase ORDER is encoded,
/// and the order is the invariant - see the batch documentation above.
///
/// # Idempotency
/// Re-running on a patient that was interrupted after FLIP but before DELETE
/// works without special handling: the COPY phase re-reads the hot file (still
/// present, because DELETE never ran), VERIFY passes, FLIP finds the instances
/// already COLD and matches nothing, and DELETE finally removes the hot file.
///
/// # Test seam
/// Exposed to the crate's integration tests (`cold_migration_atomic.rs`) so the
/// two-phase protocol can be driven against a real cold CAS without a running
/// server. It is the same function the cron loop calls.
pub fn migrate_patient_to_cold_for_test(
    state: &AppState,
    patient_id: &str,
    blobs: Vec<BlobTriple>,
) -> (u64, u64, Option<String>) {
    let batch = PatientBatch {
        patient_id: patient_id.to_string(),
        blobs: blobs
            .into_iter()
            .map(|(blake3, file_size, study_uid)| PatientBlob {
                blake3,
                file_size,
                study_uid,
            })
            .collect(),
    };
    match migrate_patient_to_cold(state, &batch) {
        PatientOutcome::Migrated(n) => (n, 0, None),
        PatientOutcome::Skipped(n) => (0, n, None),
        PatientOutcome::Aborted(reason) => (0, 0, Some(reason)),
    }
}

/// Run one cold batch. Test seam for the "no cold_path configured" contract:
/// the batch must REPORT the misconfiguration rather than treat it as a no-op,
/// because a silent no-op would look identical to "migration finished".
pub fn migrate_batch_cold_for_test(state: &AppState, min_age_days: u32) -> Result<(), String> {
    migrate_batch_cold(state, min_age_days)
}

/// Group the HOT candidates by patient and return them in batch order.
///
/// Test seam for the grouping rules (patient attribution, foreign exclusion,
/// determinism); the cron loop calls the same function.
pub fn collect_migration_patients_for_test(
    state: &AppState,
    cutoff_date: &str,
    max_patients: usize,
) -> Vec<(String, Vec<BlobTriple>)> {
    collect_migration_patients(state, cutoff_date, max_patients)
        .unwrap_or_default()
        .into_iter()
        .map(|batch| {
            (
                batch.patient_id,
                batch
                    .blobs
                    .into_iter()
                    .map(|b| (b.blake3, b.file_size, b.study_uid))
                    .collect(),
            )
        })
        .collect()
}

/// Whether a blob's hot file is absent but its cold copy is present.
///
/// A missing HOT file is only benign when the bytes are ALREADY in cold: that
/// is a resumed batch, where the previous pass copied and flipped but crashed
/// before the delete. If the blob is declared HOT and its bytes are on neither
/// tier, the patient is DAMAGED and the batch must NOT complete - completing it
/// would flip and delete the patient's other blobs and leave this one
/// unreadable, which is exactly the partial patient the invariant forbids.
fn hot_missing_is_resume(state: &AppState, blake3: &str, cold: &ContentAddressedStorage) -> bool {
    let _ = state;
    cold.cas_path(blake3).exists()
}

fn migrate_patient_to_cold(state: &AppState, batch: &PatientBatch) -> PatientOutcome {
    let Some(cold) = state.cold_cas.as_ref() else {
        return PatientOutcome::Aborted("cold storage disappeared".to_string());
    };

    // -- Phase 1: COPY (streaming) ----------------------------------------
    // `written` records the blobs THIS pass created. The abort cleanup must
    // discard only these: a cold file left by an earlier pass is real
    // progress, and deleting it on a later failure throws that work away (and
    // discards a file the hot side is about to be compared against).
    let mut copied: Vec<&PatientBlob> = Vec::with_capacity(batch.blobs.len());
    let mut written: Vec<PatientBlob> = Vec::new();
    let mut already_cold = 0u64;
    for blob in &batch.blobs {
        match copy_hot_to_cold_streaming(state, &blob.blake3, blob.file_size, cold) {
            Ok(_) => {
                copied.push(blob);
                written.push(PatientBlob {
                    blake3: blob.blake3.clone(),
                    file_size: blob.file_size,
                    study_uid: blob.study_uid.clone(),
                });
            }
            Err(e) if e == "__hot_missing__" => {
                if hot_missing_is_resume(state, &blob.blake3, cold) {
                    // Resumed batch: the previous pass already copied this
                    // blob and flipped it; only the delete is outstanding.
                    already_cold += 1;
                } else {
                    // DAMAGED: declared HOT, absent from both tiers. Do not
                    // complete this patient - see hot_missing_is_resume.
                    cleanup_cold_copies(&written, cold);
                    return PatientOutcome::Aborted(format!(
                        "hot copy of {} is missing and no cold copy exists",
                        blob.blake3
                    ));
                }
            }
            Err(e) => {
                // ABORT: discard this pass's cold partials and leave the hot
                // side and the tiers exactly as they were.
                cleanup_cold_copies(&written, cold);
                return PatientOutcome::Aborted(format!("copy failed: {e}"));
            }
        }
    }

    // -- Phase 2: VERIFY ---------------------------------------------------
    for blob in &copied {
        if let Err(e) = verify_cold_copy(&blob.blake3, blob.file_size, cold) {
            cleanup_cold_copies(&written, cold);
            return PatientOutcome::Aborted(format!("verify failed: {e}"));
        }
    }

    // -- Phase 3: FLIP (whole patient, one pass) ---------------------------
    // No DELETE happens anywhere above this line.
    let flipped = set_storage_tier_for_patient(state, batch, "cold");

    // -- Phase 4: DELETE hot (only now) ------------------------------------
    // Every blob is deleted, including the ones copied in an earlier pass: the
    // whole patient is COLD at this point, which is the precondition for
    // removing ANY of its hot copies.
    for blob in &batch.blobs {
        cleanup_hot_file_cas_safe(state, &blob.blake3);
    }

    if copied.is_empty() && already_cold > 0 {
        PatientOutcome::Skipped(already_cold)
    } else {
        PatientOutcome::Migrated(flipped.max(copied.len() as u64))
    }
}

/// Rewrite `storage_tier` in the manifest for every instance of the patient's
/// blobs, in ONE lease.
///
/// This is the batch form of [`set_storage_tier`]: taking the lease once for
/// the whole patient removes the window in which another writer could observe
/// a patient whose instances disagree about their tier. Matching is by BLAKE3,
/// the CAS key, which is what the copy used.
fn set_storage_tier_for_patient(state: &AppState, batch: &PatientBatch, tier: &str) -> u64 {
    let tier_byte = match tier {
        "cold" => clarus_core::manifest::TIER_COLD,
        "s3" => clarus_core::manifest::TIER_S3,
        _ => clarus_core::manifest::TIER_COLD,
    };
    let wanted: std::collections::HashSet<&str> =
        batch.blobs.iter().map(|b| b.blake3.as_str()).collect();
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let Ok(rd) = std::fs::read_dir(manifest_dir) else {
        return 0;
    };

    let mut matched = 0u64;
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Ok(m) = read_manifest(&ent.path()) else {
            continue;
        };
        // Cheap pre-check on the copy we already hold: skip manifests that
        // reference none of the patient's blobs without taking the lease.
        let touches = m.series.iter().flat_map(|s| &s.instances).any(|inst| {
            inst.storage_tier != tier_byte
                && wanted.contains(clarus_adapters::hex_encode(&inst.blake3).as_str())
        });
        if !touches {
            continue;
        }

        // One lease for the whole patient pass, and the tier change applied to
        // the manifest re-read UNDER that lease (the same TOCTOU discipline
        // set_storage_tier uses per blob).
        let _lease = acquire();
        let Ok(mut fresh) = read_manifest(&ent.path()) else {
            continue;
        };
        for inst in fresh.series.iter_mut().flat_map(|s| s.instances.iter_mut()) {
            let b3 = clarus_adapters::hex_encode(&inst.blake3);
            if inst.storage_tier != tier_byte && wanted.contains(b3.as_str()) {
                inst.storage_tier = tier_byte;
                matched += 1;
            }
        }
        let study_uid = name.trim_end_matches(".manifest");
        let (cache_v, _taken) = clarus_core::manifest_cache::begin_write(study_uid);
        if let Err(e) = write_manifest(&ent.path(), &fresh) {
            crate::clog_warn!(
                study = %study_uid,
                tier,
                error = %e,
                "Migration: patient tier rewrite failed"
            );
        } else {
            clarus_core::manifest_cache::end_write(
                study_uid,
                cache_v,
                std::sync::Arc::new(fresh),
                std::fs::metadata(ent.path())
                    .map(|md| md.len())
                    .unwrap_or(0),
                manifest_dir,
            );
        }
    }
    matched
}

// -- Migration batch: S3 (cloud) ------------------------------------------

/// Migrate old studies to S3 cloud storage.
///
/// If cold_path is configured, reads files from cold_path (not hot) -
/// the cold->S3 path is more efficient than hot->S3.
#[cfg(feature = "s3")]
fn migrate_batch_s3(state: &AppState, min_age_days: u32) -> Result<(), String> {
    use clarus_adapters::storage::s3::ObjectStore;

    let s3_store = state.s3.get().ok_or("No S3 storage configured")?;

    let tick_start = std::time::Instant::now();
    let mut tick_migrated = 0u64;
    let mut tick_skipped = 0u64;
    let mut tick_errors = 0u64;

    let cutoff_date = cutoff_date_str(min_age_days);
    let has_cold = state.cold_cas.is_some();

    loop {
        let candidates = collect_migration_candidates(state, &cutoff_date, 500)?;
        if candidates.is_empty() {
            break;
        }

        let chunk_start = std::time::Instant::now();
        let mut chunk_migrated = 0u64;
        let mut chunk_skipped = 0u64;

        for (blake3, file_size, _study_uid) in &candidates {
            // Step A - Idempotency: already in S3?
            let s3_key = s3_store.s3_key(blake3);
            if let Ok(_meta) = s3_store.head_object(&s3_key) {
                crate::clog_debug!(blake3, "Already in S3 - skipping upload");
                set_storage_tier(state, blake3, "s3");
                cleanup_hot_file_cas_safe(state, blake3);
                chunk_migrated += 1;
                continue;
            }

            // Step B - Read (warn+skip, no crash). Cold uses the same two-level
            // BLAKE3 layout as hot/CAS, resolved via the canonical
            // `cas_rel_path` (mini-task layout unity; single source of
            // truth in clarus-adapters).
            let bytes = if has_cold {
                let cold_path = state.cold_cas.as_ref().unwrap().cas_path(blake3);
                match std::fs::read(&cold_path) {
                    Ok(data) if *file_size == 0 || data.len() == *file_size as usize => data,
                    _ => match read_hot_file(state, blake3, *file_size) {
                        Ok(b) => b,
                        Err(e) => {
                            crate::clog_warn!(
                                blake3 = %blake3,
                                error = %e,
                                "S3 migration: failed to read file — skipping"
                            );
                            chunk_skipped += 1;
                            continue;
                        }
                    },
                }
            } else {
                match read_hot_file(state, blake3, *file_size) {
                    Ok(b) => b,
                    Err(e) => {
                        crate::clog_warn!(
                            blake3 = %blake3,
                            error = %e,
                            "S3 migration: failed to read file — skipping"
                        );
                        chunk_skipped += 1;
                        continue;
                    }
                }
            };

            // Step C - Upload. Move `bytes` in (0 new allocations on instance;
            // `bytes` is a Vec<u8> here, not used after this site).
            match s3_store.put_object(&s3_key, bytes, "application/dicom") {
                Ok(_etag) => {
                    crate::clog_debug!(blake3, s3_key, "Uploaded to S3");
                    set_storage_tier(state, blake3, "s3");
                    crate::metrics::record_s3_migration();
                    // Step D - CAS-safe cleanup
                    cleanup_hot_file_cas_safe(state, blake3);
                    chunk_migrated += 1;
                }
                Err(e) => {
                    crate::clog_warn!(blake3, error = %e, "S3 upload failed - skipping");
                    crate::metrics::record_s3_migration_error();
                    chunk_skipped += 1;
                    tick_errors += 1;
                }
            }
        }

        tick_migrated += chunk_migrated;
        tick_skipped += chunk_skipped;
        crate::clog_debug!(
            chunk_migrated,
            chunk_skipped,
            elapsed = chunk_start.elapsed().as_secs(),
            total = tick_migrated,
            "S3 migration chunk"
        );
        // A chunk that moved nothing is a persistent failure (S3 unreachable,
        // credentials refused), and `collect_migration_candidates` derives the
        // same set from the same cutoff - so the next round would retry the
        // identical instances forever. One tick of retries is enough; the next
        // cron tick tries again.
        if chunk_migrated == 0 {
            crate::clog_warn!(
                attempted = candidates.len(),
                skipped = chunk_skipped,
                "S3 migration: chunk made no progress — ending tick, next cron tick retries"
            );
            break;
        }
    }

    // Cron tick complete - one line INFO
    let elapsed = tick_start.elapsed().as_secs();
    crate::clog_info!(
        migrated = tick_migrated,
        skipped = tick_skipped,
        errors = tick_errors,
        elapsed,
        "S3 migration: {} instances, {} skipped, {} errors — {}s",
        tick_migrated,
        tick_skipped,
        tick_errors,
        elapsed,
    );
    if tick_errors > 0 {
        crate::clog_warn!(
            errors = tick_errors,
            "S3 migration: {} FAILED — check S3 connectivity",
            tick_errors,
        );
    }
    Ok(())
}

// -- Emergency migration batch --------------------------------------------

/// Emergency migration: free disk space by moving the oldest studies.
///
/// Ignores `min_age_days` - any study can be migrated.
/// Priority: cold_path -> S3 (fallback).
///
/// `free_gb_threshold` is the level that TRIGGERED this pass, and it is
/// re-checked after every chunk: the point of the pass is to reach that
/// level, so continuing past it keeps evicting a store that is no longer in
/// danger - and on a large store the pass would run long past the moment the
/// emergency ended.
fn migrate_batch_emergency(state: &AppState, free_gb_threshold: u32) -> Result<(), String> {
    let has_cold = state.cold_cas.is_some();
    #[cfg(feature = "s3")]
    let has_s3 = state.s3.get().is_some();
    #[cfg(not(feature = "s3"))]
    let has_s3 = false;

    if !has_cold && !has_s3 {
        return Err("No migration target configured for emergency".to_string());
    }

    let tick_start = std::time::Instant::now();
    let mut tick_migrated = 0u64;
    let mut tick_skipped = 0u64;

    let cutoff_date = "99991231".to_string();

    // Blobs that failed and must not be retried. Declared OUTSIDE the round
    // loop on purpose: `collect_migration_candidates` re-reads the same HOT
    // set every round, so a set rebuilt per round forgot every earlier
    // failure and the next round picked the same unmigratable blobs again.
    // The loop only terminated because a round in which nothing migrated
    // breaks out; with a mixed set it burned a full retry cycle per round on
    // blobs already known to fail (full NAS, corrupt instance).
    let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        let candidates = collect_migration_candidates(state, &cutoff_date, 500)?;

        let chunk_start = std::time::Instant::now();
        let mut chunk_migrated = 0u64;
        let mut chunk_skipped = 0u64;

        crate::clog_warn!(
            count = candidates.len(),
            has_cold,
            has_s3,
            "Emergency migration: starting batch"
        );

        for (blake3, file_size, _study_uid) in &candidates {
            if failed.contains(blake3) {
                continue;
            }
            let mut success = false;

            if has_cold {
                match migrate_one_to_filesystem(
                    state,
                    blake3,
                    *file_size,
                    state.cold_cas.as_ref().unwrap(),
                ) {
                    Ok(true) => {
                        cleanup_hot_file_cas_safe(state, blake3);
                        success = true;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        crate::clog_warn!(blake3, error = %e, "Emergency: cold failed, trying S3")
                    }
                }
            }

            #[cfg(feature = "s3")]
            if !success && has_s3 {
                use clarus_adapters::storage::s3::ObjectStore;
                let bytes = match read_hot_file(state, blake3, *file_size) {
                    Ok(b) => b,
                    Err(e) => {
                        crate::clog_warn!(blake3, error = %e, "Emergency: cannot read hot");
                        chunk_skipped += 1;
                        continue;
                    }
                };
                let s3_store = state.s3.get().unwrap();
                let s3_key = s3_store.s3_key(blake3);
                match s3_store.put_object(&s3_key, bytes, "application/dicom") {
                    Ok(_etag) => {
                        crate::clog_debug!(blake3, "Emergency: uploaded to S3");
                        set_storage_tier(state, blake3, "s3");
                        cleanup_hot_file_cas_safe(state, blake3);
                        success = true;
                    }
                    Err(e) => crate::clog_warn!(blake3, error = %e, "Emergency: S3 upload failed"),
                }
            }

            if success {
                chunk_migrated += 1;
            } else {
                chunk_skipped += 1;
                failed.insert(blake3.clone());
            }
        }

        tick_migrated += chunk_migrated;
        tick_skipped += chunk_skipped;
        crate::clog_debug!(
            chunk_migrated,
            chunk_skipped,
            elapsed = chunk_start.elapsed().as_secs(),
            total = tick_migrated,
            "Emergency migration chunk"
        );

        // Nothing moved and nothing new was tried: the store has no blob left
        // that this pass can evict, so another round would repeat it verbatim.
        if chunk_migrated == 0 {
            break;
        }
        // The threshold has been met - stop evicting.
        match get_free_space_gb(state.cas.root_dir()) {
            Ok(free_gb) if free_gb >= free_gb_threshold as u64 => {
                crate::clog_info!(
                    free_gb,
                    free_gb_threshold,
                    "Emergency migration: threshold reached, stopping"
                );
                break;
            }
            Ok(_) => {}
            Err(e) => {
                // Cannot verify progress: the honest move is to stop rather
                // than keep deleting hot copies on a guess.
                crate::clog_warn!(error = %e, "Emergency: free-space probe failed, stopping");
                break;
            }
        }
    }

    let elapsed = tick_start.elapsed().as_secs();
    crate::clog_warn!(
        migrated = tick_migrated,
        skipped = tick_skipped,
        elapsed,
        "Emergency migration: {} instances, {} skipped — {}s",
        tick_migrated,
        tick_skipped,
        elapsed,
    );
    Ok(())
}

// -- Helpers --------------------------------------------------------------

/// Migrate a single DICOM file from hot CAS to the cold file-system CAS.
///
/// The target path is `cold.cas_path(blake3)` - the canonical [`cas_rel_path`]
/// (mini-task layout unity; single source of truth in clarus-adapters),
/// so WADO's `cas_path` resolves the written file.
///
/// Returns:
/// - `Ok(true)` - successfully migrated
/// - `Ok(false)` - skipped (file gone, already migrated, etc.)
/// - `Err(e)` - fatal error, batch should abort
fn migrate_one_to_filesystem(
    state: &AppState,
    blake3: &str,
    file_size: i64,
    cold: &ContentAddressedStorage,
) -> Result<bool, String> {
    // ap101: streaming chunked copy, no whole-blob `Vec` (see
    // copy_hot_to_cold_streaming). A missing hot file is `Ok(false)` - the
    // blob needs no work, which is the emergency path's skip semantics.
    match copy_hot_to_cold_streaming(state, blake3, file_size, cold) {
        Ok(_) => {}
        Err(e) if e == "__hot_missing__" => return Ok(false),
        Err(e) => return Err(e),
    }

    // Update the manifest tier (manifest-rewrite, no SQLite).
    set_storage_tier(state, blake3, "cold");

    Ok(true)
}

/// CAS-safe hot file cleanup: delete the hot copy only if no live-manifest
/// instance still references it in the `hot` tier.
fn cleanup_hot_file_cas_safe(state: &AppState, blake3: &str) {
    let hot_path = state.cas.cas_path(blake3);
    if !hot_path.exists() {
        return;
    }
    let n = count_hot_refs(
        std::path::Path::new(&state.config.btree.manifest_dir),
        blake3,
    );
    if n == 0 {
        if let Err(e) = std::fs::remove_file(&hot_path) {
            crate::clog_warn!(blake3, error = %e, "Migration: cannot delete hot file");
        } else {
            crate::clog_debug!(blake3, "Hot file deleted - no hot refs remain");
        }
    } else {
        crate::clog_debug!(blake3, n, "Hot file kept - {n} refs still in hot");
    }
}

/// Count live-manifest instances referencing `blake3` while still in the `hot`
/// tier. `0` means the hot copy is safe to delete. Matches BLAKE3 (CAS key).
fn count_hot_refs(manifest_dir: &std::path::Path, blake3: &str) -> u64 {
    let Ok(rd) = std::fs::read_dir(manifest_dir) else {
        return u64::MAX; // cannot verify - keep the file
    };
    let mut n = 0u64;
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Ok(m) = read_manifest(&ent.path()) else {
            continue;
        };
        for inst in m.series.iter().flat_map(|s| &s.instances) {
            if inst.storage_tier != clarus_core::manifest::TIER_HOT {
                continue;
            }
            if clarus_adapters::hex_encode(&inst.blake3) == blake3 {
                n += 1;
            }
        }
    }
    n
}

/// Copy one hot CAS blob to the cold CAS, streaming in fixed-size chunks.
///
/// ap101: the previous implementation was `std::fs::read` into a `Vec` - the
/// whole instance in memory, which for a large CT/MR series is tens of MB per
/// blob multiplied by the batch. This reads and writes a fixed 64 KiB stack
/// buffer instead, so the peak allocation is the buffer and nothing scales
/// with instance size. The byte count is returned as a plain `u64`.
///
/// # Size verification
/// The copy is verified against `expected_size` by counting the bytes that
/// actually crossed, not by `metadata()` on the target: the size the manifest
/// recorded is what a reader will rely on, so the check is on the copy itself.
/// A short read (the hot file shrank or is truncated) fails the blob.
///
/// Returns `Ok(bytes_copied)` or an error; `expected_size <= 0` means the
/// manifest did not record a size and the count is not enforced.
fn copy_hot_to_cold_streaming(
    state: &AppState,
    blake3: &str,
    file_size: i64,
    cold: &ContentAddressedStorage,
) -> Result<u64, String> {
    use std::io::{Read, Write};

    let hot_path = state.cas.cas_path(blake3);
    let target_path = cold.cas_path(blake3);

    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all failed: {e}"))?;
    }

    let mut src = match std::fs::File::open(&hot_path) {
        Ok(f) => f,
        // The hot file is gone (GC'd, already migrated): not an error for this
        // blob - the caller counts it as skipped, exactly as before.
        Err(_) => return Err("__hot_missing__".to_string()),
    };
    // Write into a sibling temp file and rename once the bytes are durable.
    // Writing the target path directly means an interrupted pass leaves a
    // truncated file at the CAS address: the next run's `verify_cold_copy`
    // would see a size mismatch (recoverable), but a reader that arrives
    // first serves short bytes as if they were the instance. `rename` is
    // atomic within the directory, so the address either holds the whole blob
    // or nothing at all.
    //
    // The temp name is unique per ATTEMPT, not per blob: the planned-cold and
    // emergency workers are independent threads and can pick the same blob in
    // the same window. One shared `{blob}.migrating` let the two copies
    // interleave their chunks in a single file, and whichever renamed last
    // made the corrupted result canonical. With a distinct temp per attempt
    // the winner is always one writer's complete file; the loser's temp is
    // discarded on its own error path, and the double tier flip is
    // idempotent. (A process-wide lock would serialise the emergency pass
    // behind cold batches that can run for hours - a much worse trade.)
    static COPY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp_path = target_path.with_extension(format!(
        "migrating.{}.{}",
        std::process::id(),
        COPY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut dst = std::fs::File::create(&tmp_path)
        .map_err(|e| format!("create cold file failed for {blake3}: {e}"))?;

    // One stack buffer for every blob; 64 KiB is the page-friendly middle
    // ground between syscall count and cache footprint.
    let mut buf = [0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = match src.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                // Every early return past the `create` must drop the temp it
                // created: a partial `*.migrating.<pid>.<seq>` is a full-size
                // orphan that no tier ever references, so nothing else will
                // ever reclaim it. The size-mismatch path below always
                // cleaned up; the read and write errors did not, which is how
                // an ENOSPC or a dropped NAS handle leaked one temp per
                // failed attempt, pass after pass.
                drop(dst);
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!("read hot file {blake3} failed: {e}"));
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = dst.write_all(&buf[..n]) {
            drop(dst);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!("write cold file {blake3} failed: {e}"));
        }
        written += n as u64;
    }
    drop(dst);

    if file_size > 0 && written != file_size as u64 {
        // A short or long copy is a bad copy: never flip a tier onto it.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "Size mismatch for {blake3}: expected {file_size}, copied {written}"
        ));
    }

    // `sync_all` before the rename: a crash between the two must not leave the
    // renamed file with unwritten blocks. The directory sync afterwards makes
    // the rename itself durable.
    if let Err(e) = std::fs::OpenOptions::new()
        .write(true)
        .open(&tmp_path)
        .and_then(|f| f.sync_all())
    {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("sync cold copy {blake3} failed: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp_path, &target_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("rename cold copy {blake3} failed: {e}"));
    }
    if let Some(parent) = target_path.parent()
        && let Err(e) = sync_dir(parent)
    {
        return Err(format!("sync cold directory {parent:?} failed: {e}"));
    }

    crate::metrics::STORAGE_WRITE_BYTES.fetch_add(written, std::sync::atomic::Ordering::Relaxed);
    Ok(written)
}

/// Make a directory entry durable.
///
/// Best-effort by design: a directory cannot be opened for writing on any
/// supported platform, and on Windows `FlushFileBuffers` on a directory handle
/// is refused outright (ERROR_ACCESS_DENIED). The rename is still atomic, so
/// the worst case is that the entry is not yet durable when the power goes -
/// the CAS address then reads as absent, which is the correct pre-copy state,
/// and the blob is retried on the next pass.
fn sync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // Nothing to do: Windows has no directory-fsync primitive.
        let _ = dir;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(dir)?.sync_all()
    }
}

/// Verify a cold copy of `blake3` exists and has the expected size.
///
/// The COPY phase already counts what it wrote; this is the independent
/// re-read the protocol requires before any tier flip, so a copy that was
/// truncated after the fact (full disk, interrupted write) is caught here.
fn verify_cold_copy(
    blake3: &str,
    file_size: i64,
    cold: &ContentAddressedStorage,
) -> Result<(), String> {
    let target_path = cold.cas_path(blake3);
    let meta = std::fs::metadata(&target_path)
        .map_err(|e| format!("cold copy of {blake3} is not readable: {e}"))?;
    if file_size > 0 && meta.len() != file_size as u64 {
        return Err(format!(
            "cold copy of {blake3} is {} bytes, expected {file_size}",
            meta.len()
        ));
    }
    Ok(())
}

/// Remove cold copies of `blobs` (the ABORT path's cleanup).
///
/// Only the COLD side is touched: the abort contract is "hot untouched, tier
/// untouched", so a failed patient leaves the hot originals exactly as they
/// were and only discards the partial cold work.
fn cleanup_cold_copies(blobs: &[PatientBlob], cold: &ContentAddressedStorage) {
    for blob in blobs {
        let path = cold.cas_path(&blob.blake3);
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Read a DICOM file from hot CAS, verifying its size.
///
/// Returns `Err` if the file cannot be read or size mismatches.
///
/// Only the S3/emergency paths still need a whole-blob `Vec` (they hand the
/// bytes to an S3 upload that takes a slice). The cold path uses
/// [`copy_hot_to_cold_streaming`] instead - ap101 forbids materialising a
/// whole instance for a file-to-file copy.
#[cfg(feature = "s3")]
fn read_hot_file(state: &AppState, blake3: &str, expected_size: i64) -> Result<Vec<u8>, String> {
    let hot_path = state.cas.cas_path(blake3);

    let data =
        std::fs::read(&hot_path).map_err(|e| format!("cannot read hot file {blake3}: {e}"))?;

    // Skip size check when file_size is 0 (recovery DB may not have it).
    // The BLAKE3 content hash in the file path is the real integrity check.
    if expected_size > 0 && data.len() != expected_size as usize {
        return Err(format!(
            "Size mismatch for {blake3}: expected {expected_size}, got {}",
            data.len()
        ));
    }

    Ok(data)
}

// -- Date/time helpers ---------------------------------------------------

/// Compute a DICOM-format date string (YYYYMMDD) for N days ago.
fn cutoff_date_str(days_ago: u32) -> String {
    let secs_per_day: u64 = 86400;
    let cutoff_secs = (days_ago as u64).saturating_mul(secs_per_day);

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let target_secs = now_secs.saturating_sub(cutoff_secs);
    let total_days = target_secs / secs_per_day;
    // canonical Hinnant in clarus-core::time (single source).
    let (y, m, d) = clarus_core::time::civil_from_days(total_days as i64);
    format!("{:04}{:02}{:02}", y, m, d)
}

// -- Database query ------------------------------------------------------

/// Collect migration candidates from live manifests: instances
/// currently in the `hot` tier whose study date precedes `cutoff_date`,
/// ordered by study date ascending, limited to `limit`.
///
/// Returns `(blake3_hex, file_size, study_uid)` - the CAS key is BLAKE3.
fn collect_migration_candidates(
    state: &AppState,
    cutoff_date: &str,
    limit: u32,
) -> Result<Vec<(String, i64, String)>, String> {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let cutoff: i64 = cutoff_date.parse().unwrap_or(i64::MAX);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let Ok(rd) = std::fs::read_dir(manifest_dir) else {
        return Ok(out);
    };
    let mut entries: Vec<(i64, (String, i64, String))> = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Ok(m) = read_manifest(&ent.path()) else {
            continue;
        };
        // Study date from the study-level attrs (stored as UInt YYYYMMDD).
        let mut study_date = i64::MAX;
        for (t, v) in &m.study.attrs {
            if *t == tag::STUDY_DATE
                && let AttrValue::UInt(d) = v
            {
                study_date = *d as i64;
            }
        }
        if study_date >= cutoff {
            continue;
        }
        let study_uid = name.trim_end_matches(".manifest").to_string();
        for inst in m.series.iter().flat_map(|s| &s.instances) {
            if inst.storage_tier != clarus_core::manifest::TIER_HOT {
                continue;
            }
            let b3 = clarus_adapters::hex_encode(&inst.blake3);
            if seen.insert(b3.clone()) {
                entries.push((study_date, (b3, inst.file_size as i64, study_uid.clone())));
            }
        }
    }

    entries.sort_by_key(|(d, _)| *d);
    for (_, cand) in entries.into_iter().take(limit as usize) {
        out.push(cand);
    }
    Ok(out)
}

// -- Patient-atomic collection (OWNER INVARIANT 12.09) -------------------

/// One blob of a patient batch: the CAS key, its expected size, and the study
/// it belongs to (the study is kept so the tier flip can report what it
/// touched; the copy itself needs only the CAS key).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PatientBlob {
    blake3: String,
    file_size: i64,
    study_uid: String,
}

/// A `(blake3, file_size, study_uid)` triple as the test seams pass it.
type BlobTriple = (String, i64, String);

/// One patient batch: the (0010,0020) that identifies it, every HOT blob of
/// every study that patient owns, and the earliest study date among them.
///
/// A batch is ONE PATIENT, never a slice of one: the migration protocol flips
/// and deletes per batch, so a batch boundary IS the atomicity boundary
/// (OWNER INVARIANT: a partially migrated patient is forbidden).
#[derive(Debug, Clone)]
struct PatientBatch {
    patient_id: String,
    blobs: Vec<PatientBlob>,
}

/// Group the HOT migration candidates BY PATIENT.
///
/// Reads every live manifest, keeps the studies older than `cutoff_date`, and
/// groups their HOT instances under the study-level Patient ID (0010,0020).
/// A study with no Patient ID is skipped: it cannot be attributed to a
/// patient, and guessing an attribution is exactly the kind of silent
/// half-migration the invariant forbids.
///
/// Patients are ordered by their EARLIEST study date, so the oldest patient on
/// the tier migrates first (the same fairness order the per-instance collector
/// used). The `max_patients` cap is now a PATIENT cap, not a blob cap: a batch
/// must not be truncated, or the invariant would break at the cap.
fn collect_migration_patients(
    state: &AppState,
    cutoff_date: &str,
    max_patients: usize,
) -> Result<Vec<PatientBatch>, String> {
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let cutoff: i64 = cutoff_date.parse().unwrap_or(i64::MAX);
    // patient_id -> (earliest study date for ordering, blobs)
    let mut by_patient: std::collections::HashMap<String, (i64, Vec<PatientBlob>)> =
        std::collections::HashMap::new();
    // One blob can be referenced by two studies of the SAME patient; it must
    // appear once in the batch or the copy/verify/flip pass would do it twice.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let Ok(rd) = std::fs::read_dir(manifest_dir) else {
        return Ok(Vec::new());
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Ok(m) = read_manifest(&ent.path()) else {
            continue;
        };
        let mut study_date = i64::MAX;
        let mut patient_id: Option<String> = None;
        for (t, v) in &m.study.attrs {
            if *t == tag::STUDY_DATE
                && let AttrValue::UInt(d) = v
            {
                study_date = *d as i64;
            }
            if *t == tag::PATIENT_ID
                && let AttrValue::Text(s) = v
                && !s.is_empty()
            {
                patient_id = Some(s.clone());
            }
        }
        if study_date >= cutoff {
            continue;
        }
        let Some(patient_id) = patient_id else {
            crate::clog_debug!(
                study = %name.as_ref(),
                "Cold migration: study older than cutoff has no Patient ID - skipped (cannot be attributed to a patient)"
            );
            continue;
        };
        let study_uid = name.trim_end_matches(".manifest").to_string();
        let slot = by_patient
            .entry(patient_id)
            .or_insert((i64::MAX, Vec::new()));
        if study_date < slot.0 {
            slot.0 = study_date;
        }
        for inst in m.series.iter().flat_map(|s| &s.instances) {
            if inst.storage_tier != clarus_core::manifest::TIER_HOT {
                continue;
            }
            let b3 = clarus_adapters::hex_encode(&inst.blake3);
            if seen.insert(b3.clone()) {
                slot.1.push(PatientBlob {
                    blake3: b3,
                    file_size: inst.file_size as i64,
                    study_uid: study_uid.clone(),
                });
            }
        }
    }

    let mut batches: Vec<(i64, PatientBatch)> = by_patient
        .into_iter()
        .filter(|(_, (_, blobs))| !blobs.is_empty())
        .map(|(patient_id, (date, mut blobs))| {
            // Deterministic order inside a batch: the same manifests must
            // always produce the same sequence, or a resume would reorder.
            blobs.sort_by(|a, b| a.blake3.cmp(&b.blake3));
            (date, PatientBatch { patient_id, blobs })
        })
        .collect();
    batches.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.patient_id.cmp(&b.1.patient_id))
    });
    Ok(batches
        .into_iter()
        .take(max_patients)
        .map(|(_, batch)| batch)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarus_core::stow::InstanceModel;
    use clarus_core::stow::SeriesModel;
    use clarus_core::stow::write_study_manifest_full;

    fn write_one_manifest(
        dir: &std::path::Path,
        study: &str,
        blake3: [u8; 32],
        tier: u8,
        study_date: Option<&str>,
    ) {
        write_one_manifest_for(dir, study, blake3, tier, study_date, "MIG");
    }

    /// Like [`write_one_manifest`] but with an explicit Patient ID, which is
    /// what the patient grouping keys on.
    fn write_one_manifest_for(
        dir: &std::path::Path,
        study: &str,
        blake3: [u8; 32],
        tier: u8,
        study_date: Option<&str>,
        patient_id: &str,
    ) {
        let mut attrs = clarus_core::stow::StudyManifestAttrs {
            patient_id: Some(patient_id.into()),
            study_instance_uid: study.to_string(),
            modality: "CT".into(),
            study_date: study_date.map(|s| s.to_string()),
            ..Default::default()
        };
        let instance = InstanceModel {
            sop_instance_uid: format!("{study}.1"),
            blake3,
            storage_tier: tier,
            transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
            file_size: 64,
            ..Default::default()
        };
        let series = [SeriesModel {
            series_instance_uid: format!("{study}.s1"),
            modality: "CT".into(),
            instances: vec![instance],
            ..Default::default()
        }];
        write_study_manifest_full(dir, &mut attrs, &series).expect("write manifest");
    }

    #[test]
    fn count_hot_refs_matches_tier_and_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut b = [0u8; 32];
        b[0] = 0xAB;
        // Hot instance referencing 0xAB -> 1 hot ref.
        write_one_manifest(
            dir.path(),
            "1.2.3",
            b,
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
        );
        // Cold instance with the SAME blob -> must not count as a hot ref.
        write_one_manifest(
            dir.path(),
            "1.2.4",
            b,
            clarus_core::manifest::TIER_COLD,
            Some("20260101"),
        );
        // Another study with a different blob -> no ref to 0xAB.
        write_one_manifest(
            dir.path(),
            "1.2.5",
            [0xCD; 32],
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
        );

        assert_eq!(
            count_hot_refs(dir.path(), &clarus_adapters::hex_encode(&b)),
            1,
            "only the hot instance counts"
        );
        assert_eq!(
            count_hot_refs(dir.path(), &clarus_adapters::hex_encode(&[0xCD; 32])),
            1
        );
    }

    #[test]
    fn count_hot_refs_zero_when_no_hot_instance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut b = [0u8; 32];
        b[0] = 0xEF;
        // Only a cold instance references the blob -> safe to drop the hot copy.
        write_one_manifest(
            dir.path(),
            "1.2.6",
            b,
            clarus_core::manifest::TIER_COLD,
            Some("20260101"),
        );
        assert_eq!(
            count_hot_refs(dir.path(), &clarus_adapters::hex_encode(&b)),
            0
        );
    }
}
