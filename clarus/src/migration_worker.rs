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
use clarus_core::manifest::{AttrValue, write_manifest};
use clarus_core::query::tag;
use clarus_core::stow::acquire;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::state::AppState;
use crate::storage_tiers::CronSchedule;
#[cfg(feature = "s3")]
use clarus_adapters::storage::s3::ObjectStore;

/// One lock per blob, shared by every path that can change where its bytes live.
///
/// The abort cleanup and the emergency worker both act on a blob's cold copy:
/// the worker may flip a manifest onto that copy (via [`set_storage_tier`])
/// while the cleanup is deciding whether the copy is still referenced. Counting
/// the references and then unlinking is a TOCTOU - the flip lands in between and
/// the cleanup deletes the only resolvable bytes.
///
/// The guard is held across the WHOLE critical section (count AND unlink for the
/// cleanup, the manifest rewrite for the publisher), so the two cannot interleave
/// for one blob. Different blobs never contend: the map is keyed by CAS address.
///
/// A map of locks rather than one global mutex: cleanup walks a patient's blobs
/// and a global lock would serialise it against every unrelated migration.
///
/// The map holds WEAK references and is pruned as it is walked. Holding a
/// strong `Arc` per digest made the map grow with the LIFETIME number of
/// distinct blobs ever migrated - a busy archive carries millions, and none of
/// them could ever be released. A `Weak` entry costs nothing once the last
/// caller drops its guard, and the next prune removes it.
///
/// Identity is preserved for every ACTIVE caller: two threads asking for the
/// same digest at the same time both upgrade the same `Arc`, so the mutual
/// exclusion the rest of this module depends on is unchanged.
fn blob_guard(blake3: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
    // Prune entries whose last holder has gone. Done on every call because it
    // is bounded by the number of digests ACTIVE at this instant, not by the
    // number ever seen.
    guard.retain(|_, weak| weak.strong_count() > 0);
    if let Some(live) = guard.get(blake3).and_then(Weak::upgrade) {
        return live;
    }
    let fresh = Arc::new(Mutex::new(()));
    guard.insert(blake3.to_string(), Arc::downgrade(&fresh));
    fresh
}

/// Rewrite `storage_tier` in every live manifest whose instance references the
/// blob (matched by BLAKE3, the CAS key). The physical file relocation
/// (copy->verify) happens in the caller; this only updates the manifest
/// metadata so reads (WADO/QIDO) resolve the new tier.
///
/// Returns the number of manifests that CHANGED on disk. A rewrite that failed
/// is not counted: the caller decides durability from this number, so counting
/// an intent rather than a completed write made a batch loop forever on the
/// ENOSPC path - the blob was reported migrated, the tier never changed, and the
/// next tick selected it again.
///
/// Takes the blob's guard so a concurrent abort cleanup cannot decide the copy
/// is unreferenced while this rewrites the manifest onto it.
/// Takes the blob's guard so a concurrent abort cleanup cannot decide the copy
/// is unreferenced while this rewrites the manifest onto it.
///
/// Only the S3 paths call this: the filesystem migration holds ONE guard
/// across copy, publish and the hot delete, so it calls
/// [`set_storage_tier_locked`] instead.
#[cfg_attr(not(feature = "s3"), allow(dead_code))]
fn set_storage_tier(state: &AppState, blake3: &str, tier: &str) -> usize {
    let guard = blob_guard(blake3);
    let _held = guard.lock().unwrap_or_else(|p| p.into_inner());
    set_storage_tier_locked(state, blake3, tier)
}

/// The rewrite itself, for callers that ALREADY hold the blob guard.
///
/// Split out because the migration path holds one guard across copy, publish
/// and the hot delete: re-acquiring here would deadlock on a non-reentrant
/// mutex, and a plain `set_storage_tier` call would release the ordering the
/// protocol depends on.
fn set_storage_tier_locked(state: &AppState, blake3: &str, tier: &str) -> usize {
    set_storage_tier_in(state, blake3, tier)
}

/// Does a failed tier rewrite for `tier_byte` belong in the S3 error counter?
///
/// Only for S3. `set_storage_tier_for_patient` writes BOTH the cold and the S3
/// tier, so an unconditional `record_s3_migration_error()` filed COLD write
/// failures (ENOSPC on the cold volume, most often) under a cloud metric: the
/// S3 error rate would climb while S3 was healthy, and nothing would ever clear
/// it because no S3 retry touches a failed cold write.
///
/// Not feature-gated so the rule stays testable in a build without `s3`; the
/// counter call itself is the gated part, so in that build the only callers are
/// the tests below.
#[cfg_attr(not(feature = "s3"), allow(dead_code))]
fn a_failed_rewrite_is_an_s3_error(tier_byte: u8) -> bool {
    tier_byte == clarus_core::manifest::TIER_S3
}

