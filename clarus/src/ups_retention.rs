// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Retention sweep for workitems in a final state (PS3.18 Section 11 policy).
//!
//! PS3.18 defines no Delete transaction for workitems and puts no duration on
//! how long an origin keeps one (section 11; PS3.4 CC.2.1.3 constrains only
//! *when* a `COMPLETED`/`CANCELED` instance may go - after its deletion locks
//! are gone). Retention is therefore an SCP policy, and the sweep here is the
//! same non-standard DELETE extension the admin API already exposes, applied in
//! bulk on a timer.
//!
//! # What is swept
//!
//! A workitem whose state is `COMPLETED` or `CANCELED` and whose `updated_at`
//! is older than `[ups] final_state_retention_days`. `SCHEDULED` and
//! `IN PROGRESS` are never swept: a workitem stuck `IN PROGRESS` is the
//! dead-claimant case, which an operator resolves explicitly.
//!
//! # Deletion Locks
//!
//! A subscription created with `?deletionlock=true` (PS3.18 11.10) holds a
//! Deletion Lock, and a lock still inside `[ups] deletion_lock_ttl` keeps a
//! final workitem from being swept. That is the "internal logic to override
//! remaining deletion locks" AIW-I 4.85.4.1.3 permits, bounded by the
//! 24-hour floor the same paragraph requires - hence the default TTL. Setting
//! the TTL to `0` turns the wait off and restores the pre-lock behaviour.
//!
//! # Interplay with the CAD workitem
//!
//! A CAD workitem's UID is derived from the study UID, so sweeping a final one
//! lets a re-STOW of the same study mint the SAME UID again. Idempotency is
//! preserved, and re-inference of a study becomes possible by design.
//!
//! `0` days disables the sweep entirely (keep final workitems forever).

#![forbid(unsafe_code)]

use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::state::AppState;

/// How long the sweep thread waits between passes.
const SWEEP_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Seconds in one day, for turning the configured day count into a cutoff.
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// Seconds in one hour, for turning the configured Deletion Lock TTL into a
/// horizon. Deliberately derived from `SECS_PER_DAY` rather than written out
/// as 3600: the two must agree, and one of them is already the day.
const SECS_PER_HOUR: u64 = SECS_PER_DAY / 24;

