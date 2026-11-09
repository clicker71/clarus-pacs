// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Retention sweep for the MWL and MPPS records (PS3.18 14/15 policy).
//!
//! Neither service puts a duration on how long an origin keeps a record: MWL
//! is a read-only projection of a scheduler's worklist, and MPPS is a
//! performed step the performer writes once and finishes. Retention is
//! therefore an SCP policy, and this is where the policy is applied - the
//! same non-standard Delete extension the UPS sweep already is, on a timer.
//!
//! # What is swept
//!
//! - An MPPS record whose status is `COMPLETED` or `DISCONTINUED` and whose
//!   `updated_at` is older than `[workflow] retention_days`.
//! - An MWL SPS record whose `updated_at` is older than the same window. An
//!   MWL row has no state machine, so age is the whole predicate.
//!
//! `IN PROGRESS` MPPS records are NEVER swept: a step that never finished is
//! the stuck-performer case, which an operator resolves explicitly rather than
//! by dropping the row that describes it.
//!
//! `0` days disables the sweep entirely (keep records forever).

#![forbid(unsafe_code)]

use std::time::{SystemTime, UNIX_EPOCH};

use clarus_core::workflow::{MppsRecord, WorkflowStore};

use crate::state::AppState;

/// How long the sweep thread waits between passes.
const SWEEP_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Seconds in one day, for turning the configured day count into a cutoff.
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// Delete the expired MWL and MPPS records. Returns `(mwl_deleted, mpps_deleted)`.
///
/// Runs once inline (so a start-up pass needs no extra machinery) and then on
/// the `SWEEP_INTERVAL_SECS` cadence from [`spawn_workflow_retention`].
///
/// A record with no `updated_at` is left alone: the timestamp is what the
/// decision reads, and a record without one cannot be shown to be old.
pub fn sweep_workflow(state: &AppState) -> (usize, usize) {
    let days = state.config.workflow.retention_days;
    if days == 0 {
        return (0, 0);
    }
    let cutoff = now_secs().saturating_sub(days * SECS_PER_DAY);
    let store = WorkflowStore::new(std::path::PathBuf::from(
        state.config.workflow.resolved_dir(),
    ));

    let mwl_deleted = sweep_mwl(&store, cutoff);
    let mpps_deleted = sweep_mpps(&store, cutoff);
    (mwl_deleted, mpps_deleted)
}

/// Sweep the expired MWL rows (age only - an MWL row has no state machine).
fn sweep_mwl(store: &WorkflowStore, cutoff: u64) -> usize {
    let records = match store.all_sps() {
        Ok(r) => r,
        Err(error) => {
            crate::clog_warn!(error = %error, "MWL retention sweep: scan failed");
            return 0;
        }
    };
    let mut deleted = 0usize;
    for rec in records {
        let Some(updated) = rec.updated_at.as_deref().or(rec.created_at.as_deref()) else {
            continue;
        };
        let Some(updated_secs) = parse_stamp_secs(updated) else {
            continue;
        };
        if updated_secs >= cutoff {
            continue;
        }
        match store.delete_sps(&rec.sps_uid) {
            Ok(true) => deleted += 1,
            Ok(false) => {}
            Err(error) => {
                crate::clog_warn!(sps_uid = %rec.sps_uid, error = %error,
                    "MWL retention sweep: delete failed");
            }
        }
    }
    deleted
}

/// Sweep the FINAL MPPS rows that are past the window.
fn sweep_mpps(store: &WorkflowStore, cutoff: u64) -> usize {
    let records = match store.all_mpps() {
        Ok(r) => r,
        Err(error) => {
            crate::clog_warn!(error = %error, "MPPS retention sweep: scan failed");
            return 0;
        }
    };
    let mut deleted = 0usize;
    for rec in records {
        // An IN PROGRESS step is never swept - it is the dead-claimant case.
        if !MppsRecord::is_final_status(&rec.status) {
            continue;
        }
        let Some(updated) = rec.updated_at.as_deref().or(rec.created_at.as_deref()) else {
            continue;
        };
        let Some(updated_secs) = parse_stamp_secs(updated) else {
            continue;
        };
        if updated_secs >= cutoff {
            continue;
        }
        match store.delete_mpps(&rec.mpps_uid) {
            Ok(true) => deleted += 1,
            Ok(false) => {}
            Err(error) => {
                crate::clog_warn!(mpps_uid = %rec.mpps_uid, error = %error,
                    "MPPS retention sweep: delete failed");
            }
        }
    }
    deleted
}

/// Start the sweep thread. When `retention_days` is `0` (or the surface is
/// disabled) no thread is started at all.
pub fn spawn_workflow_retention(state: AppState) {
    if state.config.workflow.retention_days == 0 || !state.config.workflow.enabled {
        return;
    }
    std::thread::Builder::new()
        .name("workflow-retention".into())
        .spawn(move || {
            loop {
                let (mwl, mpps) = sweep_workflow(&state);
                if mwl + mpps > 0 {
                    crate::clog_info!(mwl, mpps, "workflow retention sweep finished");
                }
                std::thread::sleep(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
            }
        })
        .expect("Failed to spawn workflow retention thread");
}

/// Seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a `YYYY-MM-DD HH:MM:SS` UTC stamp into seconds since the epoch.
///
/// The inverse of the stamp writer in `handlers::ups_pure::now_stamp`; a
/// malformed stamp is `None` (the record is then left alone, never guessed at).
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

/// Days since 1970-01-01 for a proleptic-Gregorian date.
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

    #[test]
    fn parse_stamp_roundtrips_a_known_instant() {
        // 1970-01-01 00:00:00 is the epoch; one day later is 86400.
        assert_eq!(parse_stamp_secs("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_stamp_secs("1970-01-02 00:00:00"), Some(86_400));
        assert!(parse_stamp_secs("2026-10-11 09:30:00").is_some());
    }

    #[test]
    fn parse_stamp_rejects_garbage() {
        assert_eq!(parse_stamp_secs("not a stamp"), None);
        assert_eq!(parse_stamp_secs("2026-13-01 00:00:00"), None);
        assert_eq!(parse_stamp_secs("2026-10-11"), None);
    }
}
