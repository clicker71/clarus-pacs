// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Local-time + civil-date primitives.
//!
//! The **only** place that pulls wall-clock time out of the OS for the "now"
//! and render use cases (`clog` / `log_writer` prefixes). Replaces `chrono`
//! for those sites. Calendar *arithmetic* (cron -> `next_after`) stays on
//! `chrono` in the server - it is not in this module's scope.
//!
//! ## Policies
//! **No cache** - every [`local_now`] call goes to the OS (reading the
//! "second-per-second"), stack only, 0 allocations in the conversion.
//!
//! `libc::localtime_r` / `libc::localtime_s` are called from the audited
//! OS boundary [`crate::ffi::localtime_civil`] (not a hand-rolled
//! `struct tm` re-declaration - a custom `tm` struct is not used).
//!
//! [`civil_from_days`] is the single canonical Hinnant day->civil converter;
//! the copies in `migration_worker.rs` and `journal.rs` are re-homed here.
//!
//! DICOM DA/TM parsing stays with `dicom_rs` - untouched.

use std::time::{SystemTime, UNIX_EPOCH};

/// A broken-down local time (wall clock), populated from the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// Calendar year (civil, e.g. 2026).
    pub year: i32,
    /// Month, 1..=12.
    pub month: u32,
    /// Day of month, 1..=31.
    pub day: u32,
    /// Hour, 0..=23.
    pub hour: u32,
    /// Minute, 0..=59.
    pub minute: u32,
    /// Second, 0..=60 (leap second tolerated).
    pub second: u32,
    /// Millisecond, 0..=999.
    pub millis: u32,
}

/// Read the current local time from the OS.
///
/// Millisecond precision comes from `SystemTime`'s sub-second fraction; the
/// broken-down fields come from `libc::localtime_r` (Unix) / `libc::localtime_s`
/// (Windows). Zero heap: only the stack `tm` + the return struct.
pub fn local_now() -> LocalTime {
    // Unwrap-or-default: before the epoch a saturated 0 is safer than a panic.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.subsec_millis();

    let epoch_secs = now.as_secs() as libc::time_t;
    // The OS clock is the source of truth for local wall time.
    let (year, month, day, hour, minute, second) = local_broken_down(epoch_secs);

    LocalTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millis,
    }
}

/// Fill a `libc::tm` from the OS for a given epoch offset, returning the
/// broken-down civil fields. The OS call itself lives in
/// [`crate::ffi::localtime_civil`] (audited OS boundary); here we only
/// translate the populated `tm` into civil fields, falling back to the
/// epoch on conversion failure.
fn local_broken_down(epoch_secs: libc::time_t) -> (i32, u32, u32, u32, u32, u32) {
    match crate::ffi::localtime_civil(epoch_secs) {
        Some(tm) => tm_to_civil(tm),
        // Fall back to the epoch (1970-01-01 00:00:00) rather than panic.
        None => (1970, 1, 1, 0, 0, 0),
    }
}

/// Translate the OS `libc::tm` fields into civil (1-based month/day, real
/// year). `tm_year` = years since 1900, `tm_mon` = 0-based month.
#[cfg(any(unix, windows))]
fn tm_to_civil(tm: libc::tm) -> (i32, u32, u32, u32, u32, u32) {
    let year = tm.tm_year + 1900; // i32, already the return type
    let month = (tm.tm_mon + 1).max(0) as u32;
    let day = tm.tm_mday.max(0) as u32;
    let hour = tm.tm_hour.max(0) as u32;
    let minute = tm.tm_min.max(0) as u32;
    let second = tm.tm_sec.max(0) as u32;
    (year, month, day, hour, minute, second)
}

impl LocalTime {
    /// `%Y-%m-%d`, e.g. `2026-08-21`. Byte-identical to the chrono render.
    pub fn fmt_ymd(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `%Y/%m/%d %H:%M:%S`, e.g. `2026/08/21 12:34:56`. Byte-identical to the
    /// chrono render used by `clog`.
    pub fn fmt_datetime(&self) -> String {
        format!(
            "{:04}/{:02}/{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Convert days since 1970-01-01 to `(year, month, day)` (civil, 1-based).
///
/// Howard Hinnant's `civil_from_days`, public domain. The canonical copy:
/// `migration_worker.rs` and `journal.rs` no longer carry their own.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_epoch_is_1970_01_01() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_known_dates() {
        // Exact day counts from 1970-01-01 (verified).
        assert_eq!(civil_from_days(18_321), (2020, 2, 29));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(20_686), (2026, 8, 21));
    }

    #[test]
    fn civil_negative_days_before_epoch() {
        // -1 day = 1969-12-31.
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // -365 days (1970 is not a leap year) = 1969-01-01.
        assert_eq!(civil_from_days(-365), (1969, 1, 1));
    }

    #[test]
    fn civil_leap_year_bounds() {
        // 2024 is a leap year: 19781 = 2024-02-28, then 02-29, then 03-01.
        assert_eq!(civil_from_days(19_781), (2024, 2, 28));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
    }

    #[test]
    fn fmt_renders_zero_padded() {
        let t = LocalTime {
            year: 2001,
            month: 2,
            day: 3,
            hour: 4,
            minute: 5,
            second: 6,
            millis: 7,
        };
        assert_eq!(t.fmt_ymd(), "2001-02-03");
        assert_eq!(t.fmt_datetime(), "2001/02/03 04:05:06");
    }

    #[test]
    fn local_now_has_sane_range() {
        let t = local_now();
        // Year within a plausible modern window and fields in valid ranges.
        assert!((2020..=2100).contains(&t.year));
        assert!((1..=12).contains(&t.month));
        assert!((1..=31).contains(&t.day));
        assert!(t.hour <= 23);
        assert!(t.minute <= 59);
        assert!(t.second <= 60);
        assert!(t.millis <= 999);
    }
}