/// Delete the final-state workitems that are past the retention window.
///
/// Runs once inline (so a start-up pass needs no extra machinery) and then on
/// the `SWEEP_INTERVAL_SECS` cadence from [`spawn_ups_retention`]. Returns the
/// number deleted.
///
/// A record with no `updated_at` is left alone: the timestamp is what the
/// decision reads, and a record without one cannot be shown to be old.
pub fn sweep_final_workitems(state: &AppState) -> usize {
    let days = state.config.ups.final_state_retention_days;
    if days == 0 {
        return 0;
    }

    let cutoff = now_secs().saturating_sub(days * SECS_PER_DAY);
    // The Deletion Lock horizon, if any subscription still holds a lock
    // (PS3.18 11.10 creates it; PS3.4 CC.2.3.1 owns its lifetime; AIW-I
    // 4.86.4.1.2 requires the Manager to support it).
    //
    // `Some(horizon)` means a lock taken at `lock_at` is still inside the
    // configured TTL, so a final workitem must survive this pass. `None`
    // means no lock applies - either none is held, or `deletion_lock_ttl`
    // is 0, which is the "override" AIW-I 4.85.4.1.3 allows and reproduces
    // the pre-lock behaviour exactly.
    let lock_horizon = deletion_lock_horizon(state.config.ups.deletion_lock_ttl);
    let store = clarus_core::ups::WorkItemStore::new(state.workitems_dir.clone());

    let records = match store.search(&clarus_core::ups::StoreFilter::default()) {
        Ok((r, _total)) => r,
        Err(error) => {
            crate::clog_warn!(error = %error, "UPS retention sweep: search failed");
            return 0;
        }
    };

    let mut deleted = 0usize;
    for rec in records {
        if !is_final_state(&rec.workflow_state) {
            continue;
        }
        let Some(updated) = rec.updated_at.as_deref() else {
            continue;
        };
        let Some(updated_secs) = parse_stamp_secs(updated) else {
            continue;
        };
        if updated_secs >= cutoff {
            continue;
        }
        // A lock inside its TTL holds this workitem back. The lock is
        // evaluated against the sweep's own clock rather than the
        // workitem's timestamp: the question is "is a lock still held
        // NOW", not "was one held when the workitem finished".
        if let Some(horizon) = lock_horizon
            && horizon.is_locked_at(now_secs())
        {
            // Tagged so a RELEASE build keeps the line: a workitem the sweep
            // declined to delete is the question an operator asks first when a
            // locked step outlives its retention horizon.
            crate::clog_event!(
                crate::clog::Level::Info,
                "UPS_RETENTION",
                "",
                "kept={} state={} lock_age_hours={}",
                rec.workitem_uid,
                rec.workflow_state,
                horizon.age_hours(now_secs())
            );
            continue;
        }
        match store.delete(&rec.workitem_uid) {
            Ok(true) => {
                deleted += 1;
                crate::metrics::UPS_RETENTION_SWEPT.fetch_add(1, Ordering::Relaxed);
                // The override case AIW-I 4.85.4.1.3 describes: the server
                // deleted past a remaining lock. That is permitted only
                // because the TTL was at least the 24-hour floor (or the
                // operator set 0 and turned the wait off), so the log names
                // both the workitem and the lock's age for an auditor.
                match lock_horizon {
                    Some(horizon) if horizon.was_locked_at(updated_secs) => {
                        crate::clog_warn!(
                            workitem_uid = %rec.workitem_uid,
                            state = %rec.workflow_state,
                            lock_age_hours = horizon.age_hours(now_secs()),
                            ttl_hours = state.config.ups.deletion_lock_ttl,
                            "UPS retention sweep removed a final workitem past an expired \
                             Deletion Lock"
                        );
                    }
                    _ => {
                        // Tagged so a RELEASE build records the deletion: a
                        // line dropped at INFO leaves a removed workitem with
                        // no trace at all.
                        crate::clog_event!(
                            crate::clog::Level::Info,
                            "UPS_RETENTION",
                            "",
                            "removed={} state={}",
                            rec.workitem_uid,
                            rec.workflow_state
                        );
                    }
                }
            }
            // Vanished under us (a concurrent DELETE) - not a failure.
            Ok(false) => {}
            Err(error) => {
                crate::clog_warn!(
                    error = %error,
                    workitem_uid = %rec.workitem_uid,
                    "UPS retention sweep: delete failed"
                );
            }
        }
    }
    deleted
}

/// The newest Deletion Lock the sweep must honour, if any.
///
/// `ttl_hours == 0` means the operator turned lock-honouring OFF - the
/// `deletion_lock_ttl` dial's zero value, which is the configurability
/// AIW-I 4.85.4.1.3 attaches to the override. In that case no horizon is
/// returned and the sweep behaves exactly as it did before locks existed.
///
/// When locks are honoured, the horizon carries the LOCK INSTANT (the newest
/// among all subscriptions that hold one) and the TTL, so the caller can ask
/// both "is a lock still active" and "was a lock held when this workitem
/// finished" without re-reading the registry.
fn deletion_lock_horizon(ttl_hours: u64) -> Option<LockHorizon> {
    if ttl_hours == 0 {
        return None;
    }
    let lock_at = crate::push::subscriptions().newest_deletion_lock()?;
    Some(LockHorizon {
        lock_at,
        ttl_secs: ttl_hours.saturating_mul(SECS_PER_HOUR),
    })
}

/// A Deletion Lock the sweep must honour, and until when.
#[derive(Debug, Clone, Copy)]
struct LockHorizon {
    /// When the newest lock was taken, seconds since the Unix epoch.
    lock_at: u64,
    /// The configured lock lifetime, already in seconds.
    ttl_secs: u64,
}

