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
    // `1..=31` is not a calendar: February 31 and April 31 pass it, and
    // `days_from_civil` then normalises them into the following month. A DA
    // that names no day (PS3.5 Table 6.2) cannot be interpreted, so the pair
    // is refused and the caller falls back to the record stamp.
    if !valid_date(year, month, day) {
        return None;
    }
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
    // Same pre-epoch hazard as `parse_stamp_secs`: a negative day count cast
    // to `u64` becomes a date far in the future, which would make an old row
    // permanently un-expirable. A pre-1970 schedule is expired by definition.
    let days = days.max(0) as u64;
    Some(days * SECS_PER_DAY + hour * 3600 + minute * 60 + second)
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
        // Only `updated_at` dates the FINAL transition. Falling back to
        // `created_at` measured the step from when it was STARTED, which for a
        // procedure that ran for a week is arbitrarily older than the moment
        // the record became final - so a still-recent final report could be
        // deleted the instant it was written. A final record with no
        // `updated_at` is left alone: the age of the transition is unknown,
        // and guessing it is the one thing retention must not do.
        //
        // (MWL keeps its creation-time fallback below: a worklist item's age
        // does not depend on a later transition.)
        let Some(updated) = rec.updated_at.as_deref() else {
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
///
/// The shape is checked EXACTLY, not merely tokenised. `str::parse` accepts
/// a leading `+`, leading zeros and any magnitude, and a range check of
/// `1..=12`/`1..=31` accepts a day that the month cannot have - so
/// `2026-10-11 25:00:00` used to normalise into the NEXT DAY and
/// `2026-02-31 00:00:00` into March, both of which then compared as a real
/// instant and could expire a record whose timestamp the policy says must be
/// treated as malformed. Ten `char`s per field sum to the only digits allowed.
fn parse_stamp_secs(stamp: &str) -> Option<u64> {
    let (date, time) = stamp.split_once(' ')?;
    let mut d = date.split('-');
    let year: i64 = exact_num(d.next()?, 4)?.parse().ok()?;
    let month: u32 = exact_num(d.next()?, 2)?.parse().ok()?;
    let day: u32 = exact_num(d.next()?, 2)?.parse().ok()?;
    // A fourth component (`2026-10-11-x`) is not this format.
    if d.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let hour: u64 = exact_num(t.next()?, 2)?.parse().ok()?;
    let minute: u64 = exact_num(t.next()?, 2)?.parse().ok()?;
    let second: u64 = exact_num(t.next()?, 2)?.parse().ok()?;
    if t.next().is_some() {
        return None;
    }
    if !valid_date(year, month, day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // A pre-epoch day count is NEGATIVE (`1969-12-31` is -1), and `as u64`
    // would turn it into a date ~584 billion years in the future - so every
    // pre-1970 record read as "not yet expired" and was never deleted.
    // Retention only ever compares against a cutoff AFTER the epoch, so a
    // pre-epoch stamp is genuinely expired: clamp to 0 rather than wrap.
    let days = days.max(0) as u64;
    Some(days * SECS_PER_DAY + hour * 3600 + minute * 60 + second)
}

/// `value` if it is exactly `len` ASCII digits, else `None`.
fn exact_num(value: &str, len: usize) -> Option<&str> {
    (value.len() == len && value.bytes().all(|b| b.is_ascii_digit())).then_some(value)
}

/// Whether `(year, month, day)` names a day that exists on the calendar.
///
/// PS3.5 Table 6.2 types DA as `YYYYMMDD` with a real day-of-month; a
/// February 31 is not a date, so a stamp carrying one cannot be interpreted.
fn valid_date(year: i64, month: u32, day: u32) -> bool {
    if !(1..=12).contains(&month) || day == 0 {
        return false;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    day <= last
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

    /// An out-of-range clock component must be REFUSED, not normalised into
    /// the next day. `2026-10-11 25:00:00` parsed to `2026-10-12 01:00:00`,
    /// which then compared as a real instant and could expire a record whose
    /// timestamp the policy says must be treated as malformed.
    #[test]
    fn an_impossible_clock_or_calendar_date_is_refused() {
        for bad in [
            "2026-10-11 25:00:00",
            "2026-10-11 23:60:00",
            "2026-10-11 23:00:60",
            // Days that no month has.
            "2026-02-31 00:00:00",
            "2026-04-31 00:00:00",
            "2026-06-31 00:00:00",
            // 2026 is not a leap year, so there is no 29 February.
            "2026-02-29 00:00:00",
            // A day of zero.
            "2026-10-00 00:00:00",
            // Non-numeric or wrongly padded fields.
            "2026-1-11 00:00:00",
            "2026-10-11T00:00:00",
            "2026-10-11 00:00:00junk",
        ] {
            assert_eq!(parse_stamp_secs(bad), None, "{bad} must be refused");
        }
        // A real leap day in a leap year is still a date.
        assert!(parse_stamp_secs("2024-02-29 12:00:00").is_some());
    }

    /// A PRE-EPOCH stamp must not wrap into the far future.
    ///
    /// `days_from_civil` returns a negative count for any date before
    /// 1970-01-01, and `as u64` turned that into a date roughly 584 billion
    /// years ahead. Every such record compared as "not yet expired" and was
    /// never deleted - the exact opposite of what an old timestamp means.
    #[test]
    fn a_pre_epoch_stamp_is_treated_as_expired_not_as_the_future() {
        // The DAY count clamps to the epoch; the time of day is kept, so the
        // value stays in the first day rather than wrapping to the far future.
        assert_eq!(parse_stamp_secs("1969-12-31 00:00:00"), Some(0));
        assert_eq!(parse_stamp_secs("1969-12-31 23:59:59"), Some(86_399));
        assert_eq!(parse_stamp_secs("1960-01-01 00:00:00"), Some(0));
        // One second AFTER the epoch is still a real, small value.
        assert_eq!(parse_stamp_secs("1970-01-01 00:00:01"), Some(1));
        // The property that matters: no pre-epoch stamp can land past the
        // epoch, which is what the old `as u64` cast produced.
        for stamp in ["1969-12-31 00:00:00", "1950-06-15 12:00:00"] {
            assert!(parse_stamp_secs(stamp).expect("parsed") < SECS_PER_DAY);
        }
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
        // A DA that names no day on the calendar: `1..=31` accepted it and
        // `days_from_civil` then normalised it into the next month.
        assert_eq!(scheduled_start_secs("20260231", "000000"), None);
        assert_eq!(scheduled_start_secs("20260431", "000000"), None);
        // 2026 is not a leap year.
        assert_eq!(scheduled_start_secs("20260229", "000000"), None);
        // A real leap day is a date.
        assert!(scheduled_start_secs("20240229", "000000").is_some());
    }

    /// A pre-epoch DA must not wrap into the far future on the MWL path
    /// either - the same `as u64` cast lived in both converters.
    #[test]
    fn a_pre_epoch_schedule_is_expired_not_far_future() {
        assert_eq!(scheduled_start_secs("19691231", "000000"), Some(0));
        assert_eq!(scheduled_start_secs("19700101", "000000"), Some(0));
        assert_eq!(scheduled_start_secs("19700102", "000000"), Some(86_400));
    }

    /// A final MPPS record with no `updated_at` must NOT be expired from
    /// `created_at`.
    ///
    /// `created_at` dates when the step STARTED, not when it became final. A
    /// long procedure (or one whose N-SET left the write stamp empty) could
    /// therefore be deleted the instant its final report was written, because
    /// the age measured from the start already exceeded the window. The age of
    /// the FINAL transition is unknown in that case, and retention must not
    /// guess - so the record is kept.
    #[test]
    fn a_final_mpps_without_an_update_stamp_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = WorkflowStore::new(dir.path().to_path_buf());
        let now = now_secs();
        let ancient = format_stamp(now.saturating_sub(365 * SECS_PER_DAY));

        let rec = clarus_core::workflow::MppsRecord {
            mpps_uid: "1.2.840.9.6.1".to_string(),
            status: "COMPLETED".to_string(),
            created_at: Some(ancient),
            updated_at: None,
            ..Default::default()
        };
        store.put_mpps(&rec).expect("seed final record");

        // A cutoff far newer than `created_at`: the old rule would delete it.
        let cutoff = now.saturating_sub(7 * SECS_PER_DAY);
        assert_eq!(
            sweep_mpps(&store, cutoff),
            0,
            "an unknown final-transition age must not be guessed from created_at"
        );
        assert!(
            store.find_mpps("1.2.840.9.6.1").expect("read").is_some(),
            "the record survives"
        );

        // With a real `updated_at` inside the window it IS swept, so the
        // guard above is about the missing stamp, not a blanket exemption.
        let mut aged = rec.clone();
        aged.updated_at = Some(format_stamp(now.saturating_sub(30 * SECS_PER_DAY)));
        store.put_mpps(&aged).expect("update stamp");
        assert_eq!(sweep_mpps(&store, cutoff), 1, "a known age is swept");
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