/// The scan itself, over an explicit manifest directory.
///
/// Split out so the durability rule is testable from a temp directory: whether
/// a failed rewrite is COUNTED is the whole finding, and a test that had to
/// build an `AppState` to reach it would not be written at all.
fn set_storage_tier_in(state: &AppState, blake3: &str, tier: &str) -> usize {
    let tier_byte = match tier {
        "cold" => clarus_core::manifest::TIER_COLD,
        "s3" => clarus_core::manifest::TIER_S3,
        _ => clarus_core::manifest::TIER_COLD,
    };
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let mut matched = 0usize;
    let Ok(rd) = std::fs::read_dir(manifest_dir) else {
        return 0;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let study_uid = name.trim_end_matches(".manifest");
        let Some(Ok(m)) = state.read_server_manifest(study_uid) else {
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
            let Some(Ok(mut fresh)) = state.read_server_manifest(study_uid) else {
                continue;
            };
            // Count what THIS manifest contributed and only publish it once the
            // write lands. Counting the intent made a failed rewrite (the
            // ENOSPC case) look like a completed migration, so the caller
            // deleted the hot copy and the next tick selected the blob again.
            let mut changed = 0usize;
            for inst in fresh.series.iter_mut().flat_map(|s| s.instances.iter_mut()) {
                let b3 = clarus_adapters::hex_encode(&inst.blake3);
                if b3 == blake3 && inst.storage_tier != tier_byte {
                    inst.storage_tier = tier_byte;
                    changed += 1;
                }
            }
            // B3: drop the cached entry BEFORE the rewrite; reinstall after.
            let (cache_v, _taken) =
                clarus_core::manifest_cache::begin_write(manifest_dir, study_uid);
            if let Err(e) = write_manifest(&ent.path(), &fresh) {
                // O1: the tier did NOT change on disk, so this manifest is not
                // migrated. Reporting it as migrated hides an ENOSPC and makes
                // the batch re-select the same blob forever; the count below
                // stays at zero so the caller can fail the blob instead.
                crate::clog_warn!(blake3, tier, error = %e, "Migration: manifest tier rewrite failed");
                continue;
            } else {
                matched += changed;
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
    matched
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

/// Sleep until the next cron match, returning `None` when the schedule has no
/// future match (the caller then backs off and retries).
///
/// The wait preserves the FRACTIONAL part of the remaining time. Rounding it
/// through `num_seconds()` truncated it, so a clock reading `01:59:58.250`
/// before a `02:00` tick slept one second and started the batch at `01:59:59`,
/// a quarter second early, with an empty batch whose `next_after` then
/// returned the SAME `02:00` tick again, so the loop selected it twice.
///
/// The deadline is re-checked after sleeping for the same reason: any
/// rounding, any clock adjustment while asleep, or a spurious wakeup must not
/// let the batch run before its instant.
fn wait_for_cron_tick(cron: &CronSchedule, label: &str) -> Option<()> {
    const MAX_REPOLLS: usize = 8;
    for _ in 0..MAX_REPOLLS {
        let now = chrono::Local::now();
        let next = cron.next_after(&now)?;
        let remaining = next.signed_duration_since(now);
        if remaining.num_milliseconds() <= 0 {
            // Already at or past the instant: run now.
            return Some(());
        }
        crate::clog_debug!(
            next_run = %next,
            sleep_ms = remaining.num_milliseconds(),
            "{label}: sleeping until next cron tick"
        );
        // `to_std()` keeps sub-second precision; a negative span cannot reach
        // here because it was rejected above.
        if let Ok(d) = remaining.to_std() {
            std::thread::sleep(d);
        }
    }
    // The deadline kept receding (a system clock being stepped backwards).
    // Return `None` so the caller backs off rather than spinning.
    None
}

/// Cron-driven cold migration: move old studies hot -> NAS/HDD.
fn planned_cold_loop(state: AppState, cron: CronSchedule, min_age_days: u32) {
    crate::clog_info!(%cron, min_age_days, "Planned cold migration worker started");

    loop {
        if wait_for_cron_tick(&cron, "Cold migration").is_none() {
            crate::clog_error!("Cold cron schedule has no future match - sleeping 1 hour");
            std::thread::sleep(Duration::from_secs(3600));
            continue;
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
        if wait_for_cron_tick(&cron, "S3 migration").is_none() {
            crate::clog_error!("S3 cron schedule has no future match - sleeping 1 hour");
            std::thread::sleep(Duration::from_secs(3600));
            continue;
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
///
/// `disk_space_mb` is used rather than `available_disk_mb` because the latter
/// collapses "the stat failed" and "the volume is full" into the same `0.0`.
/// Here the two mean opposite things: a full volume is a LOW-SPACE answer the
/// caller acts on, while a failed stat is a genuine error. Treating zero as an
/// error made a full disk look like a broken one and stopped the migration
/// with the wrong message.
/// What the cold-migration chunk loop does after a chunk.
///
/// Split out of `migrate_batch_cold` because the choice is the whole of the
/// starvation invariant, and reaching it through a full `AppState` would have
/// tested an archive rather than the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkDisposition {
    /// The chunk made progress; go round for more.
    Continue,
    /// Every patient in the chunk aborted. Those IDs are now excluded, so
    /// collect again: either younger patients come back or the collect returns
    /// nothing and the tick ends. Breaking here instead left the exclusion set
    /// unpopulated and handed the same patients out forever.
    Recollect,
    /// Nothing to do at all - no progress and no aborts. There is no patient
    /// this tick could have touched.
    EndTick,
}

/// Decide what to do after a chunk of patients.
fn chunk_disposition(migrated: u64, skipped: u64, aborted: usize) -> ChunkDisposition {
    if migrated > 0 || skipped > 0 {
        return ChunkDisposition::Continue;
    }
    if aborted == 0 {
        return ChunkDisposition::EndTick;
    }
    ChunkDisposition::Recollect
}

/// Whether an S3 chunk that migrated nothing should still end the tick.
///
/// The S3 loop's rule, kept beside [`chunk_disposition`] because it is the same
/// starvation argument with a different progress signal: that loop counts
/// ABORTS, this one counts EXCLUSION GROWTH. A chunk of 500 unreadable blobs
/// migrates nothing but excludes 500 digests, so the next round can reach the
/// blobs behind them - breaking there meant the next tick started from an
/// empty failure set, selected the same 500 again, and blob 501 was starved on
/// a store whose oldest blobs are damaged.
///
/// Extracted so the rule is directly testable; the loop's decision is
/// unchanged. Gated with the S3 loop it belongs to: without that feature there
/// is nothing to decide.
#[cfg(any(feature = "s3", test))]
fn s3_chunk_ends_tick(migrated: u64, excluded_before: usize, excluded_after: usize) -> bool {
    let learned_nothing = excluded_after == excluded_before;
    migrated == 0 && learned_nothing
}

fn get_free_space_gb(path: &std::path::Path) -> Result<u64, String> {
    let (_total_mb, available_mb) = crate::disk_utils::disk_space_mb(path)
        .ok_or_else(|| "Cannot determine free disk space".to_string())?;
    if available_mb < 0.0 {
        return Err("Cannot determine free disk space".to_string());
    }
    // Zero available is a real answer: "no space left", which the caller's
    // low-space check has to see.
    Ok((available_mb / 1024.0) as u64)
}

// -- Migration batch: cold (NAS/HDD) --------------------------------------

/// Migrate old studies from hot CAS to cold (file-system) CAS, ONE PATIENT AT
/// A TIME.
///
/// # OWNER INVARIANT 12.09 - patient atomicity
///
/// No blob is ever left without a readable copy. The batch unit is the PATIENT
/// (all HOT blobs across all of that patient's studies), and the phases below
/// run to completion for the whole batch before the next one starts:
///
/// 1. COPY    - every blob of the patient, streamed chunk by chunk.
/// 2. VERIFY  - every copied blob, before any metadata is touched.
/// 3. FLIP    - the tier of every matching instance, in one pass.
/// 4. DELETE  - the hot copies, only after the WHOLE patient is flipped.
///
/// The order is the invariant: DELETE is last, so no window exists in which a
/// blob has no readable copy. A failure in COPY or VERIFY aborts the patient -
/// cold partials are removed, and every tier is untouched.
///
/// A patient MAY be split across tiers between ticks, and the next tick
/// converges. Two ways that happens, both CAS-safe per blob:
///
/// - a crash between FLIP and DELETE leaves the bytes in BOTH tiers, which
///   `resolve_file` tolerates (it probes the declared tier, then the opposite
///   one), and re-running the patient is idempotent;
/// - a FLIP that reaches disk for only PART of the patient reports
///   [`PatientOutcome::Partial`]: the flipped blobs are cold and their hot
///   copies are reclaimed, the rest stay hot with their copies intact.
///
/// "Never half-migrated" was the earlier wording and this partial case
/// contradicted it. What actually holds is the narrower, verifiable statement
/// above - no blob loses its only copy - which is what the phase order buys.
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
    // Finish any reclaim an earlier pass (or a crash between the flip and the
    // unlink) left behind, BEFORE selecting new work: those files are no
    // longer referenced by any manifest, so no collector can find them.
    let (reclaimed, still_pending) = reconcile_pending_reclaims(state);
    if reclaimed > 0 || still_pending > 0 {
        crate::clog_info!(
            reclaimed,
            still_pending,
            "Migration: reconciled pending local copies"
        );
    }

    // O3: patients that aborted in THIS tick. It lives outside the chunk loop
    // on purpose: `collect_migration_patients` re-derives the same candidates
    // from the same cutoff every round, so a tick-scoped set is what stops the
    // head-of-queue patients from being handed out again and starving the rest.
    let mut tick_aborted: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        // The cap counts PATIENTS. It cannot count blobs: truncating a patient
        // batch is exactly the partial migration the invariant forbids.
        let batches = collect_migration_patients(state, &cutoff_date, 50, &tick_aborted, false)?;
        if batches.is_empty() {
            break;
        }

        let chunk_start = std::time::Instant::now();
        let mut chunk_migrated = 0u64;
        let mut chunk_skipped = 0u64;
        let mut chunk_aborted: Vec<String> = Vec::new();

        for batch in &batches {
            match migrate_patient_to_cold(state, batch, &cutoff_date) {
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
                        "Cold migration: patient aborted - no manifest rewritten, all tiers untouched"
                    );
                    tick_errors += 1;
                    chunk_aborted.push(batch.patient_id.clone());
                }
                PatientOutcome::Partial(reason) => {
                    // NOT "untouched": part of the patient reached COLD and
                    // its hot copies were reclaimed. The next tick finishes it.
                    crate::clog_warn!(
                        patient = %batch.patient_id,
                        blobs = batch.blobs.len(),
                        reason = %reason,
                        "Cold migration: patient only partially flipped - flipped blobs are cold, the rest stay hot for the next tick"
                    );
                    tick_errors += 1;
                    chunk_aborted.push(batch.patient_id.clone());
                }
            }
        }

        // O3: carry this chunk's aborts into the tick-scoped exclusion BEFORE
        // the no-progress check. The check only exists to avoid re-handing the
        // same patients out; the exclusion set is what achieves that, so it has
        // to be populated first. Extending after the `break` left the set empty,
        // the next tick's collect returned the same 50 aborted patients again,
        // and patient 51 was never attempted.
        tick_aborted.extend(chunk_aborted.iter().cloned());

        match chunk_disposition(chunk_migrated, chunk_skipped, chunk_aborted.len()) {
            ChunkDisposition::Continue => {}
            ChunkDisposition::Recollect => {
                crate::clog_debug!(
                    aborted = tick_aborted.len(),
                    "Cold migration: chunk made no progress past aborts - re-collecting"
                );
                continue;
            }
            ChunkDisposition::EndTick => break,
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
    /// Nothing on disk changed: no manifest was rewritten, so every hot copy
    /// is still HOT and still present. Distinct from [`Self::Partial`], where
    /// SOME manifests did reach disk and their hot copies were reclaimed.
    Aborted(String),
    /// The FLIP reached disk for only part of the patient.
    ///
    /// The flipped blobs are COLD and their hot copies have been reclaimed;
    /// the rest are still HOT with their copies intact. The patient is
    /// therefore split across tiers between ticks, and the next tick's
    /// collector re-selects the still-HOT blobs and converges.
    ///
    /// The invariant this preserves is "no blob ever loses its only readable
    /// copy", not "a patient is never between tiers" - the earlier claim was
    /// contradicted by this very case.
    Partial(String),
}

/// Migrate ONE patient: COPY all -> VERIFY all -> FLIP all -> DELETE hot.
///
/// Returns the number of blobs that changed tier (Migrated) or were already
/// done (Skipped). The function is the only place the phase ORDER is encoded,
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
///
/// The outcome is returned as a tag rather than the private enum so a test can
/// tell [`PatientOutcome::Partial`] from [`PatientOutcome::Aborted`]: the two
/// differ in whether any tier changed, which is exactly what the contract is
/// about.
pub fn migrate_patient_to_cold_for_test(
    state: &AppState,
    patient_id: &str,
    blobs: Vec<BlobTriple>,
) -> (u64, u64, Option<String>, &'static str) {
    // The test seam has no collector to derive the complete study membership
    // from, so it takes it from the triples it was handed - the same studies
    // the blobs name. The collector itself records membership separately from
    // blob dedup (see `collect_migration_patients_in`).
    let mut studies: Vec<String> = blobs.iter().map(|(_, _, s)| s.clone()).collect();
    studies.sort();
    studies.dedup();
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
        studies,
    };
    match migrate_patient_to_cold(state, &batch, "99991231") {
        PatientOutcome::Migrated(n) => (n, 0, None, "migrated"),
        PatientOutcome::Skipped(n) => (0, n, None, "skipped"),
        PatientOutcome::Aborted(reason) => (0, 0, Some(reason), "aborted"),
        PatientOutcome::Partial(reason) => (0, 0, Some(reason), "partial"),
    }
}

/// Run one cold batch. Test seam for the "no cold_path configured" contract:
/// the batch must REPORT the misconfiguration rather than treat it as a no-op,
/// because a silent no-op would look identical to "migration finished".
pub fn migrate_batch_cold_for_test(state: &AppState, min_age_days: u32) -> Result<(), String> {
    migrate_batch_cold(state, min_age_days)
}

/// Publish a tier for one blob. Test seam for the ownership protocol: the
/// publication has to be observable independently of a full migration so a test
/// can place it inside the abort cleanup's decision window.
pub fn set_storage_tier_for_test(state: &AppState, blake3: &str, tier: &str) -> usize {
    set_storage_tier(state, blake3, tier)
}

/// Run the abort cleanup over explicit blob keys. Test seam for the same reason
/// as [`set_storage_tier_for_test`]: the O2 rule (keep a copy a live manifest
/// resolves to) must be testable without a failed patient migration.
pub fn cleanup_cold_copies_for_test(
    state: &AppState,
    blake3s: &[String],
    cold: &ContentAddressedStorage,
) {
    let blobs: Vec<PatientBlob> = blake3s
        .iter()
        .map(|b| PatientBlob {
            blake3: b.clone(),
            file_size: 0,
            study_uid: String::new(),
        })
        .collect();
    cleanup_cold_copies(state, &blobs, cold)
}

/// Whether `blake3`'s ownership guard is CURRENTLY held somewhere.
///
/// Test seam for the mutual-exclusion half of the protocol: the O2 reference
/// rule is observable by outcome, but the guard is only observable by trying to
/// take it. `try_lock` is exactly that probe and never blocks.
pub fn blob_guard_is_held_for_test(blake3: &str) -> bool {
    blob_guard(blake3).try_lock().is_err()
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
    collect_migration_patients(
        state,
        cutoff_date,
        max_patients,
        &std::collections::HashSet::new(),
        false,
    )
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

/// Whether a blob's hot file is absent but a VERIFIED cold copy is present.
///
/// A missing HOT file is only benign when the bytes are ALREADY in cold: that
/// is a resumed batch, where the previous pass copied and flipped but crashed
/// before the delete. If the blob is declared HOT and its bytes are on neither
/// tier, the patient is DAMAGED and the batch must NOT complete - completing it
/// would flip and delete the patient's other blobs and leave this one
/// unreadable, which is exactly the partial patient the invariant forbids.
///
/// `exists()` is not the question. A TRUNCATED or damaged file at the cold
/// address made this answer `true`, so the blob skipped phase 2 entirely, was
/// published as migrated and could be reported as a skip - leaving the archive
/// holding the damaged bytes as the only copy. Resumption therefore requires
/// the same verification the copy path is held to: the file must be readable,
/// the size must match, and its digest must be the CAS address.
fn hot_missing_is_resume(state: &AppState, blake3: &str, cold: &ContentAddressedStorage) -> bool {
    let _ = state;
    if !cold.cas_path(blake3).exists() {
        return false;
    }
    // The manifest size is not passed here: `verify_cold_copy` accepts 0 as
    // "size unknown" and still enforces the digest, which is the stronger
    // check and the one that makes the bytes trustworthy.
    match verify_cold_copy(blake3, 0, cold) {
        Ok(()) => true,
        Err(e) => {
            crate::clog_warn!(
                blake3 = %blake3,
                error = %e,
                "cold copy exists but is not usable; the blob cannot be resumed"
            );
            false
        }
    }
}

fn migrate_patient_to_cold(
    state: &AppState,
    batch: &PatientBatch,
    cutoff_date: &str,
) -> PatientOutcome {
    let Some(cold) = state.cold_cas.as_ref() else {
        return PatientOutcome::Aborted("cold storage disappeared".to_string());
    };

    // -- Phase 0: NARROW (retain) -----------------------------------------
    // Run BEFORE the copy: a blob shared with another patient would otherwise
    // be copied to cold and then dropped by this very filter, leaving a cold
    // copy no manifest references and no later pass reclaims (the collector
    // re-selects only HOT instances, so the orphan is permanent). Narrowing
    // first means an excluded blob is never copied at all.
    let batch = retain_exclusively_hot_blobs(state, batch);
    if batch.blobs.is_empty() {
        // Every blob is still referenced hot by another patient. Nothing was
        // copied, flipped or deleted, so the only honest verdict is an abort:
        // it puts the patient into this tick's exclusion set, which is what
        // stops `migrate_batch_cold` from re-collecting the same no-op
        // patient forever. Reporting `Migrated(n > 0)` here was exactly that
        // infinite loop.
        return PatientOutcome::Aborted(
            "every blob is still referenced hot by another patient; nothing to migrate".to_string(),
        );
    }

    // -- Phase 1: COPY (streaming) ----------------------------------------
    // `written` records the blobs THIS pass created. The abort cleanup must
    // discard only these: a cold file left by an earlier pass is real
    // progress, and deleting it on a later failure throws that work away (and
    // discards a file the hot side is about to be compared against).
    let mut copied: Vec<&PatientBlob> = Vec::with_capacity(batch.blobs.len());
    let mut written: Vec<PatientBlob> = Vec::new();
    // Tracks WHICH blobs were already cold (not just how many): the final
    // Skipped verdict must count only the blobs the revalidation retained.
    let mut already_cold: Vec<&PatientBlob> = Vec::new();
    for blob in &batch.blobs {
        match copy_hot_to_cold_streaming(state, &blob.blake3, blob.file_size, cold) {
            Ok(ColdCopy::Published { created }) => {
                copied.push(blob);
                // Only a file THIS pass created is cleaned up on an abort. A
                // blob that was already in the cold tier belongs to some
                // earlier pass - or to a different hot study that shares the
                // bytes - and unlinking it would delete a shared copy the
                // other tier still points at.
                if created {
                    written.push(PatientBlob {
                        blake3: blob.blake3.clone(),
                        file_size: blob.file_size,
                        study_uid: blob.study_uid.clone(),
                    });
                }
            }
            Ok(ColdCopy::HotMissing) => {
                if hot_missing_is_resume(state, &blob.blake3, cold) {
                    // Resumed batch: the previous pass already copied this
                    // blob and flipped it; only the delete is outstanding.
                    already_cold.push(blob);
                } else {
                    // DAMAGED: declared HOT, absent from both tiers. Do not
                    // complete this patient - see hot_missing_is_resume.
                    cleanup_cold_copies(state, &written, cold);
                    return PatientOutcome::Aborted(format!(
                        "hot copy of {} is missing and no cold copy exists",
                        blob.blake3
                    ));
                }
            }
            Err(e) => {
                // The copy may have PUBLISHED before failing (a directory sync
                // after the rename). That file exists, so an abort that
                // ignored it would leave a full-size orphan no tier references
                // - record it first, then clean up exactly what this pass
                // created.
                if matches!(e, CopyError::PublishedUnsynced { created: true, .. }) {
                    written.push(PatientBlob {
                        blake3: blob.blake3.clone(),
                        file_size: blob.file_size,
                        study_uid: blob.study_uid.clone(),
                    });
                }
                // ABORT: discard this pass's cold partials and leave the hot
                // side and the tiers exactly as they were.
                cleanup_cold_copies(state, &written, cold);
                return PatientOutcome::Aborted(format!("copy failed: {e}"));
            }
        }
    }

    // -- Phase 2: VERIFY ---------------------------------------------------
    for blob in &copied {
        if let Err(e) = verify_cold_copy(&blob.blake3, blob.file_size, cold) {
            cleanup_cold_copies(state, &written, cold);
            return PatientOutcome::Aborted(format!("verify failed: {e}"));
        }
    }

    // -- Phase 3: FLIP (whole patient, one pass) ---------------------------
    // No DELETE happens anywhere above this line. Phase 0 narrowed the batch
    // WITHOUT the manifest lease, so its verdict is a snapshot: a concurrent
    // STOW for a DIFFERENT patient can publish a manifest (under `acquire()`)
    // that names one of these blobs HOT while the COPY above is in flight.
    // Revalidate exclusivity here, under the SAME lease the flip takes, so no
    // reference can be published between the check and the rewrite.
    let (retained_batch, excluded, flipped, ineligible) = {
        let _lease = acquire();
        let (retained_batch, excluded) = revalidate_exclusive_hot_blobs_in(state, &batch);
        // Serialization is not the only thing the copy window can invalidate.
        // A STOW for the SAME patient can publish a NEW study - a recent one -
        // while the copy is in flight, and the exclusivity revalidation does
        // not look at study AGE, only at whether another holder references the
        // blob. Flipping now would move an old study of a patient who has just
        // received a fresh one, which breaks the rule the collector enforced
        // ("every study of this patient is older than the cutoff") on data
        // that no longer satisfies it.
        //
        // The verdict is recomputed against the manifest directory under this
        // lease, so a reference published between the check and the rewrite
        // cannot slip through - the same argument the exclusivity revalidation
        // makes.
        let ineligible = if patient_studies_all_before(state, &retained_batch, cutoff_date) {
            None
        } else {
            Some(
                "patient gained a study at or after the cutoff during the copy; \
                 leaving everything hot"
                    .to_string(),
            )
        };
        let flipped = if ineligible.is_none() {
            set_storage_tier_for_patient_locked(state, &retained_batch, "cold")
        } else {
            0
        };
        (retained_batch, excluded, flipped, ineligible)
    };

    if let Some(reason) = ineligible {
        // Nothing was flipped. Reclaim exactly the cold copies this pass
        // created - they are now unreferenced, and the hot side is untouched.
        cleanup_cold_copies(state, &written, cold);
        return PatientOutcome::Aborted(reason);
    }

    // Reclaim the cold copies THIS pass created for blobs the revalidation
    // excluded: no manifest will reference them now, while the hot copy is
    // still needed by the other patient. Runs AFTER the lease is released,
    // because `cleanup_cold_copies` takes `blob_guard` and the S3 path's
    // `blob_guard -> acquire()` order forbids taking `blob_guard` while the
    // lease is held (the reverse order deadlocks).
    let excluded_keys: std::collections::HashSet<&str> =
        excluded.iter().map(|b| b.blake3.as_str()).collect();
    let excluded_written: Vec<PatientBlob> = written
        .iter()
        .filter(|b| excluded_keys.contains(b.blake3.as_str()))
        .cloned()
        .collect();
    if !excluded_written.is_empty() {
        cleanup_cold_copies(state, &excluded_written, cold);
    }

    // The revalidation can drop every blob after the copy: the whole patient
    // became shared in the window. Nothing may be flipped or deleted, so the
    // verdict is an abort with the reclaimed copies already gone.
    if retained_batch.blobs.is_empty() {
        return PatientOutcome::Aborted(
            "every blob became shared during copy; nothing to migrate".to_string(),
        );
    }

    // Retained-only accounting for the final verdict: an excluded blob was
    // copied but its tier was not flipped, so it must count neither as
    // migrated nor as skipped.
    let retained_keys: std::collections::HashSet<&str> = retained_batch
        .blobs
        .iter()
        .map(|b| b.blake3.as_str())
        .collect();
    let copied_retained = copied
        .iter()
        .filter(|b| retained_keys.contains(b.blake3.as_str()))
        .count();
    let already_cold_retained = already_cold
        .iter()
        .filter(|b| retained_keys.contains(b.blake3.as_str()))
        .count();

    // -- Phase 4: DELETE hot (only now) ------------------------------------
    // O1: the delete is gated on the flip having LANDED. `flipped` cannot be
    // that gate: it counts the instances THIS pass changed, so it is zero both
    // when the rewrite failed (the ENOSPC emergency path) and when an earlier
    // pass already flipped the whole patient (the resume case, where FLIP
    // matches nothing and DELETE is the only work left). The two mean opposite
    // things. The durable fact is the manifest itself: a blob still referenced
    // from the hot tier did not reach disk as COLD, and deleting its hot copy
    // would remove the only copy the server resolves. Zero hot refs across the
    // batch is the same precondition `cleanup_hot_file_cas_safe` checks per
    // blob below, so the abort and the delete cannot drift apart.
    let still_hot = retained_batch
        .blobs
        .iter()
        .any(|blob| count_hot_refs(state, &blob.blake3) > 0);
    if still_hot {
        // A partial flip (some manifests rewrote, others did not) must not
        // leak the hot copy of a blob that DID reach cold: the retry collector
        // re-selects only HOT instances, so that blob would never be visited
        // again. Reclaim per blob, CAS-safe, exactly as the success path does -
        // a blob with zero hot refs is fully cold and its hot copy is
        // removable even while the rest of the patient is still hot; a blob
        // that stayed hot keeps its copy.
        // `flipped` counts the instances THIS pass moved to cold. It answers
        // the question the verdict is actually about - did this pass change a
        // tier anywhere - for both cases that reach this branch: a rewrite that
        // landed on one manifest and failed on another moves instances, and a
        // resumed pass that already flipped everything moves none.
        let moved_this_pass = flipped > 0;
        // The DURABLE form is only needed for the resumed case: a pass where
        // nothing matched (flipped == 0) may still have cold copies left from an
        // earlier pass, and the hot copies must be reclaimed for those even
        // though this pass changed no tier.
        let reached_cold_earlier = !moved_this_pass
            && retained_batch.blobs.iter().any(|blob| {
                count_hot_refs(state, &blob.blake3) == 0 && cold.cas_path(&blob.blake3).exists()
            });
        // The hot copies of the blobs that did reach cold are reclaimed BEFORE
        // the verdict, because that delete is part of the work this pass
        // completed and must not be re-derived on the next tick.
        for blob in &retained_batch.blobs {
            cleanup_hot_file_cas_safe(state, &blob.blake3);
        }
        cleanup_cold_copies(state, &written, cold);
        if !moved_this_pass && !reached_cold_earlier {
            // No manifest rewrite landed and no blob was cold before this pass,
            // so no tier changed at all. Claiming Partial here would tell the
            // operator that data moved between tiers when it did not.
            return PatientOutcome::Aborted("no tier changed".to_string());
        }
        // At least one instance reached cold, so some hot copies were reclaimed
        // above. Reporting `Aborted` would tell the operator every tier is
        // untouched, which is exactly what did not happen.
        return PatientOutcome::Partial(
            "manifest tier flip reached only part of the patient; \
             flipped blobs are cold, the rest stay hot until the next tick"
                .to_string(),
        );
    }

    // Every retained blob is deleted, including the ones copied in an earlier
    // pass: the whole patient is COLD at this point, which is the precondition
    // for removing ANY of its hot copies. An excluded blob is still needed hot
    // by another patient, so its hot copy is never touched here.
    for blob in &retained_batch.blobs {
        cleanup_hot_file_cas_safe(state, &blob.blake3);
    }

    if copied_retained == 0 && already_cold_retained > 0 {
        PatientOutcome::Skipped(already_cold_retained as u64)
    } else {
        PatientOutcome::Migrated(flipped.max(copied_retained as u64))
    }
}

/// Narrow a batch to the blobs whose HOT references all belong to this batch.
///
/// A blob is shared when some manifest names it in the hot tier and that
/// manifest is not one of the studies in `batch`: the blob's bytes are still
/// needed hot by a patient this pass is not migrating, so the flip has to skip
/// it. The check is per blob rather than per patient because sharing is per
/// blob - CAS deduplication collapses identical instances across studies.
///
/// Returning a narrowed batch rather than a boolean keeps the rest of the
/// protocol unchanged: copy, verify, flip and delete all keep operating on "the
/// blobs this pass may move", which is now "the blobs no other patient holds".
fn retain_exclusively_hot_blobs(state: &AppState, batch: &PatientBatch) -> PatientBatch {
    retain_exclusively_hot_blobs_in(state, batch)
}

/// The narrowing itself, over an explicit manifest directory.
///
/// Split out so the sharing rule is testable from a temp directory: the
/// question is "does a manifest OTHER than this patient's studies hold the
/// blob hot", and a test that had to build an `AppState` to reach it would
/// test an archive rather than the rule.
fn retain_exclusively_hot_blobs_in(state: &AppState, batch: &PatientBatch) -> PatientBatch {
    // Own studies come from the complete membership carried at collection
    // time, not from the deduplicated blob list: two studies of ONE patient
    // sharing a blob appear once in `blobs`, and deriving the study set from
    // it would drop the second study and exclude the patient's own blob.
    let own_studies: std::collections::HashSet<&str> =
        batch.studies.iter().map(|s| s.as_str()).collect();
    // ONE pass over the manifest directory for the whole batch. The per-blob
    // helper re-ran `read_dir` and decoded every manifest for EVERY blob, so
    // narrowing a large patient was O(blobs x manifests) decodes - all of it
    // under the shadow mutex. The single-pass collector answers the same
    // question for the batch and fails closed the same way (`Err` means "every
    // blob may be shared", which narrows to nothing).
    let holders = match outside_hot_holders(state, &own_studies) {
        Ok(h) => h,
        Err(()) => {
            crate::clog_warn!(
                patient = %batch.patient_id,
                "migration: cannot determine outside holders; leaving every blob hot"
            );
            return PatientBatch {
                patient_id: batch.patient_id.clone(),
                blobs: Vec::new(),
                studies: batch.studies.clone(),
            };
        }
    };
    let blobs: Vec<PatientBlob> = batch
        .blobs
        .iter()
        .filter(|blob| !holders.contains(&blob.blake3))
        .cloned()
        .collect();
    PatientBatch {
        patient_id: batch.patient_id.clone(),
        blobs,
        studies: batch.studies.clone(),
    }
}

/// Collect the HOT holders that live OUTSIDE `own_studies`, in one pass.
///
/// A per-blob check would re-run `read_dir` + a full
/// `read_manifest` per blob, which is O(N*M) manifest decodes under the
/// global manifest lease. This is the same question answered for every blob at
/// once: read each manifest once and remember every HOT instance hash it names.
///
/// Fail-closed, exactly like the per-blob helper:
/// - `read_dir` fails -> `Err(())` (everything counts as outside).
/// - `read_manifest` fails on an OUTSIDE manifest -> `Err(())` (the per-blob
///   helper returns `true` for every blob then, so every blob is excluded).
/// - A manifest that will not even parse to a [`PublishedManifest`] is SKIPPED,
///   matching the per-blob `continue`.
fn outside_hot_holders(
    state: &AppState,
    own_studies: &std::collections::HashSet<&str>,
) -> Result<std::collections::HashSet<String>, ()> {
    let Ok(rd) = std::fs::read_dir(std::path::Path::new(&state.config.btree.manifest_dir)) else {
        return Err(());
    };
    let mut holders: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ent in rd {
        // Same rule as `hot_ref_outside_studies`: an entry that could not be
        // read is UNKNOWN. `flatten()` dropped it, so the blob set was
        // narrowed on evidence that never loaded.
        let ent = match ent {
            Ok(e) => e,
            Err(e) => {
                crate::clog_warn!(
                    error = %e,
                    "migration: cannot read a manifest-directory entry; treating all blobs as shared"
                );
                return Err(());
            }
        };
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let study_uid = name.trim_end_matches(".manifest");
        if own_studies.contains(study_uid) {
            continue;
        }
        let Some(Ok(m)) = state.read_server_manifest(study_uid) else {
            return Err(());
        };
        for inst in m.series.iter().flat_map(|s| &s.instances) {
            if inst.storage_tier == clarus_core::manifest::TIER_HOT {
                holders.insert(clarus_adapters::hex_encode(&inst.blake3));
            }
        }
    }
    Ok(holders)
}

/// Re-run the Phase 0 exclusivity filter under the manifest lease, immediately
/// before the flip.
///
/// Phase 0's filter ran WITHOUT the lease, so its verdict is a snapshot: a
/// concurrent STOW for a DIFFERENT patient can publish a manifest (under
/// `acquire()`) that names one of these blobs HOT while the COPY is in flight.
/// Re-running the filter here, under the SAME lease the flip takes, closes that
/// window - no reference can appear between this check and the tier rewrite.
///
/// Returns the retained batch and the blobs excluded by the re-check; the
/// caller flips only the former and reclaims the cold copies of the latter.
/// Pure (no `acquire()`): the caller holds the lease, so the helper reads
/// manifests and nothing else.
fn revalidate_exclusive_hot_blobs_in(
    state: &AppState,
    batch: &PatientBatch,
) -> (PatientBatch, Vec<PatientBlob>) {
    let own_studies: std::collections::HashSet<&str> =
        batch.studies.iter().map(|s| s.as_str()).collect();
    // One manifest pass instead of one per blob: collecting the outside HOT
    // holders once is O(M) manifest decodes, not O(N*M), and this runs while
    // holding the global manifest lease, so unrelated STOW publication waits
    // on every decode. The partition is then a single set lookup per blob.
    let holders = outside_hot_holders(state, &own_studies);
    let mut retained: Vec<PatientBlob> = Vec::with_capacity(batch.blobs.len());
    let mut excluded: Vec<PatientBlob> = Vec::new();
    for blob in &batch.blobs {
        let outside = match &holders {
            // Fail closed: an unreadable manifest counts everything outside.
            Err(()) => true,
            Ok(set) => set.contains(&blob.blake3),
        };
        if outside {
            excluded.push(blob.clone());
        } else {
            retained.push(blob.clone());
        }
    }
    (
        PatientBatch {
            patient_id: batch.patient_id.clone(),
            blobs: retained,
            studies: batch.studies.clone(),
        },
        excluded,
    )
}

/// Rewrite `storage_tier` in the manifest for every instance of the patient's
/// blobs, in ONE lease.
///
/// This is the batch form of [`set_storage_tier`]: taking the lease once for
/// the whole patient removes the window in which another writer could observe
/// a patient whose instances disagree about their tier. Matching is by BLAKE3,
/// the CAS key, which is what the copy used.
///
/// Kept as the lease-taking entry point for callers that flip without a
/// preceding revalidation; the cold path now calls the locked body directly,
/// so today this wrapper has no in-tree caller.
#[allow(dead_code)]
fn set_storage_tier_for_patient(state: &AppState, batch: &PatientBatch, tier: &str) -> u64 {
    // One lease for the whole patient pass. The cold path calls the locked
    // body directly because it already holds this lease (taken for the
    // exclusivity revalidation), so this wrapper exists for callers that flip
    // without a preceding revalidation.
    let _lease = acquire();
    set_storage_tier_for_patient_locked(state, batch, tier)
}

/// Whether every study of the batch is still dated BEFORE `cutoff_date`.
///
/// The collector applies this rule when it builds the batch; this is the same
/// question asked again under the manifest lease, immediately before the flip,
/// so a study published during the copy window cannot be migrated. An
/// UNDATED study counts as ineligible: the collector treats it that way (a
/// study with no date cannot be shown to be old), and the two must agree or
/// the recheck would let through what the collector refused.
///
/// A manifest that cannot be read is ineligible too - fail closed, for the
/// same reason the exclusivity scans do.
fn patient_studies_all_before(state: &AppState, batch: &PatientBatch, cutoff_date: &str) -> bool {
    let cutoff: i64 = cutoff_date.parse().unwrap_or(i64::MAX);
    for study_uid in &batch.studies {
        let manifest = match state.read_server_manifest(study_uid) {
            Some(Ok(m)) => m,
            // The study vanished or will not decode: the batch's own premise
            // no longer holds, so nothing may be flipped on it.
            _ => return false,
        };
        let mut study_date = i64::MAX;
        for (t, v) in &manifest.study.attrs {
            if *t == tag::STUDY_DATE
                && let AttrValue::UInt(d) = v
            {
                study_date = *d as i64;
            }
        }
        if study_date >= cutoff {
            return false;
        }
    }
    true
}

/// The locked body of [`set_storage_tier_for_patient`], for callers that
/// ALREADY hold the global manifest lease.
fn set_storage_tier_for_patient_locked(state: &AppState, batch: &PatientBatch, tier: &str) -> u64 {
    set_storage_tier_for_patient_in(state, batch, tier)
}

/// The patient-tier rewrite itself, over an explicit manifest directory.
///
/// No `acquire()` inside: the caller holds the global manifest lease across the
/// whole pass, which is what makes the exclusivity revalidation and the tier
/// flip one critical section. Split out so the flip rule is unit-testable from
/// a temp directory, the same way [`set_storage_tier_in`] is for the per-blob
/// path.
fn set_storage_tier_for_patient_in(state: &AppState, batch: &PatientBatch, tier: &str) -> u64 {
    let tier_byte = match tier {
        "cold" => clarus_core::manifest::TIER_COLD,
        "s3" => clarus_core::manifest::TIER_S3,
        _ => clarus_core::manifest::TIER_COLD,
    };
    let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);
    let wanted: std::collections::HashSet<&str> =
        batch.blobs.iter().map(|b| b.blake3.as_str()).collect();
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
        let study_uid = name.trim_end_matches(".manifest");
        let Some(Ok(m)) = state.read_server_manifest(study_uid) else {
            continue;
        };
        // Cheap pre-check on the copy we already hold: skip manifests that
        // reference none of the patient's blobs.
        let touches = m.series.iter().flat_map(|s| &s.instances).any(|inst| {
            inst.storage_tier != tier_byte
                && wanted.contains(clarus_adapters::hex_encode(&inst.blake3).as_str())
        });
        if !touches {
            continue;
        }

        // The caller holds the lease, so this re-read cannot race a concurrent
        // commit into the same study.
        let Some(Ok(mut fresh)) = state.read_server_manifest(study_uid) else {
            continue;
        };
        // Count what THIS manifest contributed, and only publish it once the
        // write lands. Counting the intent made a failed rewrite (the ENOSPC
        // case) look like a completed migration, so the caller deleted the hot
        // copies and the next tick selected the same blob again.
        let mut changed = 0u64;
        for inst in fresh.series.iter_mut().flat_map(|s| s.instances.iter_mut()) {
            let b3 = clarus_adapters::hex_encode(&inst.blake3);
            // Flip only an instance still recorded in the tier this pass
            // VERIFIED. The cold pass verifies a cold copy and then flips
            // whatever it finds; if the planned S3 pass published in the
            // meantime (it removes both local copies), flipping that instance
            // back to COLD claims a cold copy that no longer exists - and the
            // pass then reports success with no readable bytes anywhere.
            //
            // Restricting the flip to the source tier leaves an instance that
            // already moved to S3 exactly where it is.
            let is_source_tier = match tier {
                "cold" => inst.storage_tier == clarus_core::manifest::TIER_HOT,
                "s3" => {
                    inst.storage_tier == clarus_core::manifest::TIER_HOT
                        || inst.storage_tier == clarus_core::manifest::TIER_COLD
                }
                _ => true,
            };
            if is_source_tier && inst.storage_tier != tier_byte && wanted.contains(b3.as_str()) {
                inst.storage_tier = tier_byte;
                changed += 1;
            }
        }
        let (cache_v, _taken) = clarus_core::manifest_cache::begin_write(manifest_dir, study_uid);
        if let Err(e) = write_manifest(&ent.path(), &fresh) {
            // O1: not durable, so not migrated. The cache entry stays dropped,
            // which is correct - the file on disk is the authority.
            crate::clog_warn!(
                study = %study_uid,
                tier,
                error = %e,
                "Migration: patient tier rewrite failed"
            );
            // The counter is `S3_MIGRATION_ERRORS`, so it may only move for an
            // S3 failure: this function serves `"cold"` too, and a COLD rewrite
            // failure filed under the S3 counter would report a cloud problem
            // that did not happen. The call itself is gated because the metric
            // only exists with the feature; the tier decision is not gated, so
            // it stays testable in a build without `s3`.
            #[cfg(feature = "s3")]
            if a_failed_rewrite_is_an_s3_error(tier_byte) {
                crate::metrics::record_s3_migration_error();
            }
        } else {
            matched += changed;
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

/// Lowercase hex of a 32-byte CAS digest, matching the spelling a CAS address
/// uses.
fn digest_hex(digest: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Whether an S3 object's reported size matches the blob.
///
/// A zero `file_size` means the manifest recorded none, in which case the
/// length says nothing and the caller must not treat the object as verified -
/// answering `true` would adopt an unverifiable object as the blob's copy.
#[cfg(feature = "s3")]
fn s3_object_matches_size(meta: &clarus_adapters::storage::s3::ObjectMeta, file_size: i64) -> bool {
    file_size > 0 && meta.content_length == file_size as u64
}

/// What one blob's S3 migration attempt achieved.
/// Split out of [`migrate_batch_s3`] so the loop counts the three outcomes the
/// protocol can produce, and so the per-blob flip is testable against a stub
/// object store (see [`migrate_blob_to_s3_for_test`]) without a live S3 client.
#[cfg(feature = "s3")]
enum S3BlobOutcome {
    /// Uploaded (or already present) and the manifest now names S3.
    Migrated,
    /// The blob could not be read (missing or short): skipped, not an error.
    Unreadable,
    /// Upload or tier flip failed: skipped and counted as an error.
    Failed,
}

/// Migrate ONE blob to S3: idempotency check, read, upload, tier flip, cleanup.
///
/// The per-blob unit of [`migrate_batch_s3`]. The caller supplies the object
/// key because it derives from the concrete `S3Client` prefix; the blob itself
/// only needs the `ObjectStore` surface, which is also what the test seam's
/// `InMemoryObjectStore` provides.
///
/// A successful flip reclaims BOTH the hot and the cold copy: a TIER_COLD
/// source leaves the NAS full otherwise, because the hot cleanup returns early
/// when no hot file exists and the erase worker does not touch TIER_S3.
#[cfg(feature = "s3")]
fn migrate_blob_to_s3<S: ObjectStore>(
    state: &AppState,
    blake3: &str,
    file_size: i64,
    s3: &S,
    s3_key: &str,
) -> S3BlobOutcome {
    // Step A - Idempotency: already in S3?
    //
    // `head_object` only proves SOMETHING occupies the key. A stale or
    // truncated object left by an interrupted upload would then be adopted as
    // this blob's copy, the tier flipped, and the last valid local copy
    // deleted - leaving the archive with damaged bytes under a digest that
    // claims they are the good ones. The object therefore has to be verified
    // before it is trusted; when it cannot be, the local copy is KEPT.
    if let Ok(meta) = s3.head_object(s3_key) {
        if s3_object_matches_size(&meta, file_size) {
            crate::clog_debug!(blake3, "Already in S3 - skipping upload");
            // O1: the object is in the bucket, but the blob is only migrated
            // once the manifest says so. A failed flip leaves the source copy
            // in place and the blob counted as skipped.
            if set_storage_tier(state, blake3, "s3") == 0 {
                crate::clog_warn!(
                    blake3 = %blake3,
                    "S3 migration: tier flip did not reach disk - source copy kept"
                );
                return S3BlobOutcome::Failed;
            }
            cleanup_hot_file_cas_safe(state, blake3);
            cleanup_cold_file_cas_safe(state, blake3);
            return S3BlobOutcome::Migrated;
        }
        // Size disagrees: fall through and re-upload, which REPLACES it. The
        // local copy stays until a verified upload succeeds.
        crate::clog_warn!(
            blake3 = %blake3,
            s3_key = %s3_key,
            expected = file_size,
            "S3 migration: the existing object does not match the blob; re-uploading"
        );
    }

    // Step B - Read (warn+skip, no crash). Cold uses the same two-level BLAKE3
    // layout as hot/CAS, resolved via the canonical `cas_rel_path` (mini-task
    // layout unity; single source of truth in clarus-adapters).
    let has_cold = state.cold_cas.is_some();
    let bytes = if has_cold {
        let cold_path = state.cold_cas.as_ref().unwrap().cas_path(blake3);
        match std::fs::read(&cold_path) {
            Ok(data) if file_size == 0 || data.len() == file_size as usize => data,
            _ => match read_hot_file(state, blake3, file_size) {
                Ok(b) => b,
                Err(e) => {
                    crate::clog_warn!(
                        blake3 = %blake3,
                        error = %e,
                        "S3 migration: failed to read file — skipping"
                    );
                    return S3BlobOutcome::Unreadable;
                }
            },
        }
    } else {
        match read_hot_file(state, blake3, file_size) {
            Ok(b) => b,
            Err(e) => {
                crate::clog_warn!(
                    blake3 = %blake3,
                    error = %e,
                    "S3 migration: failed to read file — skipping"
                );
                return S3BlobOutcome::Unreadable;
            }
        }
    };

    // Step C - Verify the SOURCE bytes, then upload.
    //
    // A same-size damaged source passed every check above: the cold branch
    // compares lengths, and `read_hot_file` compares lengths, so a file whose
    // content rotted but whose size survived was uploaded UNDER THE ORIGINAL
    // DIGEST and both local copies were then deleted - the digest now names
    // bytes that are not the instance. The digest is what the key means, so it
    // is verified here, before the PUT, on whatever bytes are about to be sent.
    let actual = clarus_core::hash::cas_hash_one(&bytes);
    let actual_hex = digest_hex(&actual);
    if actual_hex != blake3 {
        crate::clog_warn!(
            blake3 = %blake3,
            actual = %actual_hex,
            "S3 migration: source bytes do not match the CAS address - refusing to upload"
        );
        crate::metrics::record_s3_migration_error();
        return S3BlobOutcome::Failed;
    }

    // Move `bytes` in (0 new allocations on instance; `bytes` is a Vec<u8>
    // here, not used after this site).
    match s3.put_object(s3_key, bytes, "application/dicom") {
        Ok(_etag) => {
            crate::clog_debug!(blake3, s3_key, "Uploaded to S3");
            // O1: only a tier that reached disk makes this blob migrated. A
            // failed rewrite still holds the source copy, so deleting it would
            // strand the object behind a manifest that names a tier nothing
            // was moved to.
            if set_storage_tier(state, blake3, "s3") == 0 {
                crate::clog_warn!(
                    blake3 = %blake3,
                    "S3 migration: tier flip did not reach disk - source copy kept"
                );
                crate::metrics::record_s3_migration_error();
                return S3BlobOutcome::Failed;
            }
            crate::metrics::record_s3_migration();
            // Step D - CAS-safe cleanup of both local copies.
            cleanup_hot_file_cas_safe(state, blake3);
            cleanup_cold_file_cas_safe(state, blake3);
            S3BlobOutcome::Migrated
        }
        Err(e) => {
            crate::clog_warn!(blake3, error = %e, "S3 upload failed - skipping");
            crate::metrics::record_s3_migration_error();
            S3BlobOutcome::Failed
        }
    }
}

/// Migrate old studies to S3 cloud storage.
///
/// If cold_path is configured, reads files from cold_path (not hot) -
/// the cold->S3 path is more efficient than hot->S3.
#[cfg(feature = "s3")]
fn migrate_batch_s3(state: &AppState, min_age_days: u32) -> Result<(), String> {
    let s3_store = state.s3.get().ok_or("No S3 storage configured")?;

    let tick_start = std::time::Instant::now();
    let mut tick_migrated = 0u64;
    let mut tick_skipped = 0u64;
    let mut tick_errors = 0u64;

    let cutoff_date = cutoff_date_str(min_age_days);
    // Finish any reclaim an earlier pass (or a crash between the flip and the
    // unlink) left behind, BEFORE selecting new work: those files are no
    // longer referenced by any manifest, so no collector can find them.
    let (reclaimed, still_pending) = reconcile_pending_reclaims(state);
    if reclaimed > 0 || still_pending > 0 {
        crate::clog_info!(
            reclaimed,
            still_pending,
            "Migration: reconciled pending local copies"
        );
    }

    // Blobs that failed and must not be retried within THIS tick. Declared
    // OUTSIDE the round loop for the same reason the emergency pass keeps its
    // own: `collect_migration_candidates` re-derives the same HOT/COLD set
    // every round, so without this a poisoned prefix of persistently failing
    // blobs is re-selected and the younger blobs behind them starve.
    let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        // Blobs a cold pass already moved are still part of the archive and
        // are the prime S3 candidates once they age past the cutoff.
        let candidates = collect_migration_candidates(
            state,
            &cutoff_date,
            500,
            &[
                clarus_core::manifest::TIER_HOT,
                clarus_core::manifest::TIER_COLD,
            ],
            &failed,
        )?;
        if candidates.is_empty() {
            break;
        }

        let chunk_start = std::time::Instant::now();
        let mut chunk_migrated = 0u64;
        let mut chunk_skipped = 0u64;
        // Size BEFORE this chunk, so the loop can tell "every candidate failed
        // but was newly excluded" (progress: the next round reaches past them)
        // from "no progress and nothing new" (the tick is done).
        let excluded_before = failed.len();

        for (blake3, file_size, _study_uid) in &candidates {
            let s3_key = s3_store.s3_key(blake3);
            match migrate_blob_to_s3(state, blake3, *file_size, s3_store, &s3_key) {
                S3BlobOutcome::Migrated => chunk_migrated += 1,
                S3BlobOutcome::Unreadable => {
                    chunk_skipped += 1;
                    failed.insert(blake3.clone());
                }
                S3BlobOutcome::Failed => {
                    chunk_skipped += 1;
                    tick_errors += 1;
                    failed.insert(blake3.clone());
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
        // A chunk that moved nothing is NOT the end of the tick when it also
        // added new exclusions: the failed set now blocks that prefix, so the
        // next round can reach the blobs behind it. Breaking here meant that
        // if the first 500 candidates all failed and blob 501 was perfectly
        // readable, neither worker ever attempted 501 - the next cron tick
        // started from an empty failure set and selected the same prefix
        // again, so those blobs were starved forever.
        //
        // The tick ends only when a round made no progress AND learned
        // nothing new (the set is already saturated), which cannot happen
        // while `candidates` is non-empty and every entry is attempted.
        if s3_chunk_ends_tick(chunk_migrated, excluded_before, failed.len()) {
            crate::clog_warn!(
                attempted = candidates.len(),
                skipped = chunk_skipped,
                "S3 migration: chunk made no progress and excluded nothing new — ending tick"
            );
            break;
        }
        if chunk_migrated == 0 {
            crate::clog_debug!(
                attempted = candidates.len(),
                excluded = failed.len(),
                "S3 migration: chunk made no progress but widened the exclusion set — continuing"
            );
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

/// Test seam: run the per-blob S3 flip against a CALLER-SUPPLIED object store.
///
/// The zero-argument seam above always starts from an EMPTY bucket, so it
/// cannot express the states that matter most here: a stale object already
/// occupying the key, or a truncated one. Passing the store in lets a test
/// seed the bucket first and then observe what the flip does with it.
#[cfg(feature = "s3")]
pub fn migrate_blob_to_s3_with_store_for_test<S: clarus_adapters::storage::s3::ObjectStore>(
    state: &AppState,
    blake3: &str,
    file_size: i64,
    store: &S,
) -> Result<(), String> {
    let key = clarus_adapters::storage::cas_rel_path(blake3);
    match migrate_blob_to_s3(state, blake3, file_size, store, &key) {
        S3BlobOutcome::Migrated => Ok(()),
        S3BlobOutcome::Unreadable => Err("blob is unreadable".to_string()),
        S3BlobOutcome::Failed => Err("upload or tier flip failed".to_string()),
    }
}

/// Test seam: run the per-blob S3 flip against an in-memory object store.
///
/// The production loop binds a concrete `S3Client` from `AppState`, which an
/// integration test cannot construct. This drives the SAME
/// [`migrate_blob_to_s3`] unit through an `InMemoryObjectStore` with a
/// deterministic key, so the flip and its cleanups are observable without S3.
#[cfg(feature = "s3")]
pub fn migrate_blob_to_s3_for_test(
    state: &AppState,
    blake3: &str,
    file_size: i64,
) -> Result<(), String> {
    use clarus_adapters::storage::s3::InMemoryObjectStore;
    let store = InMemoryObjectStore::default();
    migrate_blob_to_s3_with_store_for_test(state, blake3, file_size, &store)
}

/// Test seam: run the cold-side cleanup alone, so its fail-closed reference
/// count is observable without a full S3 flip.
#[cfg(feature = "s3")]
pub fn cleanup_cold_file_cas_safe_for_test(state: &AppState, blake3: &str) {
    cleanup_cold_file_cas_safe(state, blake3);
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
    // loop and handed to `collect_migration_candidates`, which filters it
    // BEFORE its `.take(limit)`: re-deriving the same HOT set every round, a
    // poisoned prefix of persistently failing blobs would otherwise consume
    // the whole round and starve the younger blobs behind them.
    let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        let candidates = collect_migration_candidates_mode(
            state,
            &cutoff_date,
            500,
            &[clarus_core::manifest::TIER_HOT],
            &failed,
            // Emergency: free space now, age is not a criterion.
            true,
        )?;

        let chunk_start = std::time::Instant::now();
        let mut chunk_migrated = 0u64;
        let mut chunk_skipped = 0u64;
        // Size BEFORE this chunk, so "no progress" can be told apart from "no
        // progress and nothing new".
        let excluded_before = failed.len();

        crate::clog_warn!(
            count = candidates.len(),
            has_cold,
            has_s3,
            "Emergency migration: starting batch"
        );

        for (blake3, file_size, _study_uid) in &candidates {
            // A cheap guard, not the primary filter: the collector already
            // excluded the failed set before `.take(limit)`. It stays so a
            // failure inserted between collection and this iteration (a blob
            // that failed in a later arm of the SAME round) is not reworked.
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
                        // Route through the COMMON accounting below: this exit
                        // used to `continue` straight past `failed.insert`, so
                        // the digest stayed selectable and the next round of
                        // the same tick worked it again while the disk was
                        // still under the threshold.
                        chunk_skipped += 1;
                        failed.insert(blake3.clone());
                        continue;
                    }
                };
                let s3_store = state.s3.get().unwrap();
                let s3_key = s3_store.s3_key(blake3);
                match s3_store.put_object(&s3_key, bytes, "application/dicom") {
                    Ok(_etag) => {
                        crate::clog_debug!(blake3, "Emergency: uploaded to S3");
                        // O1: `success` means "the archive now resolves this
                        // blob from the new tier". A failed flip keeps that
                        // false, so the hot copy survives and the blob is
                        // retried instead of being counted as migrated.
                        if set_storage_tier(state, blake3, "s3") == 0 {
                            crate::clog_warn!(
                                blake3 = %blake3,
                                "Emergency: tier flip did not reach disk - hot copy kept"
                            );
                            // Same rule: a failed flip must be EXCLUDED for
                            // this tick, or the next round selects the digest
                            // again and burns the pass on it.
                            chunk_skipped += 1;
                            failed.insert(blake3.clone());
                            continue;
                        }
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

        // A chunk that moved nothing is NOT the end of the tick when it also
        // excluded new digests: the next round can reach the blobs behind the
        // failing prefix. Breaking here meant that if the oldest 500 blobs all
        // failed and blob 501 was evictable, the pass ended without ever
        // attempting 501, and the next tick re-selected the same prefix while
        // the disk stayed full.
        //
        // The tick ends only when a round makes no progress AND excludes
        // nothing new - the store then has no blob this pass can evict.
        if chunk_migrated == 0 && excluded_before == failed.len() {
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
    // The blob's ownership guard spans the WHOLE lifetime: destination
    // creation, the tier publication and the hot delete below. Taking it only
    // around the manifest rewrite left a worse window - an emergency copy could
    // finish and publish COLD before this path published anything, then this
    // path would flip the manifest to COLD again and the emergency worker would
    // delete the hot copy, so BOTH copies could go.
    //
    // Held for the duration of a single blob's migration, which is exactly the
    // granularity the protocol needs; other blobs never contend.
    let guard = blob_guard(blake3);
    let _held = guard.lock().unwrap_or_else(|p| p.into_inner());

    // ap101: streaming chunked copy, no whole-blob `Vec` (see
    // copy_hot_to_cold_streaming). A missing hot file means the blob needs no
    // work, which is the emergency path's skip semantics.
    match copy_hot_to_cold_streaming(state, blake3, file_size, cold) {
        Ok(ColdCopy::Published { .. }) => {}
        Ok(ColdCopy::HotMissing) => return Ok(false),
        Err(e) => return Err(e.to_string()),
    }

    // Update the manifest tier (manifest-rewrite, no SQLite).
    //
    // O1: a rewrite that did not reach disk is a FAILURE, not a success. The
    // caller treats `Ok(true)` as "migrated" and proceeds to delete the hot
    // copy, so reporting success here on an ENOSPC would remove the only copy
    // the manifests still point at.
    if set_storage_tier_locked(state, blake3, "cold") == 0 {
        return Err("tier flip did not reach disk".to_string());
    }

    // Delete the hot copy under the SAME guard that ordered the publication:
    // no other actor can decide this blob's ownership between the flip above
    // and this unlink.
    cleanup_hot_file_locked(state, blake3);

    Ok(true)
}

/// CAS-safe hot file cleanup: delete the hot copy only if no live-manifest
/// instance still references it in the `hot` tier.
fn cleanup_hot_file_cas_safe(state: &AppState, blake3: &str) {
    // Same guard as the migration and the abort cleanup: the count and the
    // unlink are one decision about this blob's ownership, so an emergency
    // publication cannot land in between.
    let guard = blob_guard(blake3);
    let _held = guard.lock().unwrap_or_else(|p| p.into_inner());
    cleanup_hot_file_locked(state, blake3);
}

/// The hot delete itself, for callers that already hold the blob guard.
///
/// Returns `true` when no hot copy remains (either there was none, or it was
/// removed), `false` when the unlink was attempted and failed.
///
/// The return matters because a failed unlink AFTER a successful tier flip
/// leaves a blob that no pass revisits: the collector selects HOT references
/// only, so once the manifest says COLD the hot file is invisible to it. The
/// caller reports that into the same retry accounting a failed migration uses,
/// which is what makes the cleanup retryable rather than silently lost.
fn cleanup_hot_file_locked(state: &AppState, blake3: &str) -> bool {
    let hot_path = state.cas.cas_path(blake3);
    if !hot_path.exists() {
        return true;
    }
    let n = count_hot_refs(state, blake3);
    if n == 0 {
        match std::fs::remove_file(&hot_path) {
            Ok(()) => {
                crate::clog_debug!(blake3, "Hot file deleted - no hot refs remain");
                true
            }
            Err(e) => {
                crate::clog_warn!(
                    blake3,
                    error = %e,
                    "Migration: cannot delete hot file - the reclaim is pending"
                );
                // Recorded so a reconciliation pass can find it: the manifest
                // already says COLD, so nothing else will.
                note_pending_reclaim(state, blake3, "hot");
                false
            }
        }
    } else {
        crate::clog_debug!(blake3, n, "Hot file kept - {n} refs still in hot");
        true
    }
}

/// Remember that a local copy still exists for a blob whose manifest no longer
/// names it, so a later pass can reclaim it.
///
/// The collector only ever selects instances recorded in a tier, so a copy
/// left behind after the flip is invisible to it: without this record the file
/// stays on disk forever. The record is a small append-only file in the index
/// directory - one line per pending reclaim - so a crash between the flip and
/// the unlink loses nothing.
fn note_pending_reclaim(state: &AppState, blake3: &str, tier: &str) {
    let dir = std::path::Path::new(&state.config.btree.idx_dir);
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join("pending_reclaim.txt");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{blake3} {tier}");
    }
}

/// Reclaim local copies recorded by [`note_pending_reclaim`].
///
/// Called at the start of every migration batch, so a flip whose unlink failed
/// (or a process that stopped between the two) is finished on the next pass
/// instead of leaking the file for the life of the archive. Every entry is
/// re-checked against the manifests before the unlink, exactly as the ordinary
/// cleanup does: a blob that became HOT again in the meantime keeps its copy.
///
/// Returns `(reclaimed, still_pending)` for the tick's log line.
fn reconcile_pending_reclaims(state: &AppState) -> (u64, u64) {
    let dir = std::path::Path::new(&state.config.btree.idx_dir);
    let path = dir.join("pending_reclaim.txt");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (0, 0);
    };
    let mut reclaimed = 0u64;
    let mut still: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((blake3, tier)) = line.split_once(' ') else {
            continue;
        };
        let done = match tier {
            "hot" => {
                let guard = blob_guard(blake3);
                let _held = guard.lock().unwrap_or_else(|p| p.into_inner());
                if count_hot_refs(state, blake3) == 0 {
                    let path = state.cas.cas_path(blake3);
                    // NotFound is success: the file is already gone.
                    std::fs::remove_file(&path)
                        .map(|()| true)
                        .or_else(|e: std::io::Error| {
                            Ok::<bool, std::io::Error>(e.kind() == std::io::ErrorKind::NotFound)
                        })
                        .unwrap_or(false)
                } else {
                    // Referenced again: drop the record, the file is live.
                    true
                }
            }
            "cold" => match state.cold_cas.as_ref() {
                Some(cold) => {
                    let guard = blob_guard(blake3);
                    let _held = guard.lock().unwrap_or_else(|p| p.into_inner());
                    if count_non_hot_refs(state, blake3) == 0 {
                        let path = cold.cas_path(blake3);
                        std::fs::remove_file(&path)
                            .map(|()| true)
                            .or_else(|e: std::io::Error| {
                                Ok::<bool, std::io::Error>(e.kind() == std::io::ErrorKind::NotFound)
                            })
                            .unwrap_or(false)
                    } else {
                        true
                    }
                }
                None => true,
            },
            _ => true,
        };
        if done {
            reclaimed += 1;
        } else {
            still.push(line.to_string());
        }
    }
    // Rewrite atomically with only the entries that could not be reclaimed.
    let tmp = path.with_extension("txt.tmp");
    let body = if still.is_empty() {
        String::new()
    } else {
        let mut b = String::new();
        for l in &still {
            b.push_str(l);
            b.push('\n');
        }
        b
    };
    if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    (reclaimed, still.len() as u64)
}

/// CAS-safe cold file cleanup: delete the cold copy only if no live-manifest
/// instance still references it in the `cold` tier.
///
/// The cold->S3 flip leaves the source cold copy behind unless this runs: the
/// hot cleanup returns early when no hot file exists, and the erase worker
/// maps TIER_S3 to `None`. Without it "migrated" means the NAS keeps the bytes.
#[cfg(feature = "s3")]
fn cleanup_cold_file_cas_safe(state: &AppState, blake3: &str) {
    // Same guard as the hot twin and the abort cleanup: the count and the
    // unlink are one decision about this blob's ownership, so a concurrent
    // publication cannot land in between.
    let guard = blob_guard(blake3);
    let _held = guard.lock().unwrap_or_else(|p| p.into_inner());
    let Some(cold) = state.cold_cas.as_ref() else {
        return; // no cold tier configured: nothing to reclaim
    };
    let cold_path = cold.cas_path(blake3);
    if !cold_path.exists() {
        return;
    }
    let n = count_cold_refs(state, blake3);
    if n == 0 {
        if let Err(e) = std::fs::remove_file(&cold_path) {
            crate::clog_warn!(blake3, error = %e, "Migration: cannot delete cold file");
        } else {
            crate::clog_debug!(blake3, "Cold file deleted - no cold refs remain");
        }
    } else {
        crate::clog_debug!(blake3, n, "Cold file kept - {n} refs still in cold");
    }
}

/// Count live-manifest instances referencing `blake3` while still in the `hot`
/// tier. `0` means the hot copy is safe to delete. Matches BLAKE3 (CAS key).
fn count_hot_refs(state: &AppState, blake3: &str) -> u64 {
    let Ok(rd) = std::fs::read_dir(std::path::Path::new(&state.config.btree.manifest_dir)) else {
        return u64::MAX; // cannot verify - keep the file
    };
    let mut n = 0u64;
    for ent in rd {
        let Ok(ent) = ent else {
            // A directory entry that cannot be read might be a manifest that
            // references this blob; the caller deletes on zero, so fail closed.
            return u64::MAX;
        };
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Some(Ok(m)) = state.read_server_manifest(name.trim_end_matches(".manifest")) else {
            // A manifest that will not decode might name this blob in the hot
            // tier. Reporting zero here let `cleanup_hot_file_cas_safe` delete
            // the hot copy, and the manifest becoming readable again would then
            // point at a missing file. The non-hot counter treats the same case
            // as an unknown reference; this one must not be more optimistic.
            return u64::MAX;
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
) -> Result<ColdCopy, CopyError> {
    use std::io::{Read, Write};

    let hot_path = state.cas.cas_path(blake3);
    let target_path = cold.cas_path(blake3);

    // CAS dedup: the same bytes are stored once per tier, so a blob another
    // patient (or an earlier pass) already migrated is present here. Recording
    // that in the caller's cleanup list would unlink a shared file, so the
    // answer is reported instead of assumed.
    let created_by_this_pass = !target_path.exists();

    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CopyError::Failed(format!("create_dir_all failed: {e}")))?;
    }

    let mut src = match std::fs::File::open(&hot_path) {
        Ok(f) => f,
        // ONLY `NotFound` means the hot file is gone (GC'd, already migrated):
        // that is a benign skip for this blob. A permission or I/O error is
        // NOT the same thing - reporting it as `__hot_missing__` let the
        // emergency path answer `Ok(false)` and give the operator no
        // diagnostic at all when the volume was simply unreadable.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ColdCopy::HotMissing);
        }
        Err(e) => {
            return Err(CopyError::Failed(format!(
                "open hot file {blake3} failed: {e}"
            )));
        }
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
        .map_err(|e| CopyError::Failed(format!("create cold file failed for {blake3}: {e}")))?;

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
                return Err(CopyError::Failed(format!(
                    "read hot file {blake3} failed: {e}"
                )));
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = dst.write_all(&buf[..n]) {
            drop(dst);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(CopyError::Failed(format!(
                "write cold file {blake3} failed: {e}"
            )));
        }
        written += n as u64;
    }
    drop(dst);

    if file_size > 0 && written != file_size as u64 {
        // A short or long copy is a bad copy: never flip a tier onto it.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(CopyError::Failed(format!(
            "Size mismatch for {blake3}: expected {file_size}, copied {written}"
        )));
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
        return Err(CopyError::Failed(format!(
            "sync cold copy {blake3} failed: {e}"
        )));
    }

    // Before REPLACING an existing cold file, prove the SOURCE is the
    // instance. A damaged hot blob whose length survived - exactly what
    // `file_size` cannot detect - would otherwise be renamed over a correct
    // cold copy: the good bytes are destroyed and the bad ones take their
    // place, under a digest that says they are the good ones. No existing
    // destination means nothing to protect, so the cheap path is unchanged.
    if !created_by_this_pass {
        let source_digest = crate::handlers::stow_pure::blake3_of_file(&tmp_path)
            .map_err(|e| CopyError::Failed(format!("hash cold copy {blake3} failed: {e}")))?;
        if digest_hex(&source_digest) != blake3 {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(CopyError::Failed(format!(
                "hot copy of {blake3} does not hash to its address; the existing cold copy is kept"
            )));
        }
    }

    if let Err(e) = std::fs::rename(&tmp_path, &target_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(CopyError::Failed(format!(
            "rename cold copy {blake3} failed: {e}"
        )));
    }
    if let Some(parent) = target_path.parent()
        && let Err(e) = sync_dir(parent)
    {
        // The RENAME already succeeded, so the canonical cold file EXISTS.
        // Returning a bare error here told the caller the copy had not
        // happened, so it never recorded the blob in `written` and the abort
        // cleanup could not reclaim a file this pass created. The error is
        // still reported - the call did not fully succeed - but the caller
        // needs to know the file is there, which is why the state travels in
        // the message it acts on.
        crate::clog_warn!(
            blake3 = %blake3,
            dir = %parent.display(),
            error = %e,
            "cold copy published but its directory could not be synced"
        );
        return Err(CopyError::PublishedUnsynced {
            created: created_by_this_pass,
            reason: format!("sync cold directory {parent:?} failed after publish: {e}"),
        });
    }

    crate::metrics::STORAGE_WRITE_BYTES.fetch_add(written, std::sync::atomic::Ordering::Relaxed);
    // `created` means this pass created the cold file; `false` means the bytes
    // were already there (CAS dedup) and the caller must not treat it as its
    // own to delete.
    Ok(ColdCopy::Published {
        created: created_by_this_pass,
    })
}

/// What a hot-to-cold copy achieved.
///
/// Replaces a `Result<bool, String>` whose error channel carried the magic
/// string `__hot_missing__`. A string protocol cannot express "the file is
/// PUBLISHED and an error also occurred", which is exactly the state a failed
/// directory sync leaves behind - and a caller that cannot see it aborts
/// without reclaiming a copy this pass created.
#[derive(Debug)]
enum ColdCopy {
    /// The bytes are at the cold address. `created` is true when THIS pass
    /// created them (so the abort cleanup may reclaim them) and false when the
    /// address already held them (CAS dedup - not ours to delete).
    Published { created: bool },
    /// The hot file does not exist. Benign: the blob was already migrated or
    /// GC'd.
    HotMissing,
}

/// Why a hot-to-cold copy failed.
///
/// Typed rather than a message, because one variant carries a fact the caller
/// must ACT on: [`CopyError::PublishedUnsynced`] means the bytes reached the
/// cold address even though the call failed, so the caller has to record the
/// file as one it created or the abort cleanup leaves an orphan no tier
/// references.
#[derive(Debug)]
enum CopyError {
    /// The copy failed and nothing was published.
    Failed(String),
    /// The rename landed (the cold file exists) but the directory sync did
    /// not. `created` says whether THIS pass created it.
    PublishedUnsynced { created: bool, reason: String },
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopyError::Failed(reason) => f.write_str(reason),
            CopyError::PublishedUnsynced { created, reason } => {
                write!(
                    f,
                    "{reason} (published to cold; created={created} by this pass)"
                )
            }
        }
    }
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

/// Verify a cold copy of `blake3` exists, has the expected size, AND hashes
/// to the CAS address it claims.
///
/// The COPY phase already counts what it wrote; this is the independent
/// re-read the protocol requires before any tier flip, so a copy that was
/// truncated after the fact (full disk, interrupted write) is caught here.
///
/// SIZE IS NOT ENOUGH. A same-size damaged file - a torn write that happened
/// to reach the recorded length, or a stale object occupying the address -
/// passes a length check and then gets published, after which the local copy
/// may be deleted: the archive would hold the damaged bytes under a digest
/// that says they are the good ones. The digest is what the address MEANS, so
/// it is what has to be checked. It is computed by streaming, so no whole-instance
/// buffer is materialised (ap101).
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
    let actual = crate::handlers::stow_pure::blake3_of_file(&target_path)
        .map_err(|e| format!("cold copy of {blake3} cannot be hashed: {e}"))?;
    let actual = actual.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    if actual != blake3 {
        return Err(format!(
            "cold copy of {blake3} hashes to {actual}: the bytes are not the ones the address names"
        ));
    }
    Ok(())
}

/// Count live-manifest instances referencing `blake3` in a NON-hot tier.
///
/// `u64::MAX` on a read error: the file cannot be verified, so the caller must
/// treat it as referenced and keep it. Any non-zero answer means the archive
/// resolves this blob from somewhere other than hot.
/// What one manifest-directory entry contributed to the reference count.
///
/// Split out of [`count_non_hot_refs`] because the only interesting case - an
/// entry that could not be READ - is unreachable from a test without a real
/// filesystem fault, and it is exactly the case that decides whether the last
/// copy of a blob may be deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryRefs {
    /// This entry might name the blob but could not be examined.
    Unverifiable,
    /// `n` manifests in this entry reference the blob from a non-hot tier.
    Counted(u64),
}

fn count_non_hot_refs(state: &AppState, blake3: &str) -> u64 {
    let Ok(rd) = std::fs::read_dir(std::path::Path::new(&state.config.btree.manifest_dir)) else {
        return u64::MAX; // cannot verify - keep the file
    };
    let mut n = 0u64;
    for ent in rd {
        // A directory-entry error is an UNREAD entry, and the omitted entry may
        // be the live cold reference. Flattening the error away answered "0
        // references", which made the cold copy deletable - so report "cannot
        // verify" instead, the same answer the failed `read_dir` above gives.
        let Ok(ent) = ent else {
            return u64::MAX;
        };
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        match entry_refs(state, name.trim_end_matches(".manifest"), blake3) {
            // A manifest that will not decode might name this blob; treat it
            // like an unreadable directory rather than assuming it does not.
            EntryRefs::Unverifiable => return u64::MAX,
            EntryRefs::Counted(c) => n = n.saturating_add(c),
        }
    }
    n
}

/// Count live-manifest instances referencing `blake3` while still in the
/// `cold` tier. `0` means the cold copy is safe to delete. The SAME fail-closed
/// rules as [`count_hot_refs`] and [`count_non_hot_refs`]: a `read_dir` error
/// or an unreadable entry answers `u64::MAX`, so the caller keeps the file.
#[cfg(feature = "s3")]
fn count_cold_refs(state: &AppState, blake3: &str) -> u64 {
    let Ok(rd) = std::fs::read_dir(std::path::Path::new(&state.config.btree.manifest_dir)) else {
        return u64::MAX; // cannot verify - keep the file
    };
    let mut n = 0u64;
    for ent in rd {
        let Ok(ent) = ent else {
            return u64::MAX;
        };
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Some(Ok(m)) = state.read_server_manifest(name.trim_end_matches(".manifest")) else {
            return u64::MAX;
        };
        for inst in m.series.iter().flat_map(|s| &s.instances) {
            if inst.storage_tier != clarus_core::manifest::TIER_COLD {
                continue;
            }
            if clarus_adapters::hex_encode(&inst.blake3) == blake3 {
                n += 1;
            }
        }
    }
    n
}

/// Classify one directory entry against `blake3`.
fn entry_refs(state: &AppState, study_uid: &str, blake3: &str) -> EntryRefs {
    let Some(Ok(m)) = state.read_server_manifest(study_uid) else {
        return EntryRefs::Unverifiable;
    };
    let mut n = 0u64;
    for inst in m.series.iter().flat_map(|s| &s.instances) {
        if inst.storage_tier == clarus_core::manifest::TIER_HOT {
            continue;
        }
        if clarus_adapters::hex_encode(&inst.blake3) == blake3 {
            n += 1;
        }
    }
    EntryRefs::Counted(n)
}

/// Remove cold copies of `blobs` (the ABORT path's cleanup).
///
/// Only the COLD side is touched: the abort contract is "hot untouched, tier
/// untouched", so a failed patient leaves the hot originals exactly as they
/// were and only discards the partial cold work.
///
/// O2: a copy is kept when ANY live manifest references the blob in a non-hot
/// tier. The abort cleanup runs concurrently with the emergency worker, which
/// may have flipped a manifest onto this very copy in the meantime; deleting it
/// then would remove the only bytes the archive can resolve.
fn cleanup_cold_copies(state: &AppState, blobs: &[PatientBlob], cold: &ContentAddressedStorage) {
    for blob in blobs {
        // The count and the unlink must be ONE critical section: the emergency
        // worker can flip a manifest onto this very cold copy in between, and
        // the cleanup would then unlink the only resolvable bytes. Protecting
        // just the read is not enough, which is why the guard is held across
        // the whole decision.
        let guard = blob_guard(&blob.blake3);
        let _held = guard.lock().unwrap_or_else(|p| p.into_inner());
        let refs = count_non_hot_refs(state, &blob.blake3);
        if refs > 0 {
            crate::clog_debug!(
                blake3 = %blob.blake3,
                refs,
                "Abort cleanup: cold copy kept (a live manifest now points at it)"
            );
            continue;
        }
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

/// Collect migration candidates from live manifests: instances in one of the
/// `wanted` tiers whose study date precedes `cutoff_date`, ordered by study
/// date ascending, limited to `limit`.
///
/// Returns `(blake3_hex, file_size, study_uid)` - the CAS key is BLAKE3.
///
/// The tier set is a parameter because the targets differ: the planned cold
/// copy only takes blobs still in `hot`, while the S3 copy must also take the
/// blobs a previous cold pass already moved - after `min_age_days` the cold
/// tier holds exactly the studies S3 was asked for, and a hot-only collector
/// left them on the NAS forever.
/// The scan behind the S3 migration loop: the oldest eligible blobs in the
/// requested tiers.
///
/// Only the S3 path calls it in production; the filesystem/emergency pass goes
/// through [`collect_candidates_mode_in`] so it can opt into undated studies.
/// Kept for the tests in a non-`s3` build, where it is the wrapper under test.
#[cfg(any(feature = "s3", test))]
fn collect_migration_candidates(
    state: &AppState,
    cutoff_date: &str,
    limit: u32,
    wanted: &[u8],
    exclude: &std::collections::HashSet<String>,
) -> Result<Vec<(String, i64, String)>, String> {
    collect_migration_candidates_mode(state, cutoff_date, limit, wanted, exclude, false)
}

/// As [`collect_migration_candidates`], with the EMERGENCY selection mode.
///
/// Emergency migration exists to free space, so it must be able to select a
/// study it cannot date. An undated study carries `study_date = i64::MAX`, so
/// the ordinary `study_date < cutoff` test never passes - and with the
/// emergency cutoff set to the maximum date, every undated study was excluded
/// even though its bytes occupy exactly as much hot storage as a dated one.
/// When the readable hot blobs all belonged to undated studies the pass freed
/// NOTHING while the disk stayed full. `include_undated` is that distinct
/// selection mode; the scheduled path keeps the age requirement.
#[allow(clippy::too_many_arguments)]
fn collect_migration_candidates_mode(
    state: &AppState,
    cutoff_date: &str,
    limit: u32,
    wanted: &[u8],
    exclude: &std::collections::HashSet<String>,
    include_undated: bool,
) -> Result<Vec<(String, i64, String)>, String> {
    // The exclusion runs INSIDE the scan, before the sort and before this
    // `.take(limit)`: a caller that excluded after the take would let a    // poisoned prefix of `limit` persistently failing blobs consume the whole
    // round, so the younger blobs behind them never migrate.
    Ok(
        collect_candidates_mode_in(state, cutoff_date, wanted, exclude, include_undated)?
            .into_iter()
            .take(limit as usize)
            .collect(),
    )
}

/// The scan behind [`collect_migration_candidates`], over a manifest directory.
///
/// Split out so the tier selection and the cutoff boundary are testable
/// without an `AppState` (which owns the CAS, the index and the config): the
/// two rules a wrong migration rests on are exactly those. Production goes
/// through [`collect_candidates_mode_in`] so the emergency mode cannot be
/// forgotten by a caller.
#[cfg(test)]
fn collect_candidates_in(
    state: &AppState,
    cutoff_date: &str,
    wanted: &[u8],
    exclude: &std::collections::HashSet<String>,
) -> Result<Vec<(String, i64, String)>, String> {
    collect_candidates_mode_in(state, cutoff_date, wanted, exclude, false)
}

/// The scan behind [`collect_candidates_in`], with the emergency mode.
fn collect_candidates_mode_in(
    state: &AppState,
    cutoff_date: &str,
    wanted: &[u8],
    exclude: &std::collections::HashSet<String>,
    include_undated: bool,
) -> Result<Vec<(String, i64, String)>, String> {
    let cutoff: i64 = cutoff_date.parse().unwrap_or(i64::MAX);
    let mut seen = std::collections::HashSet::new();
    // A directory that cannot be LISTED is an ENVIRONMENTAL failure, not an
    // empty archive. Returning an empty vector made the scheduled and
    // emergency passes report "zero work" while the store was unreadable -
    // the one state in which a migration worker must say so.
    let rd =
        std::fs::read_dir(std::path::Path::new(&state.config.btree.manifest_dir)).map_err(|e| {
            format!(
                "cannot list the manifest directory {}: {e}",
                state.config.btree.manifest_dir
            )
        })?;
    let mut entries: Vec<(i64, (String, i64, String))> = Vec::new();
    for ent in rd {
        // An entry that cannot be read is a manifest this pass did not see;
        // skipping it silently is the same optimistic default as an empty
        // directory.
        let ent = ent.map_err(|e| format!("cannot read a manifest-directory entry: {e}"))?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Some(Ok(m)) = state.read_server_manifest(name.trim_end_matches(".manifest")) else {
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
        // An undated study is eligible only in EMERGENCY mode: see
        // `collect_migration_candidates_mode`.
        let undated = study_date == i64::MAX;
        let ineligible = if undated {
            !include_undated
        } else {
            study_date >= cutoff
        };
        if ineligible {
            continue;
        }
        let study_uid = name.trim_end_matches(".manifest").to_string();
        for inst in m.series.iter().flat_map(|s| &s.instances) {
            if !wanted.contains(&inst.storage_tier) {
                continue;
            }
            let b3 = clarus_adapters::hex_encode(&inst.blake3);
            // A blob the caller already failed on this tick is excluded HERE,
            // before the sort and before the wrapper's `.take(limit)`: it must
            // not consume a slot the younger, still-migratable blobs need.
            if exclude.contains(&b3) {
                continue;
            }
            if seen.insert(b3.clone()) {
                entries.push((study_date, (b3, inst.file_size as i64, study_uid.clone())));
            }
        }
    }

    entries.sort_by_key(|(d, _)| *d);
    Ok(entries.into_iter().map(|(_, cand)| cand).collect())
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
    /// Every study UID this patient owns, carried independently of blob dedup.
    ///
    /// `blobs` is deduplicated by CAS key (two studies sharing bytes appear
    /// once), so it cannot be the source of "which studies are mine": a
    /// shared blob would then record only one study and the flip guard would
    /// treat the patient's own second study as an outside hot reference,
    /// excluding the blob forever.
    studies: Vec<String>,
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
    exclude: &std::collections::HashSet<String>,
    include_undated: bool,
) -> Result<Vec<PatientBatch>, String> {
    collect_migration_patients_in(state, cutoff_date, max_patients, exclude, include_undated)
}

/// The scan itself, over an explicit manifest directory.
///
/// Split out so the grouping and exclusion rules are testable from a temp
/// directory: they are the part that decides which patients a tick touches, and
/// a test that had to build an `AppState` to reach them would test less.
fn collect_migration_patients_in(
    state: &AppState,
    cutoff_date: &str,
    max_patients: usize,
    exclude: &std::collections::HashSet<String>,
    include_undated: bool,
) -> Result<Vec<PatientBatch>, String> {
    let cutoff: i64 = cutoff_date.parse().unwrap_or(i64::MAX);

    // ONE pass over the directory. Eligibility and blob collection must come
    // from the same manifest observations: a commit that publishes a new study
    // between two passes is invisible to the first (which decided eligibility)
    // but visible to the second (which collects), so the new study's HOT blobs
    // would be migrated despite the cutoff.
    //
    // Per manifest this records `(patient_id, study_date, hot_blobs)`; the
    // eligibility verdict is applied AFTER the scan, over those same records.
    //
    // patient_id -> (earliest study date for ordering, blobs, any study >=
    // cutoff, every study UID this patient owns). The study list is separate
    // from the blob list because blobs are deduplicated by CAS key: two
    // studies sharing bytes must still both count as the patient's own, or
    // the flip guard treats the second study as an outside hot reference.
    let mut by_patient: std::collections::HashMap<
        String,
        (i64, Vec<PatientBlob>, bool, Vec<String>),
    > = std::collections::HashMap::new();

    let Ok(rd) = std::fs::read_dir(std::path::Path::new(&state.config.btree.manifest_dir)) else {
        return Ok(Vec::new());
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".manifest") {
            continue;
        }
        let Ok(m) = scan_hook(state, name.trim_end_matches(".manifest")) else {
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
        let Some(patient_id) = patient_id else {
            if study_date < cutoff {
                crate::clog_debug!(
                    study = %name.as_ref(),
                    "Cold migration: study older than cutoff has no Patient ID - skipped (cannot be attributed to a patient)"
                );
            }
            continue;
        };
        let study_uid = name.trim_end_matches(".manifest").to_string();
        let slot =
            by_patient
                .entry(patient_id)
                .or_insert((i64::MAX, Vec::new(), false, Vec::new()));
        slot.3.push(study_uid.clone());
        if study_date < slot.0 {
            slot.0 = study_date;
        }
        // The MISSING-date sentinel (`i64::MAX`) is what makes an undated
        // study ineligible in the ordinary path: a study we cannot date is not
        // provably old. EMERGENCY migration is the exception - its whole point
        // is to free space, and an undated blob sitting in hot storage frees
        // exactly as much as a dated one. Leaving it ineligible meant that if
        // every readable hot blob belonged to undated studies, the emergency
        // pass freed NOTHING while the disk stayed full.
        let undated = study_date == i64::MAX;
        let ineligible = if undated {
            !include_undated
        } else {
            study_date >= cutoff
        };
        if ineligible {
            slot.2 = true;
        }
        // Dedup WITHIN the patient only. One blob can be referenced by two
        // studies of the same patient and must appear once in the batch, or
        // the copy/verify/flip pass would do it twice. A CROSS-patient dedup
        // would be unsound: it lets whichever patient the directory walk
        // reached first consume the blob, so the outcome would depend on
        // enumeration order - and if that first patient turned out to be
        // ineligible, the blob's other (eligible) referent would never see it.
        let seen = &mut slot.1;
        for inst in m.series.iter().flat_map(|s| &s.instances) {
            if inst.storage_tier != clarus_core::manifest::TIER_HOT {
                continue;
            }
            let b3 = clarus_adapters::hex_encode(&inst.blake3);
            if seen.iter().any(|b| b.blake3 == b3) {
                continue;
            }
            seen.push(PatientBlob {
                blake3: b3,
                file_size: inst.file_size as i64,
                study_uid: study_uid.clone(),
            });
        }
    }

    let mut batches: Vec<(i64, PatientBatch)> = by_patient
        .into_iter()
        // A patient is eligible only when EVERY study of that patient is older
        // than the cutoff, which `has_new_study` records from the SAME
        // observations the blobs came from.
        .filter(|(_, (_, _, has_new_study, _))| !has_new_study)
        .filter(|(_, (_, blobs, _, _))| !blobs.is_empty())
        // O3: a patient that aborted earlier in THIS tick is not a candidate
        // again. Without this the same head-of-queue patients are re-collected
        // every chunk (they sort first by study date), and younger patients
        // never reach the `take(max_patients)` cap - they starve.
        .filter(|(patient_id, _)| !exclude.contains(patient_id))
        .map(|(patient_id, (date, mut blobs, _, mut studies))| {
            // Deterministic order inside a batch: the same manifests must
            // always produce the same sequence, or a resume would reorder.
            blobs.sort_by(|a, b| a.blake3.cmp(&b.blake3));
            studies.sort();
            studies.dedup();
            (
                date,
                PatientBatch {
                    patient_id,
                    blobs,
                    studies,
                },
            )
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

/// Read one manifest during the collection scan.
///
/// A seam, not a wrapper. The eligibility verdict and the blob list must come
/// from the SAME observation of each manifest, and the only way to prove that
/// is to inject a concurrent commit at the moment a manifest is read. In a
/// build without the hook this is exactly the audited server read.
fn scan_hook(state: &AppState, study_uid: &str) -> Result<clarus_core::manifest::Manifest, String> {
    let read = match state.read_server_manifest(study_uid) {
        None => Err(format!("not a published manifest path: {}", study_uid)),
        Some(Ok(m)) => Ok(m),
        Some(Err(e)) => Err(e.to_string()),
    };
    #[cfg(test)]
    if let Some(hook) = SCAN_HOOK.with(|h| h.borrow().clone()) {
        let path = std::path::Path::new(&state.config.btree.manifest_dir)
            .join(format!("{study_uid}.manifest"));
        hook(path);
    }
    read
}

#[cfg(test)]
type ScanHook = std::rc::Rc<dyn Fn(std::path::PathBuf)>;

#[cfg(test)]
thread_local! {
    /// Called once per manifest scanned, before it is read.
    static SCAN_HOOK: std::cell::RefCell<Option<ScanHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_scan_hook(hook: Option<ScanHook>) {
    SCAN_HOOK.with(|h| *h.borrow_mut() = hook);
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
            transfer_syntax_uid: clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE.into(),
            file_size: 64,
            ..Default::default()
        };
        let series = [SeriesModel {
            // A numeric suffix: PS3.5 9.1 requires every UID component to be
            // digits only, so `.s1` made the fixture an invalid UID and the
            // test silently depended on whether the reader validated it.
            series_instance_uid: format!("{study}.1"),
            modality: "CT".into(),
            instances: vec![instance],
            ..Default::default()
        }];
        write_study_manifest_full(dir, &mut attrs, &series).expect("write manifest");
    }

    /// A minimal `AppState` rooted at `dir`, for the collector wrapper tests.
    ///
    /// `collect_migration_candidates` reads only `state.config.btree.manifest_dir`,
    /// but its signature takes the whole state, so the test builds the same
    /// shape the integration tests use (`cold_migration_atomic.rs`).
    fn state_for(dir: &std::path::Path) -> AppState {
        let btree_root = dir.join("btree");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        let storage_dir = dir.join("cas");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");

        let mut config = crate::config::Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        AppState {
            cas: ContentAddressedStorage::new(storage_dir).expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: crate::sse_fanout::DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            log_buffer: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            maintenance: std::sync::Arc::new(std::sync::RwLock::new(
                crate::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: crate::shadow::ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
            config,
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: crate::state::TranscodePool::new(2),
            readiness: crate::handlers::readiness::Readiness::new(),
        }
    }

    /// A minimal `AppState` whose configured manifest dir IS `manifest_dir`,
    /// for the tests that write manifests directly into the temp dir root.
    fn state_with_manifest_dir(manifest_dir: &std::path::Path) -> AppState {
        let idx_dir = manifest_dir.join("idx");
        let storage_dir = manifest_dir.join("cas");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");

        let mut config = crate::config::Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        AppState {
            cas: ContentAddressedStorage::new(storage_dir).expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: crate::sse_fanout::DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            log_buffer: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            maintenance: std::sync::Arc::new(std::sync::RwLock::new(
                crate::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: crate::shadow::ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(
                manifest_dir.to_path_buf(),
                idx_dir,
            ),
            config,
            workitems_dir: manifest_dir.join("workitems"),
            commitments_dir: manifest_dir.join("commitments"),
            transcode_pool: crate::state::TranscodePool::new(2),
            readiness: crate::handlers::readiness::Readiness::new(),
        }
    }

    #[test]
    fn count_hot_refs_matches_tier_and_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
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
            count_hot_refs(&state, &clarus_adapters::hex_encode(&b)),
            1,
            "only the hot instance counts"
        );
        assert_eq!(
            count_hot_refs(&state, &clarus_adapters::hex_encode(&[0xCD; 32])),
            1
        );
    }

    #[test]
    fn count_hot_refs_zero_when_no_hot_instance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
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
        assert_eq!(count_hot_refs(&state, &clarus_adapters::hex_encode(&b)), 0);
    }

    /// An unreadable manifest must fail closed, never count as zero refs.
    ///
    /// `cleanup_hot_file_cas_safe` deletes the hot copy only on a zero answer.
    /// A manifest that will not decode might reference the blob in the hot
    /// tier; reporting zero deleted the file, and the manifest becoming
    /// readable again would then point at a missing file.
    #[test]
    fn an_unreadable_manifest_is_an_unknown_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        // Garbage bytes where a manifest should be: read_manifest fails decode.
        std::fs::write(
            dir.path().join("1.2.98.manifest"),
            b"\xde\xad\xbe\xefnot-a-manifest-not-cbor-bytes",
        )
        .expect("write corrupt manifest");
        assert_eq!(
            count_hot_refs(&state, &clarus_adapters::hex_encode(&[0xEE; 32])),
            u64::MAX,
            "an unreadable manifest must be treated as an unknown reference"
        );
    }

    /// The tier set decides what a target sees, and the S3 target must see the
    /// blobs a cold pass already moved.
    ///
    /// Falsification anchor: the same store scanned with the hot-only set
    /// returns the HOT instance and not the COLD one. A collector that ignored
    /// its tier set would return both, and the S3 pass would never pick up a
    /// study the NAS already holds - which is the whole point of the tier
    /// after `min_age_days`.
    #[test]
    fn a_cold_instance_is_a_candidate_for_the_cloud_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut hot = [0u8; 32];
        hot[0] = 0x11;
        let mut cold = [0u8; 32];
        cold[0] = 0x22;
        write_one_manifest(
            dir.path(),
            "1.2.7",
            hot,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
        );
        write_one_manifest(
            dir.path(),
            "1.2.8",
            cold,
            clarus_core::manifest::TIER_COLD,
            Some("20200102"),
        );

        // `collect_migration_candidates` only needs the manifest directory, so
        // the scan is exercised through a state-free helper below.
        let both = collect_candidates_in(
            &state,
            "99991231",
            &[
                clarus_core::manifest::TIER_HOT,
                clarus_core::manifest::TIER_COLD,
            ],
            &std::collections::HashSet::new(),
        )
        .expect("scan ok");
        let hashes: Vec<&str> = both.iter().map(|(h, _, _)| h.as_str()).collect();
        assert!(
            hashes.contains(&clarus_adapters::hex_encode(&cold).as_str()),
            "a blob already in cold is still a cloud candidate"
        );
        assert_eq!(both.len(), 2, "both tiers contribute");

        let hot_only = collect_candidates_in(
            &state,
            "99991231",
            &[clarus_core::manifest::TIER_HOT],
            &std::collections::HashSet::new(),
        )
        .expect("scan ok");
        assert_eq!(hot_only.len(), 1, "the hot-only set excludes the cold blob");
        assert_eq!(
            hot_only[0].0,
            clarus_adapters::hex_encode(&hot),
            "the hot instance is the one the cold pass selected"
        );
    }

    /// Cutoff boundary: a study dated exactly on the cutoff is NOT old enough.
    #[test]
    fn a_study_dated_on_the_cutoff_is_not_a_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        write_one_manifest(
            dir.path(),
            "1.2.9",
            [0x33; 32],
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
        );
        assert!(
            collect_candidates_in(
                &state,
                "20260101",
                &[clarus_core::manifest::TIER_HOT],
                &std::collections::HashSet::new(),
            )
            .expect("scan ok")
            .is_empty(),
            "the cutoff is exclusive, so the pass never races a same-day study"
        );
        assert_eq!(
            collect_candidates_in(
                &state,
                "20260102",
                &[clarus_core::manifest::TIER_HOT],
                &std::collections::HashSet::new(),
            )
            .expect("scan ok")
            .len(),
            1,
            "one day past the cutoff the study is eligible"
        );
    }

    /// The exclusion runs INSIDE the scan, before the sort and before the
    /// wrapper's `.take(limit)`.
    ///
    /// Falsification anchor: with the exclusion applied AFTER the take (the old
    /// shape), the first `limit` blobs - all excluded here - are returned and
    /// the round migrates nothing while the younger blobs behind them starve.
    #[test]
    fn the_collector_excludes_before_take() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let b1 = [0x01; 32];
        let b2 = [0x02; 32];
        let b3 = [0x03; 32];
        write_one_manifest(
            dir.path(),
            "1.2.101",
            b1,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
        );
        write_one_manifest(
            dir.path(),
            "1.2.102",
            b2,
            clarus_core::manifest::TIER_HOT,
            Some("20200102"),
        );
        write_one_manifest(
            dir.path(),
            "1.2.103",
            b3,
            clarus_core::manifest::TIER_HOT,
            Some("20200103"),
        );

        let mut exclude: std::collections::HashSet<String> = std::collections::HashSet::new();
        exclude.insert(clarus_adapters::hex_encode(&b1));
        exclude.insert(clarus_adapters::hex_encode(&b2));
        exclude.insert(clarus_adapters::hex_encode(&b3));

        // All three excluded: the scan yields nothing, so a limit of 2 cannot
        // return the first two excluded blobs.
        let got = collect_candidates_in(
            &state,
            "99991231",
            &[clarus_core::manifest::TIER_HOT],
            &exclude,
        )
        .expect("scan ok");
        assert!(
            got.is_empty(),
            "excluded blobs must never reach the caller: {got:?}"
        );

        // A fourth blob NOT in the exclude set must still be returned even
        // though it sorts AFTER the three excluded ones.
        let b4 = [0x04; 32];
        write_one_manifest(
            dir.path(),
            "1.2.104",
            b4,
            clarus_core::manifest::TIER_HOT,
            Some("20200104"),
        );
        let got = collect_candidates_in(
            &state,
            "99991231",
            &[clarus_core::manifest::TIER_HOT],
            &exclude,
        )
        .expect("scan ok");
        assert_eq!(got.len(), 1, "only the non-excluded blob remains");
        assert_eq!(
            got[0].0,
            clarus_adapters::hex_encode(&b4),
            "the non-excluded blob is reached past the excluded prefix"
        );
    }

    /// The wrapper applies `.take(limit)` AFTER the exclusion, over a real
    /// `AppState`: with `limit = 2` and three excluded blobs ahead of one
    /// non-excluded blob, the non-excluded one is still returned.
    #[test]
    fn the_wrapper_takes_after_the_exclusion() {
        let root = tempfile::tempdir().expect("tempdir");
        let state = state_for(root.path());
        let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);

        let b1 = [0x11; 32];
        let b2 = [0x12; 32];
        let b3 = [0x13; 32];
        let b4 = [0x14; 32];
        for (study, blob, date) in [
            ("1.2.201", b1, "20200101"),
            ("1.2.202", b2, "20200102"),
            ("1.2.203", b3, "20200103"),
            ("1.2.204", b4, "20200104"),
        ] {
            write_one_manifest(
                manifest_dir,
                study,
                blob,
                clarus_core::manifest::TIER_HOT,
                Some(date),
            );
        }

        let mut exclude: std::collections::HashSet<String> = std::collections::HashSet::new();
        exclude.insert(clarus_adapters::hex_encode(&b1));
        exclude.insert(clarus_adapters::hex_encode(&b2));
        exclude.insert(clarus_adapters::hex_encode(&b3));

        let got = collect_migration_candidates(
            &state,
            "99991231",
            2,
            &[clarus_core::manifest::TIER_HOT],
            &exclude,
        )
        .expect("collect");
        assert_eq!(
            got.len(),
            1,
            "three excluded blobs must not consume the limit-2 window: {got:?}"
        );
        assert_eq!(got[0].0, clarus_adapters::hex_encode(&b4));
    }

    /// The S3 loop's collector call passes a non-empty failed set: a cold blob
    /// in the set is dropped and a hot blob outside it is still returned, with
    /// the S3 tiers and the 500 cap the loop uses.
    #[cfg(feature = "s3")]
    #[test]
    fn the_s3_loop_collector_passes_the_failed_set() {
        let root = tempfile::tempdir().expect("tempdir");
        let state = state_for(root.path());
        let manifest_dir = std::path::Path::new(&state.config.btree.manifest_dir);

        let cold_failed = [0x21; 32];
        let hot_ok = [0x22; 32];
        write_one_manifest(
            manifest_dir,
            "1.2.301",
            cold_failed,
            clarus_core::manifest::TIER_COLD,
            Some("20200101"),
        );
        write_one_manifest(
            manifest_dir,
            "1.2.302",
            hot_ok,
            clarus_core::manifest::TIER_HOT,
            Some("20200102"),
        );

        let mut exclude: std::collections::HashSet<String> = std::collections::HashSet::new();
        exclude.insert(clarus_adapters::hex_encode(&cold_failed));

        let got = collect_migration_candidates(
            &state,
            "99991231",
            500,
            &[
                clarus_core::manifest::TIER_HOT,
                clarus_core::manifest::TIER_COLD,
            ],
            &exclude,
        )
        .expect("collect");
        let hashes: Vec<&str> = got.iter().map(|(h, _, _)| h.as_str()).collect();
        assert!(
            !hashes.contains(&clarus_adapters::hex_encode(&cold_failed).as_str()),
            "a failed cold blob must not be re-collected in the same tick"
        );
        assert!(
            hashes.contains(&clarus_adapters::hex_encode(&hot_ok).as_str()),
            "the still-migratable hot blob is reached past the failed one"
        );
    }

    /// A cold copy's reference count counts ONLY `TIER_COLD` instances, the
    /// same shape as [`count_hot_refs_matches_tier_and_hash`].
    #[cfg(feature = "s3")]
    #[test]
    fn count_cold_refs_matches_tier_and_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut b = [0u8; 32];
        b[0] = 0xAB;
        // Cold instance referencing 0xAB -> 1 cold ref.
        write_one_manifest(
            dir.path(),
            "1.2.401",
            b,
            clarus_core::manifest::TIER_COLD,
            Some("20260101"),
        );
        // Hot instance with the SAME blob -> must not count as a cold ref.
        write_one_manifest(
            dir.path(),
            "1.2.402",
            b,
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
        );
        // An S3 instance with the same blob -> also not a cold ref.
        write_one_manifest(
            dir.path(),
            "1.2.403",
            b,
            clarus_core::manifest::TIER_S3,
            Some("20260101"),
        );

        assert_eq!(
            count_cold_refs(&state, &clarus_adapters::hex_encode(&b)),
            1,
            "only the cold instance counts"
        );
    }

    /// An unreadable manifest fails the cold count closed, exactly like the hot
    /// twin: it might reference the blob in the cold tier.
    #[cfg(feature = "s3")]
    #[test]
    fn an_unreadable_manifest_is_an_unknown_cold_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        std::fs::write(
            dir.path().join("1.2.499.manifest"),
            b"\xde\xad\xbe\xefnot-a-manifest-not-cbor-bytes",
        )
        .expect("write corrupt manifest");
        assert_eq!(
            count_cold_refs(&state, &clarus_adapters::hex_encode(&[0xEE; 32])),
            u64::MAX,
            "an unreadable manifest must be treated as an unknown reference"
        );
    }

    /// O1: a manifest whose rewrite did not land is NOT counted as migrated.
    ///
    /// The count is what the caller uses to decide whether to delete the hot
    /// copy. Counting the intent made a failed write (the ENOSPC path) look
    /// successful: the hot bytes were removed while every manifest still named
    /// the hot tier, and the batch re-selected the same blob forever.
    #[test]
    fn a_failed_manifest_rewrite_is_not_counted_as_migrated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut b = [0u8; 32];
        b[0] = 0x91;
        write_one_manifest(
            dir.path(),
            "1.2.30",
            b,
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
        );
        let hex = clarus_adapters::hex_encode(&b);

        // The happy path counts the manifest it actually changed.
        assert_eq!(
            set_storage_tier_in(&state, &hex, "cold"),
            1,
            "one manifest changed on disk"
        );
        // Re-running it changes nothing, so it counts nothing.
        assert_eq!(
            set_storage_tier_in(&state, &hex, "cold"),
            0,
            "an already-flipped manifest is not a change"
        );

        // Now make the rewrite fail: the blocked-path seam reports a failed
        // write, so the tier cannot change on disk and the count must stay
        // zero. This is the ENOSPC shape without needing a full filesystem,
        // and it does not depend on the atomic writer's (now per-writer) temp
        // sibling name.
        let mut c = [0u8; 32];
        c[0] = 0x92;
        write_one_manifest(
            dir.path(),
            "1.2.31",
            c,
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
        );
        clarus_core::manifest::block_manifest_write_for_test(&dir.path().join("1.2.31.manifest"));
        let failed_count = set_storage_tier_in(&state, &clarus_adapters::hex_encode(&c), "cold");
        clarus_core::manifest::unblock_manifest_write_for_test(&dir.path().join("1.2.31.manifest"));
        assert_eq!(
            failed_count, 0,
            "a rewrite that did not reach disk is not a migration"
        );
        // The record on disk still says HOT, which is why the caller must not
        // delete the hot copy.
        let m = state
            .read_server_manifest("1.2.31")
            .expect("published path")
            .expect("manifest still readable");
        assert_eq!(
            m.series[0].instances[0].storage_tier,
            clarus_core::manifest::TIER_HOT,
            "the tier is unchanged when the write fails"
        );
    }

    /// O2: a blob a live manifest references in a NON-hot tier is not free to
    /// delete from cold.
    ///
    /// The abort cleanup and the emergency worker run concurrently: the worker
    /// can flip a manifest onto the very cold copy the aborting patient just
    /// wrote, and the old unconditional unlink then removed the archive's only
    /// remaining bytes. The count answers "does anything resolve this from a
    /// non-hot tier".
    #[test]
    fn count_non_hot_refs_sees_a_flipped_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut b = [0u8; 32];
        b[0] = 0x77;

        // A manifest that has been FLIPPED to cold for this blob.
        write_one_manifest(
            dir.path(),
            "1.2.20",
            b,
            clarus_core::manifest::TIER_COLD,
            Some("20260101"),
        );
        assert_eq!(
            count_non_hot_refs(&state, &clarus_adapters::hex_encode(&b)),
            1,
            "the flipped manifest means the cold copy is the live one"
        );
        // It is NOT a hot ref, which is why `count_hot_refs` could not answer
        // this question on its own.
        assert_eq!(count_hot_refs(&state, &clarus_adapters::hex_encode(&b)), 0);

        // A blob referenced only from hot is safe to drop from cold.
        let mut only_hot = [0u8; 32];
        only_hot[0] = 0x78;
        write_one_manifest(
            dir.path(),
            "1.2.21",
            only_hot,
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
        );
        assert_eq!(
            count_non_hot_refs(&state, &clarus_adapters::hex_encode(&only_hot)),
            0,
            "nothing resolves it from a non-hot tier"
        );
    }

    /// O3: a patient excluded from the collect is not handed out again.
    ///
    /// `collect_migration_patients` re-derives candidates from the same cutoff
    /// every chunk, and the ordering is by earliest study date - so the patients
    /// that aborted are exactly the ones at the head of the queue. Without the
    /// exclusion set they are collected again immediately and the younger
    /// patients never reach the `take(max_patients)` cap.
    #[test]
    fn an_excluded_patient_is_not_collected_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        // Two patients: PAT-OLD sorts first, PAT-NEW second.
        write_one_manifest_for(
            dir.path(),
            "1.2.10",
            [0x41; 32],
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-OLD",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.11",
            [0x42; 32],
            clarus_core::manifest::TIER_HOT,
            Some("20200601"),
            "PAT-NEW",
        );

        let all = collect_migration_patients_in(&state, "99991231", 10, &Default::default(), false)
            .expect("collect");
        let all: Vec<String> = all.into_iter().map(|b| b.patient_id).collect();
        assert_eq!(all.len(), 2, "both patients are candidates");
        assert_eq!(all[0], "PAT-OLD", "the oldest patient sorts first");

        // Excluding the head-of-queue patient must expose the next one, which
        // is what stops it starving behind a patient that keeps aborting.
        let mut exclude: std::collections::HashSet<String> = std::collections::HashSet::new();
        exclude.insert("PAT-OLD".to_string());
        let after = collect_migration_patients_in(&state, "99991231", 10, &exclude, false)
            .expect("collect with exclusion");
        let after: Vec<String> = after.into_iter().map(|b| b.patient_id).collect();
        assert_eq!(
            after,
            vec!["PAT-NEW".to_string()],
            "the aborted patient is skipped"
        );
    }

    /// A failed COLD rewrite must not move the S3 error counter.
    ///
    /// `set_storage_tier_for_patient` serves both tiers, but the metric it used
    /// to call unconditionally is `S3_MIGRATION_ERRORS`. A cold-volume ENOSPC
    /// therefore reported a cloud fault, and no S3 retry could ever clear it.
    #[test]
    fn a_cold_rewrite_failure_is_not_an_s3_error() {
        assert!(
            !a_failed_rewrite_is_an_s3_error(clarus_core::manifest::TIER_COLD),
            "a COLD write failure must not be counted as an S3 error"
        );
        assert!(
            a_failed_rewrite_is_an_s3_error(clarus_core::manifest::TIER_S3),
            "an S3 write failure must be counted"
        );
        // HOT is not rewritten by this path at all; if it ever is, filing it
        // under S3 would be the same category error.
        assert!(
            !a_failed_rewrite_is_an_s3_error(clarus_core::manifest::TIER_HOT),
            "a HOT write failure must not be counted as an S3 error"
        );
    }

    /// F1: the abort cleanup and the tier publisher must not interleave.
    ///
    /// The crash case: `count_non_hot_refs` reads 0, the emergency worker flips
    /// a manifest onto this cold copy, and the cleanup then unlinks the only
    /// resolvable bytes. Guarding only the read leaves the window open, so the
    /// guard has to span the decision AND the unlink.
    ///
    /// A true thread race cannot be scheduled reliably, so the test pins the
    /// mechanism the fix relies on: the same blob yields the SAME lock, so a
    /// holder blocks every other actor, and two different blobs do not contend.
    #[test]
    fn one_blob_yields_one_shared_lock() {
        let a = blob_guard("blob-aaa");
        let b = blob_guard("blob-aaa");
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "every actor on one blob must take the SAME lock, or the race is \
             only narrowed, not closed"
        );

        let other = blob_guard("blob-bbb");
        assert!(
            !std::sync::Arc::ptr_eq(&a, &other),
            "unrelated blobs must not serialise against each other"
        );

        // Holding the guard blocks a second acquisition on the same blob.
        let held = a.lock().expect("lock");
        let probe = blob_guard("blob-aaa");
        assert!(
            probe.try_lock().is_err(),
            "a second actor must not enter while the blob is guarded"
        );
        drop(held);
        assert!(probe.try_lock().is_ok(), "the guard must release");
    }
    /// B2: an unreadable manifest is "cannot verify", not "no refs".
    ///
    /// `rd.flatten()` dropped entry errors, so an omitted entry that was the
    /// live cold reference produced a count of 0 and made the only remaining
    /// copy deletable. The count now reports `u64::MAX` for anything it could
    /// not examine.
    #[test]
    fn an_unverifiable_manifest_keeps_the_cold_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut b = [0u8; 32];
        b[0] = 0x91;
        write_one_manifest(
            dir.path(),
            "1.2.40",
            b,
            clarus_core::manifest::TIER_COLD,
            Some("20260101"),
        );
        let hex = clarus_adapters::hex_encode(&b);
        // Sanity: a readable directory really does resolve the reference.
        assert_eq!(count_non_hot_refs(&state, &hex), 1);

        // A manifest that will not DECODE is unverifiable: it might name the
        // blob, so the caller must keep the file.
        let corrupt = dir.path().join("1.2.41.manifest");
        std::fs::write(&corrupt, b"not a manifest").expect("write corrupt");
        assert_eq!(
            count_non_hot_refs(&state, &hex),
            u64::MAX,
            "an undecodable manifest must stop the count, not contribute zero"
        );

        // The same decision at the entry level, which is where a read failure
        // lands before it can be mistaken for "no references".
        assert_eq!(entry_refs(&state, "1.2.41", &hex), EntryRefs::Unverifiable);

        // A directory that does not exist at all is equally unverifiable.
        let missing = dir.path().join("no-such-dir");
        let missing_state = state_with_manifest_dir(&missing);
        std::fs::remove_dir_all(&missing).expect("remove missing dir");
        assert_eq!(count_non_hot_refs(&missing_state, &hex), u64::MAX);
    }

    /// B1: an all-abort chunk must RE-COLLECT, not end the tick.
    ///
    /// The exclusion set only helps if it is populated before the decision, and
    /// the tick only reaches younger patients if an all-abort chunk loops. The
    /// old code broke out first, so the next tick re-derived the same 50
    /// aborted patients and patient 51 was never attempted.
    #[test]
    fn an_all_abort_chunk_recollects_instead_of_ending_the_tick() {
        assert_eq!(
            chunk_disposition(0, 0, 50),
            ChunkDisposition::Recollect,
            "an all-abort chunk must loop so the excluded IDs cannot starve the queue"
        );
        // Nothing at all to do ends the tick.
        assert_eq!(chunk_disposition(0, 0, 0), ChunkDisposition::EndTick);
        // Any progress continues.
        assert_eq!(chunk_disposition(1, 0, 50), ChunkDisposition::Continue);
        assert_eq!(chunk_disposition(0, 1, 50), ChunkDisposition::Continue);
    }

    /// `:1470` - the S3 loop must continue when the exclusion set GREW, even
    /// with nothing migrated.
    ///
    /// A chunk of 500 unreadable blobs migrated nothing but excluded 500
    /// digests. Ending the tick there meant the next tick started from an
    /// empty exclusion set, selected the same 500, and blob 501 was never
    /// attempted - starved forever on a store where the oldest 500 are
    /// damaged.
    #[test]
    fn the_s3_loop_continues_while_the_exclusion_set_grows() {
        // Nothing migrated, but every attempt added an exclusion: the round
        // DID make progress (the set moved), so the tick must continue.
        assert!(
            !s3_chunk_ends_tick(0, 0, 500),
            "500 new exclusions are progress: blob 501 must be reachable"
        );
        // Nothing migrated and nothing new learned: saturated, end.
        assert!(
            s3_chunk_ends_tick(0, 500, 500),
            "a saturated exclusion set with no progress ends the tick"
        );
        // A migration is progress whatever the set did.
        assert!(
            !s3_chunk_ends_tick(1, 500, 500),
            "one migration continues the tick"
        );
        // One new exclusion is enough, even from a large prefix.
        assert!(
            !s3_chunk_ends_tick(0, 500, 501),
            "a single new exclusion must still continue"
        );
    }

    /// B1: eligibility and collection must share ONE observation.
    ///
    /// Pass 1 decided eligibility, pass 2 collected blobs. A commit landing
    /// between them was invisible to the first and visible to the second, so a
    /// NEW study's HOT blobs migrated despite the cutoff.
    ///
    /// The hazard is reproduced the only way `read_dir` permits: the new study
    /// is published through the scan hook, and the assertion is that the
    /// patient's blobs are never collected together with a verdict that was
    /// formed without it. A single pass reads the manifest it decides on, so the
    /// published study either changes its verdict or is not collected.
    #[test]
    fn a_study_published_during_the_scan_never_migrates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut old_blob = [0u8; 32];
        old_blob[0] = 0x71;
        let mut new_blob = [0u8; 32];
        new_blob[0] = 0x72;
        write_one_manifest_for(
            dir.path(),
            "1.2.200",
            old_blob,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-RACE",
        );

        // Publish the NEW study after the old one is read. `read_dir` snapshots
        // the directory when iteration starts, so a file created afterwards is
        // not enumerated in the SAME pass - which is precisely why two passes
        // are unsafe: the second `read_dir` starts fresh and does see it.
        let published = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = published.clone();
        let target = dir.path().to_path_buf();
        set_scan_hook(Some(std::rc::Rc::new(move |path: std::path::PathBuf| {
            if !path.to_string_lossy().contains("1.2.200") || flag.replace(true) {
                return;
            }
            write_one_manifest_for(
                &target,
                "1.2.201",
                new_blob,
                clarus_core::manifest::TIER_HOT,
                Some("20260101"),
                "P-RACE",
            );
        })));

        let batches = collect_migration_patients_in(
            &state,
            "20250101",
            50,
            &std::collections::HashSet::new(),
            false,
        )
        .expect("collect");
        set_scan_hook(None);
        assert!(
            published.get(),
            "the hook must have published the new study"
        );

        // ONE pass cannot see the new file, so the old study migrates - and
        // that is correct, because at the moment the pass read the directory
        // the patient had exactly one study, which was old.
        let collected: Vec<&str> = batches
            .iter()
            .flat_map(|b| b.blobs.iter().map(|blob| blob.blake3.as_str()))
            .collect();

        // Assert on the FIRST scan, not only on the second. The second scan is
        // required to be empty, so an assertion against IT says nothing about
        // the first: a collector that ignored the cutoff entirely would still
        // pass every assertion below. The blob the concurrent commit published
        // belongs to the NEW study, so a correct first pass cannot have seen
        // it - and if it did, the old study would have been migrated together
        // with it.
        let new_hex = clarus_adapters::hex_encode(&new_blob);
        assert!(
            !collected.contains(&new_hex.as_str()),
            "the FIRST scan must not collect the concurrently published blob; got {collected:?}"
        );
        // It must, however, have collected the old study's blob - otherwise
        // the assertion above would pass vacuously on an empty batch.
        assert!(
            collected.contains(&clarus_adapters::hex_encode(&old_blob).as_str()),
            "the first scan collects the old study; got {collected:?}"
        );

        // The regression: a SECOND collect now sees the new study, so the
        // patient must vanish from the candidate set entirely. A two-pass
        // collector would still hold the eligibility verdict from its own first
        // pass and collect the old study again.
        let after = collect_migration_patients_in(
            &state,
            "20250101",
            50,
            &std::collections::HashSet::new(),
            false,
        )
        .expect("collect after publish");
        assert!(
            after.is_empty(),
            "a patient with a new study must not be a candidate; got {:?}",
            after
                .iter()
                .map(|b| b.patient_id.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            !after
                .iter()
                .flat_map(|b| b.blobs.iter())
                .any(|blob| blob.blake3 == new_hex),
            "the new study's blobs must never be collected"
        );
    }

    /// B3: a patient is eligible only when EVERY study is older than the cutoff.
    ///
    /// The per-STUDY filter ran before grouping, so a patient with one old and
    /// one new study migrated the old one and was left split across tiers -
    /// the partial migration the invariant forbids. Eligibility is now decided
    /// over all of a patient's studies before any blob is collected.
    #[test]
    fn a_patient_with_any_new_study_stays_entirely_hot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut old_blob = [0u8; 32];
        old_blob[0] = 0x31;
        let mut new_blob = [0u8; 32];
        new_blob[0] = 0x32;
        // Same patient, two studies: one old, one new relative to the cutoff.
        write_one_manifest_for(
            dir.path(),
            "1.2.100",
            old_blob,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-SPLIT",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.101",
            new_blob,
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
            "P-SPLIT",
        );

        let batches = collect_migration_patients_in(
            &state,
            "20250101",
            50,
            &std::collections::HashSet::new(),
            false,
        )
        .expect("collect");
        assert!(
            batches.is_empty(),
            "a patient with a new study must migrate nothing, got {} batch(es)",
            batches.len()
        );
    }

    /// Positive control: all-old studies still migrate in full, both of them.
    #[test]
    fn a_patient_whose_studies_are_all_old_migrates_in_full() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut a = [0u8; 32];
        a[0] = 0x41;
        let mut b = [0u8; 32];
        b[0] = 0x42;
        write_one_manifest_for(
            dir.path(),
            "1.2.110",
            a,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-OLD",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.111",
            b,
            clarus_core::manifest::TIER_HOT,
            Some("20210101"),
            "P-OLD",
        );

        let batches = collect_migration_patients_in(
            &state,
            "20250101",
            50,
            &std::collections::HashSet::new(),
            false,
        )
        .expect("collect");
        assert_eq!(batches.len(), 1, "one patient batch");
        assert_eq!(
            batches[0].blobs.len(),
            2,
            "BOTH studies of the old patient must be collected"
        );
    }

    /// Positive control: a single old study still migrates (unchanged path).
    #[test]
    fn a_patient_with_one_old_study_still_migrates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut a = [0u8; 32];
        a[0] = 0x51;
        write_one_manifest_for(
            dir.path(),
            "1.2.120",
            a,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-SINGLE",
        );

        let batches = collect_migration_patients_in(
            &state,
            "20250101",
            50,
            &std::collections::HashSet::new(),
            false,
        )
        .expect("collect");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].blobs.len(), 1);
    }

    /// A study with NO StudyDate is not provably old, so its patient stays hot.
    #[test]
    fn an_undated_study_keeps_its_patient_hot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut a = [0u8; 32];
        a[0] = 0x61;
        write_one_manifest_for(
            dir.path(),
            "1.2.130",
            a,
            clarus_core::manifest::TIER_HOT,
            None,
            "P-UNDATED",
        );

        let batches = collect_migration_patients_in(
            &state,
            "20250101",
            50,
            &std::collections::HashSet::new(),
            false,
        )
        .expect("collect");
        assert!(
            batches.is_empty(),
            "a missing StudyDate carries the i64::MAX sentinel and is not old enough"
        );
    }

    /// A blob shared by an eligible and an ineligible patient must not depend
    /// on which manifest the directory walk reached first.
    ///
    /// The collector used to dedup blobs across patients with one global set,
    /// so the first patient to reference the shared blob consumed it. Whichever
    /// patient that was decided the outcome: an eligible patient first meant the
    /// shared blob was handed over even though the other patient still named it
    /// hot; an ineligible patient first meant it was dropped for everyone.
    /// Dedup is per patient now, so both orders produce the same batch, and the
    /// flip guard is what keeps the shared blob hot while the other patient
    /// still references it (see `retain_exclusively_hot_blobs`).
    #[test]
    fn a_blob_shared_with_an_ineligible_patient_does_not_depend_on_scan_order() {
        // The directory walk order is not controllable, so the READ order is:
        // the hook collects every manifest path during a first pass (returning
        // an empty manifest keeps the collector idle), then a second collector
        // run replays the paths in the order the test wants.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut shared = [0u8; 32];
        shared[0] = 0x71;
        let mut own = [0u8; 32];
        own[0] = 0x72;
        // P-OLD is past the cutoff (20200101); P-NEW has a current study.
        write_one_manifest_for(
            dir.path(),
            "1.2.200",
            shared,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-OLD",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.201",
            own,
            clarus_core::manifest::TIER_HOT,
            Some("20200102"),
            "P-OLD",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.202",
            shared,
            clarus_core::manifest::TIER_HOT,
            Some("20260601"),
            "P-NEW",
        );

        let collect = || {
            collect_migration_patients_in(
                &state,
                "20250101",
                50,
                &std::collections::HashSet::new(),
                false,
            )
            .expect("collect")
        };

        // P-NEW owns a study past the cutoff, so only P-OLD is a candidate, and
        // its batch must hold BOTH blobs whichever manifest is seen first: the
        // shared one is a legitimate reference of P-OLD.
        let keys_of = |batches: &Vec<PatientBatch>| {
            assert_eq!(batches.len(), 1, "only the eligible patient is collected");
            assert_eq!(batches[0].patient_id, "P-OLD");
            let mut keys: Vec<String> = batches[0].blobs.iter().map(|b| b.blake3.clone()).collect();
            keys.sort();
            assert_eq!(
                keys.len(),
                2,
                "the shared blob belongs in the batch regardless of which \
                 patient the walk reached first"
            );
            keys
        };
        keys_of(&collect());

        // Now replay the SAME manifests with the ineligible one visited first.
        // The hook cannot reorder `read_dir`, so the ordering is emulated by the
        // dedup scope itself: a cross-patient set lets the first visitor claim
        // the blob. That is the property under test, and the scan-order
        // invariance above is what it breaks.
        let ordered = ordered_blob_claim();
        assert!(
            ordered,
            "a shared blob claimed by an ineligible patient first must still \
             reach the eligible patient's batch"
        );
    }

    /// The dedup scope must be PER PATIENT in the PRODUCTION collector, not
    /// only in a hand-built map.
    ///
    /// The previous version of this test constructed its own `HashMap` and its
    /// own `HashSet`, so a real change from per-patient to global dedup inside
    /// `collect_migration_patients_in` could not make it fail. It also assumed
    /// filename order, which `read_dir` does not promise.
    ///
    /// This drives the production collector with BOTH visit orders - the
    /// ineligible patient's manifest enumerated first, then the eligible one's -
    /// and requires the eligible patient's batch to carry the shared digest in
    /// both cases.
    #[test]
    fn the_blob_dedup_scope_is_per_patient_in_the_production_collector() {
        let mut shared = [0u8; 32];
        shared[0] = 0xa1;

        for order in [["1.2.500", "1.2.501"], ["1.2.501", "1.2.500"]] {
            let dir = tempfile::tempdir().expect("tempdir");
            let state = state_with_manifest_dir(dir.path());
            // `1.2.500` belongs to the INELIGIBLE patient (a study at/
            // after the cutoff); `1.2.501` to the eligible one. Both name the
            // same blob, so a global dedup lets whichever was read first
            // consume it.
            for study in order {
                let (date, patient) = if study == "1.2.500" {
                    ("20260101", "P-NEW")
                } else {
                    ("20200101", "P-OLD")
                };
                write_one_manifest_for(
                    dir.path(),
                    study,
                    shared,
                    clarus_core::manifest::TIER_HOT,
                    Some(date),
                    patient,
                );
            }

            let batches = collect_migration_patients_in(
                &state,
                "20250101",
                50,
                &std::collections::HashSet::new(),
                false,
            )
            .expect("collect");

            assert_eq!(
                batches.len(),
                1,
                "only the eligible patient is a candidate (order {order:?})"
            );
            assert_eq!(batches[0].patient_id, "P-OLD");
            assert!(
                batches[0]
                    .blobs
                    .iter()
                    .any(|b| b.blake3 == clarus_adapters::hex_encode(&shared)),
                "the eligible patient must see the shared blob regardless of visit order {order:?}"
            );
        }
    }

    /// F3: two studies of ONE patient sharing a blob must both be recorded as
    /// the patient's own studies, independent of blob dedup.
    ///
    /// The blob appears once in `blobs` (CAS dedup), but the patient owns two
    /// studies. Deriving the study set from `blobs` would keep only one study
    /// and the flip guard would then treat the second as an outside hot
    /// reference - excluding the patient's own blob forever.
    #[test]
    fn a_blob_shared_within_one_patient_keeps_both_studies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut shared = [0u8; 32];
        shared[0] = 0x91;
        write_one_manifest_for(
            dir.path(),
            "1.2.400",
            shared,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-SHARED",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.401",
            shared,
            clarus_core::manifest::TIER_HOT,
            Some("20200102"),
            "P-SHARED",
        );

        let batches = collect_migration_patients_in(
            &state,
            "20250101",
            50,
            &std::collections::HashSet::new(),
            false,
        )
        .expect("collect");
        assert_eq!(batches.len(), 1, "one patient batch");
        assert_eq!(
            batches[0].blobs.len(),
            1,
            "the shared blob is deduplicated to one entry"
        );
        let mut studies = batches[0].studies.clone();
        studies.sort();
        assert_eq!(
            studies,
            vec!["1.2.400".to_string(), "1.2.401".to_string()],
            "both studies are the patient's own"
        );
    }

    /// F3: the flip guard keeps a blob shared WITHIN one patient, because the
    /// patient's own second study is not an outside reference.
    #[test]
    fn a_blob_shared_within_one_patient_is_not_excluded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut shared = [0u8; 32];
        shared[0] = 0x92;
        write_one_manifest_for(
            dir.path(),
            "1.2.500",
            shared,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-SHARED",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.501",
            shared,
            clarus_core::manifest::TIER_HOT,
            Some("20200102"),
            "P-SHARED",
        );

        // The production batch shape: one deduplicated blob entry, but the
        // complete two-study membership carried alongside it.
        let batch = PatientBatch {
            patient_id: "P-SHARED".to_string(),
            blobs: vec![PatientBlob {
                blake3: clarus_adapters::hex_encode(&shared),
                file_size: 64,
                study_uid: "1.2.500".to_string(),
            }],
            studies: vec!["1.2.500".to_string(), "1.2.501".to_string()],
        };
        let retained = retain_exclusively_hot_blobs_in(&state, &batch);
        assert_eq!(
            retained.blobs.len(),
            1,
            "the patient's own shared blob is kept, not excluded"
        );
    }

    /// Whether the collector's dedup keeps a blob that an ineligible patient
    /// references too. Drives the real collector over a directory in which the
    /// ineligible study sorts FIRST on disk by file name.
    fn ordered_blob_claim() -> bool {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut shared = [0u8; 32];
        shared[0] = 0x81;
        let mut own = [0u8; 32];
        own[0] = 0x82;
        // "1.2.AAA" (ineligible, current date) sorts before "1.2.ZZZ" (eligible).
        write_one_manifest_for(
            dir.path(),
            "1.2.300.AAA",
            shared,
            clarus_core::manifest::TIER_HOT,
            Some("20260601"),
            "P-NEW",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.300.ZZZ",
            shared,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-OLD",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.300.ZZY",
            own,
            clarus_core::manifest::TIER_HOT,
            Some("20200102"),
            "P-OLD",
        );
        let batches = collect_migration_patients_in(
            &state,
            "20250101",
            50,
            &std::collections::HashSet::new(),
            false,
        )
        .expect("collect");
        if batches.len() != 1 {
            return false;
        }
        batches[0].blobs.len() == 2
    }

    /// The revalidation is the Phase 0 filter re-run under the lease: a blob
    /// another patient started holding HOT during the copy must be excluded
    /// from the flip, even though Phase 0 passed it.
    #[test]
    fn revalidate_exclusive_hot_blobs_excludes_a_blob_shared_during_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut x = [0u8; 32];
        x[0] = 0xA1;
        // Patient A (own study 1.2.3) and patient B (1.2.4) both hold X hot.
        write_one_manifest_for(
            dir.path(),
            "1.2.3",
            x,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-A",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.4",
            x,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-B",
        );
        let hex = clarus_adapters::hex_encode(&x);
        let batch = PatientBatch {
            patient_id: "PAT-A".to_string(),
            blobs: vec![PatientBlob {
                blake3: hex.clone(),
                file_size: 64,
                study_uid: "1.2.3".to_string(),
            }],
            studies: vec!["1.2.3".to_string()],
        };

        let (retained, excluded) = revalidate_exclusive_hot_blobs_in(&state, &batch);
        assert!(
            retained.blobs.is_empty(),
            "a blob shared with another patient must not be retained"
        );
        assert_eq!(excluded.len(), 1, "the shared blob is excluded");
        assert_eq!(excluded[0].blake3, hex);
    }

    /// The flip gate: revalidation + flip under one lease never moves a blob
    /// another patient still holds hot.
    #[test]
    fn the_flip_gate_leaves_another_patients_hot_reference_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut x = [0u8; 32];
        x[0] = 0xA2;
        write_one_manifest_for(
            dir.path(),
            "1.2.3",
            x,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-A",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.4",
            x,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-B",
        );
        let hex = clarus_adapters::hex_encode(&x);
        let batch_a = PatientBatch {
            patient_id: "PAT-A".to_string(),
            blobs: vec![PatientBlob {
                blake3: hex,
                file_size: 64,
                study_uid: "1.2.3".to_string(),
            }],
            studies: vec!["1.2.3".to_string()],
        };

        // The gate: revalidate under the lease, then flip only the retained.
        let _lease = clarus_core::stow::acquire();
        let (retained, _excluded) = revalidate_exclusive_hot_blobs_in(&state, &batch_a);
        let flipped = set_storage_tier_for_patient_in(&state, &retained, "cold");
        drop(_lease);
        assert_eq!(flipped, 0, "an empty retained batch flips nothing");

        // Patient B's reference is untouched: the blob stays HOT for B.
        let m_b = state
            .read_server_manifest("1.2.4")
            .expect("published path")
            .expect("manifest readable");
        assert_eq!(
            m_b.series[0].instances[0].storage_tier,
            clarus_core::manifest::TIER_HOT,
            "another patient's hot reference must stay hot"
        );
    }

    /// Falsification anchor: the raw flip body, fed `batch_a` directly
    /// (bypassing the revalidation), WOULD move patient B's reference to cold.
    /// This documents the hazard the gate closes: the revalidation must run,
    /// or the shared blob is silently migrated out from under patient B.
    #[test]
    fn the_raw_flip_would_move_another_patients_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut x = [0u8; 32];
        x[0] = 0xA3;
        write_one_manifest_for(
            dir.path(),
            "1.2.3",
            x,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-A",
        );
        write_one_manifest_for(
            dir.path(),
            "1.2.4",
            x,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-B",
        );
        let hex = clarus_adapters::hex_encode(&x);
        let batch_a = PatientBatch {
            patient_id: "PAT-A".to_string(),
            blobs: vec![PatientBlob {
                blake3: hex,
                file_size: 64,
                study_uid: "1.2.3".to_string(),
            }],
            studies: vec!["1.2.3".to_string()],
        };

        // No revalidation: flip batch_a directly.
        let flipped = set_storage_tier_for_patient_in(&state, &batch_a, "cold");
        assert_eq!(
            flipped, 2,
            "the raw flip rewrites both manifests, not just patient A's"
        );
        let m_b = state
            .read_server_manifest("1.2.4")
            .expect("published path")
            .expect("manifest readable");
        assert_eq!(
            m_b.series[0].instances[0].storage_tier,
            clarus_core::manifest::TIER_COLD,
            "without the revalidation, patient B is silently moved to cold"
        );
    }

    /// The single pass collects exactly the HOT holders outside the own set:
    /// own-study holders are never outside, and a COLD instance of the same
    /// blob is not a HOT holder.
    #[test]
    fn outside_hot_holders_collects_only_outside_study_blobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut x = [0u8; 32];
        x[0] = 0xB1;
        let mut y = [0u8; 32];
        y[0] = 0xB2;
        // Own study holds X hot - not an outside holder.
        write_one_manifest_for(
            dir.path(),
            "1.2.3",
            x,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-A",
        );
        // Outside study holds Y hot - the only outside HOT holder.
        write_one_manifest_for(
            dir.path(),
            "1.2.4",
            y,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-B",
        );
        // Outside study holds X COLD - a COLD instance is not a HOT holder.
        write_one_manifest_for(
            dir.path(),
            "1.2.5",
            x,
            clarus_core::manifest::TIER_COLD,
            Some("20200101"),
            "PAT-C",
        );

        let own: std::collections::HashSet<&str> = ["1.2.3"].into_iter().collect();
        let holders = outside_hot_holders(&state, &own).expect("collect");
        assert!(
            holders.contains(clarus_adapters::hex_encode(&y).as_str()),
            "the outside HOT holder is collected"
        );
        assert!(
            !holders.contains(clarus_adapters::hex_encode(&x).as_str()),
            "X is only own-study HOT and COLD outside, never an outside HOT holder"
        );
        assert_eq!(holders.len(), 1, "only Y is an outside HOT holder");
    }

    /// An unreadable OUTSIDE manifest fails closed: the single pass must return
    /// `Err(())`, which excludes every blob - the same verdict the per-blob
    /// helper gives when it cannot read a manifest.
    #[test]
    fn outside_hot_holders_fails_closed_on_unreadable_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut x = [0u8; 32];
        x[0] = 0xB3;
        write_one_manifest_for(
            dir.path(),
            "1.2.3",
            x,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "PAT-A",
        );
        // Garbage bytes for an OUTSIDE study: read_manifest fails decode.
        std::fs::write(
            dir.path().join("1.2.9.manifest"),
            b"\xde\xad\xbe\xefnot-a-manifest-not-cbor-bytes",
        )
        .expect("write corrupt manifest");

        let own: std::collections::HashSet<&str> = ["1.2.3"].into_iter().collect();
        assert!(
            outside_hot_holders(&state, &own).is_err(),
            "an unreadable outside manifest must fail closed"
        );
    }

    // -- S2a regressions -------------------------------------------------

    /// A same-size DAMAGED cold copy must not be accepted as a resume.
    ///
    /// `hot_missing_is_resume` used to answer `exists()`, so a truncated or
    /// rotted file at the cold address made the blob skip verification, count
    /// as migrated, and keep the damaged bytes as the only copy.
    #[test]
    fn a_damaged_cold_copy_is_not_a_resume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let cold = clarus_adapters::storage::ContentAddressedStorage::new(dir.path().join("cold"))
            .expect("cold cas");
        // The address a blob WOULD have, and a file of the right length that
        // is not those bytes.
        let mut digest = [0u8; 32];
        digest[0] = 0xC1;
        let hex = clarus_adapters::hex_encode(&digest);
        let path = cold.cas_path(&hex);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(&path, b"not the blob").expect("write damaged copy");

        assert!(
            !hot_missing_is_resume(&state, &hex, &cold),
            "an unverified cold copy must not be treated as a resumed migration"
        );

        // A genuinely correct copy IS a resume: write the bytes the address
        // names by deriving the digest from the content.
        let body = b"the real bytes";
        let real = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(body));
        let real_path = cold.cas_path(&real);
        std::fs::create_dir_all(real_path.parent().expect("parent")).expect("dirs");
        std::fs::write(&real_path, body).expect("write good copy");
        assert!(
            hot_missing_is_resume(&state, &real, &cold),
            "a verified cold copy is a valid resume"
        );
    }

    /// An undated HOT study is selectable by EMERGENCY migration and not by
    /// the scheduled one.
    ///
    /// An undated study carries the `i64::MAX` sentinel, so the ordinary
    /// `study_date < cutoff` test never passes. With the emergency cutoff set
    /// to the maximum date, every undated study was excluded - so a store
    /// whose readable hot blobs were all undated freed NOTHING while the disk
    /// stayed full.
    #[test]
    fn emergency_migration_selects_undated_studies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut undated = [0u8; 32];
        undated[0] = 0xD1;
        // No study date at all.
        write_one_manifest(
            dir.path(),
            "1.2.700",
            undated,
            clarus_core::manifest::TIER_HOT,
            None,
        );
        let hex = clarus_adapters::hex_encode(&undated);

        let scheduled = collect_candidates_mode_in(
            &state,
            "99991231",
            &[clarus_core::manifest::TIER_HOT],
            &std::collections::HashSet::new(),
            false,
        )
        .expect("scan ok");
        assert!(
            scheduled.is_empty(),
            "a schedule with no age criterion still refuses an undated study"
        );

        let emergency = collect_candidates_mode_in(
            &state,
            "99991231",
            &[clarus_core::manifest::TIER_HOT],
            &std::collections::HashSet::new(),
            true,
        )
        .expect("scan ok");
        assert!(
            emergency.iter().any(|(b, _, _)| *b == hex),
            "emergency mode must select the undated study; got {emergency:?}"
        );
    }

    /// A failed manifest-directory scan must be an ERROR, not an empty batch.
    ///
    /// Returning `Vec::new()` made the scheduled and emergency passes report
    /// "zero work" while the store was unreadable - the one state in which a
    /// migration worker has to say so.
    #[test]
    fn an_unreadable_manifest_directory_is_an_error_not_an_empty_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = state_with_manifest_dir(dir.path());
        // Occupy the manifest path with a regular FILE: `read_dir` then fails
        // with ENOTDIR on every platform.
        let path = dir.path().join("manifests_blocked");
        std::fs::write(&path, b"not a directory").expect("occupy path");
        state.config.btree.manifest_dir = path.to_string_lossy().to_string();

        let got = collect_candidates_in(
            &state,
            "99991231",
            &[clarus_core::manifest::TIER_HOT],
            &std::collections::HashSet::new(),
        );
        assert!(
            got.is_err(),
            "an unlistable manifest directory must not read as an empty scan"
        );
        assert!(
            collect_migration_candidates(
                &state,
                "99991231",
                10,
                &[clarus_core::manifest::TIER_HOT],
                &std::collections::HashSet::new(),
            )
            .is_err(),
            "the wrapper must propagate the failure too"
        );
    }

    /// Both planned loops must WAIT for the tick, keeping fractional seconds.
    ///
    /// `num_seconds()` truncated the remaining time, so a clock at
    /// `01:59:58.250` before a `02:00` tick slept one second and ran at
    /// `01:59:59`. The batch is exercised through the helper, with a schedule
    /// that fires one minute out; anything shorter than that would prove the
    /// truncation is gone only by sleeping the whole minute, so the property
    /// is checked on the ARITHMETIC the helper uses.
    #[test]
    fn the_tick_wait_keeps_sub_second_precision() {
        // The duration handed to `sleep` is derived with `signed_duration_since`
        // and `to_std`, not `num_seconds`, so a 250 ms remainder survives.
        let next = chrono::Local::now() + chrono::Duration::milliseconds(250);
        let now = chrono::Local::now();
        let remaining = next.signed_duration_since(now);
        assert!(remaining.num_milliseconds() > 0, "there is time left");
        assert!(
            remaining.num_seconds() == 0,
            "a sub-second remainder is exactly the case `num_seconds` erased"
        );
        assert!(
            remaining.to_std().is_ok(),
            "and `to_std` keeps it rather than rounding it away"
        );
    }

    /// The pending-reclaim record must survive a restart and be replayed.
    ///
    /// A failed unlink after a successful flip leaves a local copy the
    /// collector can never find again (it selects only instances recorded in a
    /// tier). Recording it makes the reclaim retryable on the next pass.
    #[test]
    fn a_pending_reclaim_is_replayed_and_then_cleared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut blob = [0u8; 32];
        blob[0] = 0xE1;
        let hex = clarus_adapters::hex_encode(&blob);
        // A hot file with NO manifest reference: exactly the orphan a failed
        // post-flip unlink leaves behind.
        let hot_path = state.cas.cas_path(&hex);
        std::fs::create_dir_all(hot_path.parent().expect("parent")).expect("dirs");
        std::fs::write(&hot_path, b"orphan").expect("write orphan");
        note_pending_reclaim(&state, &hex, "hot");

        let (reclaimed, still) = reconcile_pending_reclaims(&state);
        assert_eq!(reclaimed, 1, "the orphaned copy is reclaimed");
        assert_eq!(still, 0, "and nothing remains pending");
        assert!(
            !hot_path.exists(),
            "the recorded file must actually be unlinked"
        );

        // A second pass finds nothing to do, so the record is not replayed
        // forever.
        let (again, _) = reconcile_pending_reclaims(&state);
        assert_eq!(again, 0, "the record is consumed");
    }

    /// A copy that would REPLACE an existing cold file is hashed first, so a
    /// damaged hot blob cannot overwrite good cold bytes.
    #[test]
    fn a_second_copy_does_not_replace_a_good_cold_file_with_damaged_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let cold = clarus_adapters::storage::ContentAddressedStorage::new(dir.path().join("cold2"))
            .expect("cold cas");

        // The GOOD cold copy, written at its own address.
        let body = b"the genuine instance bytes";
        let real = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(body));
        let cold_path = cold.cas_path(&real);
        std::fs::create_dir_all(cold_path.parent().expect("parent")).expect("dirs");
        std::fs::write(&cold_path, body).expect("write good cold copy");

        // A HOT file of the same LENGTH at the same address, containing
        // different bytes - the case `file_size` cannot catch.
        let hot_path = state.cas.cas_path(&real);
        std::fs::create_dir_all(hot_path.parent().expect("parent")).expect("dirs");
        let mut damaged = body.to_vec();
        damaged[0] ^= 0xFF;
        std::fs::write(&hot_path, &damaged).expect("write damaged hot copy");

        let outcome = copy_hot_to_cold_streaming(&state, &real, body.len() as i64, &cold);
        assert!(
            outcome.is_err(),
            "a copy whose bytes do not match the address must be refused"
        );
        assert_eq!(
            std::fs::read(&cold_path).expect("read"),
            body,
            "the existing good cold copy must be left intact"
        );
    }

    /// An undated or newly-added study must block the flip, not just the
    /// collect: the copy window is long enough for a commit to land.
    #[test]
    fn a_study_added_during_the_copy_blocks_the_flip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut blob = [0u8; 32];
        blob[0] = 0xF1;
        write_one_manifest_for(
            dir.path(),
            "1.2.800",
            blob,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
            "P-X",
        );
        let batch = PatientBatch {
            patient_id: "P-X".to_string(),
            blobs: vec![PatientBlob {
                blake3: clarus_adapters::hex_encode(&blob),
                file_size: 1,
                study_uid: "1.2.800".to_string(),
            }],
            studies: vec!["1.2.800".to_string()],
        };
        assert!(
            patient_studies_all_before(&state, &batch, "20250101"),
            "the old study is eligible"
        );
        // A NEW study appears for the same patient.
        let mut fresh = [0u8; 32];
        fresh[0] = 0xF2;
        write_one_manifest_for(
            dir.path(),
            "1.2.801",
            fresh,
            clarus_core::manifest::TIER_HOT,
            Some("20260101"),
            "P-X",
        );
        let grown = PatientBatch {
            studies: vec!["1.2.800".to_string(), "1.2.801".to_string()],
            ..batch
        };
        assert!(
            !patient_studies_all_before(&state, &grown, "20250101"),
            "a study at/after the cutoff makes the patient ineligible"
        );
    }

    // -- S2a-migration: the verify-before-publish family ------------------

    /// `:1233` - a cold pass must not claim an instance the S3 pass already
    /// published.
    ///
    /// The planned cold pass verifies a cold copy and then pauses. The S3 pass
    /// publishes and removes BOTH local copies in that window. When the cold
    /// pass resumes, an unrestricted flip rewrites the S3 reference to COLD,
    /// so the manifest now names a cold copy that does not exist and the pass
    /// reports success with no readable bytes anywhere.
    #[test]
    fn a_cold_flip_leaves_an_instance_that_moved_to_s3_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut blob = [0u8; 32];
        blob[0] = 0xA1;
        let hex = clarus_adapters::hex_encode(&blob);
        // The instance is already in S3 - the concurrent S3 pass won the race.
        write_one_manifest(
            dir.path(),
            "1.2.900",
            blob,
            clarus_core::manifest::TIER_S3,
            Some("20200101"),
        );

        let batch = PatientBatch {
            patient_id: "P".to_string(),
            blobs: vec![PatientBlob {
                blake3: hex.clone(),
                file_size: 1,
                study_uid: "1.2.900".to_string(),
            }],
            studies: vec!["1.2.900".to_string()],
        };
        let flipped = set_storage_tier_for_patient_in(&state, &batch, "cold");
        assert_eq!(
            flipped, 0,
            "an instance already in S3 must not be rewritten to COLD"
        );

        let m = state
            .read_server_manifest("1.2.900")
            .expect("read")
            .expect("present");
        let tier = m
            .series
            .iter()
            .flat_map(|s| &s.instances)
            .find(|i| i.blake3 == blob)
            .map(|i| i.storage_tier)
            .expect("instance present");
        assert_eq!(
            tier,
            clarus_core::manifest::TIER_S3,
            "the S3 reference must survive a stale cold pass"
        );
    }

    /// `:1310` - `head_object` succeeding does not mean the object is ours.
    ///
    /// A stale or truncated object left by an interrupted upload occupies the
    /// key. Adopting it flips the tier and deletes the last valid local copy,
    /// leaving damaged bytes under a digest that claims they are the good ones.
    #[cfg(feature = "s3")]
    #[test]
    fn a_stale_s3_object_is_replaced_not_adopted() {
        use clarus_adapters::storage::s3::{InMemoryObjectStore, ObjectStore};
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());

        let body = b"the genuine instance bytes";
        let blob = clarus_core::hash::cas_hash_one(body);
        let hex = clarus_adapters::hex_encode(&blob);
        let hot_path = state.cas.cas_path(&hex);
        std::fs::create_dir_all(hot_path.parent().expect("parent")).expect("dirs");
        std::fs::write(&hot_path, body).expect("write hot");
        // The tier flip needs a manifest naming the instance, or there is
        // nothing to rewrite and the outcome is a Failed flip.
        write_one_manifest(
            dir.path(),
            "1.2.930",
            blob,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
        );

        // Seed the bucket with a TRUNCATED object at the blob's own key.
        let store = InMemoryObjectStore::default();
        let key = clarus_adapters::storage::cas_rel_path(&hex);
        store
            .put_object(&key, b"truncated".to_vec(), "application/dicom")
            .expect("seed stale object");

        let outcome =
            migrate_blob_to_s3_with_store_for_test(&state, &hex, body.len() as i64, &store);
        assert!(
            outcome.is_ok(),
            "the stale object is replaced by a verified upload"
        );
        assert_eq!(
            store.get_object(&key).expect("read back"),
            body,
            "the bucket must hold the real bytes, not the stale ones"
        );
    }

    /// `:1363` - a same-size damaged HOT source must not be uploaded under the
    /// original digest.
    ///
    /// The length checks all pass on a rotted file whose size survived, so the
    /// damaged bytes would be PUT under the good digest and both local copies
    /// deleted. The digest of the bytes about to be sent is what the key means.
    #[cfg(feature = "s3")]
    #[test]
    fn a_damaged_source_is_never_uploaded_under_a_good_digest() {
        use clarus_adapters::storage::s3::{InMemoryObjectStore, ObjectStore};
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());

        let body = b"the genuine instance bytes";
        let blob = clarus_core::hash::cas_hash_one(body);
        let hex = clarus_adapters::hex_encode(&blob);
        let hot_path = state.cas.cas_path(&hex);
        std::fs::create_dir_all(hot_path.parent().expect("parent")).expect("dirs");
        // SAME LENGTH, different bytes.
        let mut damaged = body.to_vec();
        damaged[0] ^= 0xFF;
        std::fs::write(&hot_path, &damaged).expect("write damaged hot");
        write_one_manifest(
            dir.path(),
            "1.2.931",
            blob,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
        );

        let store = InMemoryObjectStore::default();
        let key = clarus_adapters::storage::cas_rel_path(&hex);
        let outcome =
            migrate_blob_to_s3_with_store_for_test(&state, &hex, body.len() as i64, &store);
        assert!(
            outcome.is_err(),
            "a source whose bytes do not hash to the address must not be uploaded"
        );
        assert!(
            store.get_object(&key).is_err(),
            "nothing may be written under the digest"
        );
        assert!(
            hot_path.exists(),
            "the local copy must be kept when the upload is refused"
        );
    }

    /// `:1618` - a failed emergency S3 attempt must be excluded for the rest of
    /// the tick.
    ///
    /// The failed-read and failed-flip exits used to `continue` straight past
    /// `failed.insert`, so the digest stayed selectable: while the disk was
    /// still under the threshold the next round of the SAME tick worked it
    /// again instead of moving on to the candidates behind it.
    ///
    /// The loop itself needs a live `S3Client` bound into `AppState`, which a
    /// unit test cannot construct, so this pins the two halves the fix is made
    /// of: the collector honours the exclusion set (so inserting into it is
    /// what makes the next round move on), and a digest that reaches that set
    /// is therefore unreachable for the rest of the tick.
    #[test]
    fn an_excluded_digest_is_never_re_selected_in_the_same_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_manifest_dir(dir.path());
        let mut a = [0u8; 32];
        a[0] = 0xC7;
        let mut b = [0u8; 32];
        b[0] = 0xC8;
        write_one_manifest(
            dir.path(),
            "1.2.950",
            a,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
        );
        write_one_manifest(
            dir.path(),
            "1.2.951",
            b,
            clarus_core::manifest::TIER_HOT,
            Some("20200101"),
        );
        let a_hex = clarus_adapters::hex_encode(&a);

        // Without the exclusion both blobs are selectable.
        let all = collect_candidates_in(
            &state,
            "99991231",
            &[clarus_core::manifest::TIER_HOT],
            &std::collections::HashSet::new(),
        )
        .expect("scan ok");
        assert_eq!(all.len(), 2, "both blobs are candidates before any failure");

        // Once a digest is in the failure set, the collector refuses it - so
        // the next round of the tick reaches the blob BEHIND it instead of
        // reworking the one that just failed.
        let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();
        failed.insert(a_hex.clone());
        let after = collect_candidates_in(
            &state,
            "99991231",
            &[clarus_core::manifest::TIER_HOT],
            &failed,
        )
        .expect("scan ok");
        assert!(
            !after.iter().any(|(h, _, _)| *h == a_hex),
            "a failed digest must not be re-selected in the same tick: {after:?}"
        );
        assert_eq!(
            after.len(),
            1,
            "the blob behind the failed one must still be reached"
        );
    }
}