impl LockHorizon {
    /// Whether the lock is still inside its TTL at `now`.
    #[must_use]
    fn is_locked_at(&self, now: u64) -> bool {
        now.saturating_sub(self.lock_at) < self.ttl_secs
    }

    /// Whether a lock was in force when the workitem reached its final state.
    ///
    /// Used only to choose the log line: a sweep that removes a workitem the
    /// lock had already stopped protecting is worth naming, because it is
    /// the override the profile requires to be capable of waiting 24 hours.
    #[must_use]
    fn was_locked_at(&self, then: u64) -> bool {
        then >= self.lock_at
    }

    /// The lock's age in hours at `now`, for the log.
    #[must_use]
    fn age_hours(&self, now: u64) -> u64 {
        now.saturating_sub(self.lock_at) / SECS_PER_HOUR
    }
}

/// Start the retention thread: one pass now, then one every 24 hours.
///
/// Same `std::thread` shape as the push worker and the erase worker - no
/// runtime, no new dependency. When the retention days are `0` the thread is
/// not started at all.
pub fn spawn_ups_retention(state: AppState) {
    if state.config.ups.final_state_retention_days == 0 {
        return;
    }
    std::thread::Builder::new()
        .name("ups-retention".into())
        .spawn(move || {
            loop {
                let deleted = sweep_final_workitems(&state);
                if deleted > 0 {
                    crate::clog_event!(
                        crate::clog::Level::Info,
                        "UPS_RETENTION",
                        "",
                        "sweep finished deleted={}",
                        deleted
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
            }
        })
        .expect("Failed to spawn UPS retention thread");
}

/// Whether this state is one the sweep may remove.
///
/// Kept local so the sweep's predicate is readable on its own line; it mirrors
/// `handlers::ups::state_machine::is_final_state`.
fn is_final_state(state: &str) -> bool {
    matches!(state, "COMPLETED" | "CANCELED")
}

/// Seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse the store's `YYYY-MM-DD HH:MM:SS` UTC stamp into epoch seconds.
///
/// The store owns the format (see `ups_pure::now_stamp`), so the inverse lives
/// next to its consumer rather than in a general date library.
fn parse_stamp_secs(stamp: &str) -> Option<u64> {
    let (date, time) = stamp.split_once(' ')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: u64 = t.next()?.parse().ok()?;
    let minute: u64 = t.next()?.parse().ok()?;
    let second: u64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((days as u64) * SECS_PER_DAY + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (the inverse of the
/// `civil_from_days` used by the stamp writer).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarus_core::ups::{StoreFilter, WorkItemRecord, WorkItemStore};
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};

    /// Serialises every test that runs the sweep.
    ///
    /// The sweep reads the process-wide subscription registry, which the
    /// Deletion Lock tests must seed and clear. Without this lock a lock
    /// seeded by one test would be visible to another test's sweep and change
    /// its result - which is exactly how the first draft of these tests
    /// failed. One mutex for the whole module is the cheapest way to keep the
    /// registry a shared resource while making each test's view of it its own.
    static SWEEP_LOCK: StdMutex<()> = StdMutex::new(());

    /// Run `f` holding the module's sweep lock, with an empty registry, and
    /// restore whatever the registry held afterwards.
    fn with_clean_registry<R>(f: impl FnOnce() -> R) -> R {
        let _guard = SWEEP_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let registry = crate::push::subscriptions();
        let saved = registry.snapshot();
        registry.clear();
        let out = f();
        registry.clear();
        for sub in saved {
            registry.subscribe(sub);
        }
        out
    }

    /// Register one subscription carrying a lock taken `age_secs` ago.
    fn hold_lock_aged(age_secs: u64) {
        crate::push::subscriptions().subscribe(crate::push::Subscription {
            subscriber: "LOCKHOLDER".to_string(),
            deletion_lock_at: Some(now_secs().saturating_sub(age_secs)),
            ..crate::push::Subscription::default()
        });
    }

    /// A workitem record with the three keys the store requires plus the
    /// state and stamp the sweep reads. Everything else stays absent, which
    /// is what the sweep must tolerate.
    fn record(uid: &str, state: &str, updated_at: Option<&str>) -> WorkItemRecord {
        WorkItemRecord {
            id: None,
            workitem_uid: uid.to_string(),
            workflow_state: state.to_string(),
            modality: String::new(),
            updated_at: updated_at.map(str::to_string),
            ..WorkItemRecord::default()
        }
    }

    /// A real `AppState` over a temp tree, so the sweep runs against the same
    /// store the server uses instead of a stand-in. `days` is the only knob
    /// under test.
    fn state_with_retention(root: &Path, days: u64) -> AppState {
        state_with_knobs(root, days, 24)
    }

    /// As above, with the Deletion Lock TTL also under the caller's control.
    fn state_with_knobs(root: &Path, days: u64, lock_ttl_hours: u64) -> AppState {
        let idx_dir = root.join("idx");
        let manifest_dir = root.join("manifests");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");

        let mut config = crate::config::Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        config.ups.final_state_retention_days = days;
        config.ups.deletion_lock_ttl = lock_ttl_hours;

        AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(root.join("cas"))
                .expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: crate::sse_fanout::DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            log_buffer: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            maintenance: Arc::new(StdRwLock::new(crate::state::MaintenanceState::default())),
            #[cfg(feature = "s3")]
            s3: Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: crate::shadow::ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
            config: config.clone(),
            workitems_dir: root.join("workitems"),
            commitments_dir: root.join("commitments"),
            transcode_pool: crate::state::TranscodePool::new(2),
            readiness: crate::handlers::readiness::Readiness::new(),
        }
    }

    /// A stamp `days` in the past, in the store's `YYYY-MM-DD HH:MM:SS` form.
    fn stamp_days_ago(days: u64) -> String {
        let secs = now_secs().saturating_sub(days * SECS_PER_DAY);
        let (y, m, d, hh, mm, ss) = civil_from_secs(secs);
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    }

    /// Inverse of `days_from_civil`, for building test stamps only.
    fn civil_from_secs(secs: u64) -> (i64, u32, u32, u64, u64, u64) {
        let days = (secs / SECS_PER_DAY) as i64;
        let rem = secs % SECS_PER_DAY;
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = if m <= 2 { y + 1 } else { y };
        (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
    }

    /// Seed the store with the given records and return it for assertions.
    fn seeded(state: &AppState, recs: &[WorkItemRecord]) -> WorkItemStore {
        let store = WorkItemStore::new(state.workitems_dir.clone());
        for rec in recs {
            store.insert(rec).expect("seed workitem");
        }
        store
    }

    fn remaining(store: &WorkItemStore) -> Vec<String> {
        let mut uids: Vec<String> = store
            .search(&StoreFilter::default())
            .map(|(v, _)| v)
            .expect("search")
            .into_iter()
            .map(|r| r.workitem_uid)
            .collect();
        uids.sort();
        uids
    }

    #[test]
    fn only_a_final_workitem_past_the_window_is_removed() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            let state = state_with_retention(temp.path(), 7);
            let store = seeded(
                &state,
                &[
                    // Final and old on both final states -> swept.
                    record("1.1.1.1", "COMPLETED", Some(&stamp_days_ago(30))),
                    record("1.1.1.2", "CANCELED", Some(&stamp_days_ago(8))),
                    // Final but fresh -> kept, one just inside the window.
                    record("1.1.1.3", "COMPLETED", Some(&stamp_days_ago(6))),
                    record("1.1.1.4", "CANCELED", Some(&stamp_days_ago(0))),
                    // Non-final and old -> kept: a state the sweep must not touch.
                    record("1.1.1.5", "SCHEDULED", Some(&stamp_days_ago(365))),
                    record("1.1.1.6", "IN PROGRESS", Some(&stamp_days_ago(365))),
                ],
            );

            let deleted = sweep_final_workitems(&state);

            assert_eq!(deleted, 2, "the two old final workitems");
            assert_eq!(
                remaining(&store),
                vec!["1.1.1.3", "1.1.1.4", "1.1.1.5", "1.1.1.6"],
                "fresh finals and every non-final state survive"
            );
        });
    }

    #[test]
    fn a_final_workitem_without_a_readable_stamp_is_kept() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            let state = state_with_retention(temp.path(), 7);
            let store = seeded(
                &state,
                &[
                    // No stamp at all: cannot be shown to be old.
                    record("1.2.1.1", "COMPLETED", None),
                    // A stamp the parser refuses -> treated the same way.
                    record("1.2.1.2", "CANCELED", Some("not a stamp")),
                ],
            );

            assert_eq!(sweep_final_workitems(&state), 0, "nothing is provably old");
            assert_eq!(remaining(&store), vec!["1.2.1.1", "1.2.1.2"]);
        });
    }

    // -- Deletion Lock horizon (PS3.18 11.10, AIW-I 4.85.4.1.3) ----------

    #[test]
    fn a_live_lock_holds_a_final_workitem_back() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            let state = state_with_retention(temp.path(), 7);
            let store = seeded(
                &state,
                &[record("2.1.1.1", "COMPLETED", Some(&stamp_days_ago(30)))],
            );
            hold_lock_aged(0);

            assert_eq!(
                sweep_final_workitems(&state),
                0,
                "the lock, taken now, is well inside its TTL"
            );
            assert_eq!(remaining(&store), vec!["2.1.1.1"]);
        });
    }

    #[test]
    fn an_expired_lock_no_longer_holds_a_final_workitem_back() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            let state = state_with_knobs(temp.path(), 7, 24);
            let store = seeded(
                &state,
                &[record("2.2.1.1", "COMPLETED", Some(&stamp_days_ago(30)))],
            );
            // The configured TTL is 24h, so a lock taken 25h ago is spent.
            hold_lock_aged(25 * SECS_PER_HOUR);

            assert_eq!(
                sweep_final_workitems(&state),
                1,
                "the lock is past its TTL, so AIW-I 4.85.4.1.3's override applies"
            );
            assert!(remaining(&store).is_empty());
        });
    }

    #[test]
    fn a_zero_ttl_overrides_every_lock() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            // The knob is the only thing changed: a lock held RIGHT NOW.
            let state = state_with_knobs(temp.path(), 7, 0);
            let store = seeded(
                &state,
                &[record("2.3.1.1", "COMPLETED", Some(&stamp_days_ago(30)))],
            );
            hold_lock_aged(0);

            assert_eq!(
                sweep_final_workitems(&state),
                1,
                "0 means locks are not honoured at all"
            );
            assert!(remaining(&store).is_empty());
        });
    }

    #[test]
    fn a_lock_does_not_rescue_a_non_final_or_fresh_workitem() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            let state = state_with_retention(temp.path(), 7);
            let store = seeded(
                &state,
                &[
                    // Non-final: outside the sweep's remit whether or not a
                    // lock exists.
                    record("2.4.1.1", "IN PROGRESS", Some(&stamp_days_ago(30))),
                    // Final but inside the retention window already.
                    record("2.4.1.2", "COMPLETED", Some(&stamp_days_ago(1))),
                ],
            );
            hold_lock_aged(0);

            assert_eq!(sweep_final_workitems(&state), 0);
            assert_eq!(remaining(&store), vec!["2.4.1.1", "2.4.1.2"]);
        });
    }

    #[test]
    fn the_horizon_reads_the_newest_lock_only() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            let state = state_with_retention(temp.path(), 7);
            let store = seeded(
                &state,
                &[record("2.5.1.1", "COMPLETED", Some(&stamp_days_ago(30)))],
            );
            // One spent lock and one live one: the live one must win, since
            // honouring only the newest is the permissive reading and a
            // subscriber that just asked for a lock must not be ignored
            // because an older subscription's lock expired.
            hold_lock_aged(48 * SECS_PER_HOUR);
            hold_lock_aged(0);

            assert_eq!(
                sweep_final_workitems(&state),
                0,
                "the newer lock is still live"
            );
            assert_eq!(remaining(&store), vec!["2.5.1.1"]);
        });
    }

    #[test]
    fn the_lock_horizon_arithmetic() {
        // Directly, without the registry: the boundary is `age < ttl`.
        let horizon = LockHorizon {
            lock_at: 1_000,
            ttl_secs: 100,
        };
        assert!(horizon.is_locked_at(1_000), "at the instant it was taken");
        assert!(horizon.is_locked_at(1_099), "one second inside the TTL");
        assert!(!horizon.is_locked_at(1_100), "the TTL is exclusive");
        assert!(!horizon.is_locked_at(2_000));

        assert!(horizon.was_locked_at(1_000));
        assert!(horizon.was_locked_at(1_500), "the lock outlived the stamp");
        assert!(!horizon.was_locked_at(999), "finished before the lock");

        assert_eq!(horizon.age_hours(1_000 + 3600), 1);
        assert_eq!(horizon.age_hours(999), 0, "saturating, never negative");
    }

    #[test]
    fn zero_days_disables_the_sweep_entirely() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            let state = state_with_retention(temp.path(), 0);
            let store = seeded(
                &state,
                &[record("1.3.1.1", "COMPLETED", Some(&stamp_days_ago(3650)))],
            );

            assert_eq!(
                sweep_final_workitems(&state),
                0,
                "0 keeps final workitems forever"
            );
            assert_eq!(remaining(&store), vec!["1.3.1.1"]);
        });
    }

    #[test]
    fn the_sweep_is_idempotent_on_a_second_pass() {
        with_clean_registry(|| {
            let temp = tempfile::tempdir().expect("temp dir");
            let state = state_with_retention(temp.path(), 7);
            let store = seeded(
                &state,
                &[record("1.4.1.1", "COMPLETED", Some(&stamp_days_ago(30)))],
            );

            assert_eq!(sweep_final_workitems(&state), 1, "first pass removes it");
            assert_eq!(
                sweep_final_workitems(&state),
                0,
                "second pass finds nothing"
            );
            assert!(remaining(&store).is_empty());
        });
    }

    #[test]
    fn a_stamp_round_trips_through_epoch_seconds() {
        // 1970-01-01 00:00:00 is the epoch itself.
        assert_eq!(parse_stamp_secs("1970-01-01 00:00:00"), Some(0));
        // One day later, and the values the writer emits in the field.
        assert_eq!(parse_stamp_secs("1970-01-02 00:00:00"), Some(86_400));
        // A date the CAD path stamps on a real run.
        let secs = parse_stamp_secs("2026-09-17 12:00:00").expect("parses");
        assert_eq!(secs % 86_400, 12 * 3600);
    }

    #[test]
    fn malformed_stamps_are_refused_rather_than_guessed() {
        assert_eq!(parse_stamp_secs("not a stamp"), None);
        assert_eq!(parse_stamp_secs("2026-13-01 00:00:00"), None);
        assert_eq!(parse_stamp_secs("2026-01-32 00:00:00"), None);
        assert_eq!(parse_stamp_secs("2026-01-01"), None);
    }

    #[test]
    fn only_final_states_are_sweepable() {
        assert!(is_final_state("COMPLETED"));
        assert!(is_final_state("CANCELED"));
        assert!(!is_final_state("SCHEDULED"));
        assert!(!is_final_state("IN PROGRESS"));
    }

    #[test]
    fn the_test_stamp_helper_agrees_with_the_parser() {
        // Guards the test helper itself: a wrong inverse would make the
        // window assertions above pass for the wrong reason.
        let secs = parse_stamp_secs(&stamp_days_ago(8)).expect("parses");
        let expected = now_secs().saturating_sub(8 * SECS_PER_DAY);
        assert!(
            secs <= expected && expected - secs < 2,
            "an 8-day-old stamp must land on the same day as the cutoff"
        );
    }
}
