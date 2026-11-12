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
//! - An MWL SPS record whose age is past the same window. An MWL row has no
//!   state machine, so age is the whole predicate - where the age is measured
//!   from the LATER of the record stamp and the scheduled start date, so an
//!   order filed today for a slot next month is not swept before the
//!   procedure.
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
///
/// The age reference is the LATER of the record stamp and the scheduled start,
/// because a worklist row's useful life ends when the procedure happens, not
/// when the order was filed. An order imported today for a slot 45 days out has
/// `updated_at` = today, so an age-only rule deleted it long before the
/// modality would ever query for it - the row disappeared exactly when it
/// became the thing the worklist exists to return.
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
        // A future start date holds the row past its filing age. A start date
        // in the past is already covered by the stamp, so `max` never shortens
        // a row's life.
        let age_secs = scheduled_start_secs(&rec.sps_start_date, &rec.sps_start_time)
            .map_or(updated_secs, |start| updated_secs.max(start));
        if age_secs >= cutoff {
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

/// Convert an MWL `(sps_start_date, sps_start_time)` pair into epoch seconds.
///
/// The stored forms are the DICOM ones - `DA` `YYYYMMDD` and `TM` `HHMMSS`
/// (PS3.4 K.6-1 / PS3.3 C.4.1). `TM` may carry a fractional part
/// (`HHMMSS.FFFFFF`); the fraction is dropped because retention works in days.
/// A malformed or empty pair is `None`, so a row whose schedule cannot be read
/// falls back to the stamp rather than being kept or dropped on a guess.
fn scheduled_start_secs(date: &str, time: &str) -> Option<u64> {
    let date = date.trim();
    let time = time.trim();
    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i64 = date.get(0..4)?.parse().ok()?;
    let month: u32 = date.get(4..6)?.parse().ok()?;
    let day: u32 = date.get(6..8)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // `TM` is optional in a worklist query; an absent time means midnight,
    // which can only make the row look OLDER - never newer than it is.
    let hhmmss = time.split('.').next().unwrap_or("");
    let (hour, minute, second) = match hhmmss.len() {
        6 => (
            hhmmss.get(0..2)?.parse::<u64>().ok()?,
            hhmmss.get(2..4)?.parse::<u64>().ok()?,
            hhmmss.get(4..6)?.parse::<u64>().ok()?,
        ),
        // A partial TM (`HH`, `HHMM`) is legal DICOM; fill the rest with zero.
        4 => (
            hhmmss.get(0..2)?.parse::<u64>().ok()?,
            hhmmss.get(2..4)?.parse::<u64>().ok()?,
            0,
        ),
        2 => (hhmmss.parse::<u64>().ok()?, 0, 0),
        0 => (0, 0, 0),
        _ => return None,
    };
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((days as u64) * SECS_PER_DAY + hour * 3600 + minute * 60 + second)
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
                    // Tagged so a RELEASE build keeps the line: MWL and MPPS
                    // deletions are exactly what an operator has to be able
                    // to account for later, and a bare INFO tag is dropped.
                    crate::clog_event!(
                        crate::clog::Level::Info,
                        "WORKFLOW_RETENTION",
                        "",
                        "mwl={} mpps={}",
                        mwl,
                        mpps
                    );
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

    #[test]
    fn scheduled_start_secs_reads_the_dicom_forms() {
        // Midnight on a known day, and the same day with a time.
        assert_eq!(scheduled_start_secs("19700101", ""), Some(0));
        assert_eq!(scheduled_start_secs("19700101", "000000"), Some(0));
        assert_eq!(scheduled_start_secs("19700102", "000000"), Some(86_400));
        assert_eq!(
            scheduled_start_secs("19700101", "013000"),
            Some(3600 + 1800)
        );
        // A fractional TM keeps the whole-second part.
        assert_eq!(
            scheduled_start_secs("19700101", "013000.500"),
            Some(3600 + 1800)
        );
        // Partial TM is legal DICOM (PS3.3 C.4.1) and pads with zero.
        assert_eq!(scheduled_start_secs("19700101", "01"), Some(3600));
        assert_eq!(scheduled_start_secs("19700101", "0130"), Some(3600 + 1800));
    }

    #[test]
    fn scheduled_start_secs_rejects_what_it_cannot_read() {
        assert_eq!(scheduled_start_secs("", ""), None);
        assert_eq!(scheduled_start_secs("1970-1-1", "000000"), None);
        assert_eq!(scheduled_start_secs("19701301", "000000"), None);
        assert_eq!(scheduled_start_secs("19700101", "250000"), None);
        assert_eq!(scheduled_start_secs("19700101", "xx"), None);
    }

    /// A worklist row scheduled in the FUTURE must outlive its filing age.
    ///
    /// The sweep aged every MWL row by `updated_at`/`created_at` alone, so an
    /// order imported today for a slot 45 days out was deleted after
    /// `retention_days` - gone exactly when the modality would query for it,
    /// and before the procedure even happened. Age is now measured from the
    /// later of the stamp and the scheduled start.
    ///
    /// `sweep_mwl` is exercised directly: it takes an explicit cutoff, so the
    /// test does not need to wait on the clock or touch `AppState`.
    #[test]
    fn a_future_scheduled_mwl_row_is_not_swept_before_its_procedure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = WorkflowStore::new(dir.path().to_path_buf());
        let now = now_secs();
        // Filed 60 days ago (older than a 7-day window) but scheduled 45 days
        // from now: the procedure has not happened yet.
        let filed = now.saturating_sub(60 * SECS_PER_DAY);
        let future = now + 45 * SECS_PER_DAY;
        let rec = clarus_core::workflow::SpsRecord {
            sps_uid: "1.2.840.9.5.1".to_string(),
            sps_start_date: format_stamp_date(future),
            sps_start_time: "090000".to_string(),
            modality: "CT".to_string(),
            scheduled_performing_physician: "SMITH^JOHN".to_string(),
            patient_name: "IVANOV^IVAN".to_string(),
            patient_id: "PAT-T1".to_string(),
            accession_number: Some("ACC-T1".to_string()),
            created_at: Some(format_stamp(filed)),
            updated_at: Some(format_stamp(filed)),
            ..Default::default()
        };
        store.put_sps(&rec).expect("seed future row");

        // A second row, filed long ago AND scheduled long ago: control.
        let past = now.saturating_sub(90 * SECS_PER_DAY);
        let old = clarus_core::workflow::SpsRecord {
            sps_uid: "1.2.840.9.5.2".to_string(),
            sps_start_date: format_stamp_date(past),
            sps_start_time: "090000".to_string(),
            patient_id: "PAT-T1-OLD".to_string(),
            created_at: Some(format_stamp(past)),
            updated_at: Some(format_stamp(past)),
            ..Default::default()
        };
        store.put_sps(&old).expect("seed old row");

        let cutoff = now.saturating_sub(7 * SECS_PER_DAY);
        let deleted = sweep_mwl(&store, cutoff);
        assert_eq!(deleted, 1, "only the genuinely old row is swept");

        let left = store.all_sps().expect("scan");
        assert_eq!(left.len(), 1, "the future row must survive");
        assert_eq!(left[0].sps_uid, "1.2.840.9.5.1");
    }

    /// Render epoch seconds as a `YYYY-MM-DD` `DA` value.
    fn format_stamp_date(secs: u64) -> String {
        let days = (secs / SECS_PER_DAY) as i64;
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}{m:02}{d:02}")
    }

    /// Render epoch seconds as the `YYYY-MM-DD HH:MM:SS` record stamp.
    fn format_stamp(secs: u64) -> String {
        let days = (secs / SECS_PER_DAY) as i64;
        let (y, m, d) = civil_from_days(days);
        let rem = secs % SECS_PER_DAY;
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    }

    /// The inverse of [`days_from_civil`] (Hinnant's algorithm).
    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }
}
