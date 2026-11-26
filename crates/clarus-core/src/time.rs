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
pub fn local_now() -> Result<LocalTime, TimeError> {
    // A clock that reads before the epoch is not a time we can render: the
    // old `unwrap_or_default()` turned it into `0`, which renders as
    // 1970-01-01 00:00:00 - a real-looking timestamp on a log line. The
    // caller decides whether that is worth reporting or skipping.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| TimeError::ClockBeforeEpoch(e.duration().as_secs()))?;
    let millis = now.subsec_millis();

    // `as_secs()` is u64; `libc::time_t` is i64 on both supported targets, so
    // the conversion cannot trap, but a value above `i64::MAX` would wrap to a
    // negative instant. Reject it instead of rendering a silent 1901 date.
    let epoch_secs =
        libc::time_t::try_from(now.as_secs()).map_err(|_| TimeError::OutOfRange(now.as_secs()))?;
    // The OS clock is the source of truth for local wall time.
    let (year, month, day, hour, minute, second) = local_broken_down(epoch_secs)?;

    Ok(LocalTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millis,
    })
}

/// Break down an epoch offset into local wall-clock fields.
///
/// The journal stamps an event when it is queued and renders it later, so the
/// conversion has to accept a stored instant rather than reading the clock
/// again. Sub-second precision is not recoverable from an epoch second and is
/// reported as 0.
///
/// FALLIBLE, because the OS conversion can genuinely fail: Windows
/// `localtime_s` rejects a negative `time_t` outright (MSVC documents
/// `EINVAL` for out-of-range input), and no `time_t` is guaranteed to be
/// representable by a civil date. The previous signature returned an ordinary
/// `LocalTime` built from `(1970, 1, 1, 0, 0, 0)` on failure, so a corrupt or
/// negative journal timestamp rendered as 1970-01-01 - indistinguishable from
/// a real event at the epoch. The caller now decides what an unconvertible
/// instant means.
pub fn from_epoch_secs(epoch_secs: i64) -> Result<LocalTime, TimeError> {
    // `libc::time_t` is i64 on both supported targets, but the narrowing must
    // stay explicit: a 32-bit `time_t` target would truncate silently.
    let tt = libc::time_t::try_from(epoch_secs)
        .map_err(|_| TimeError::OutOfRange(epoch_secs.unsigned_abs()))?;
    let (year, month, day, hour, minute, second) = local_broken_down(tt)?;
    Ok(LocalTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millis: 0,
    })
}

/// A time that the OS could not convert into civil fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeError {
    /// The requested instant is not representable as a `libc::time_t`.
    OutOfRange(u64),
    /// The system clock reads before 1970-01-01.
    ClockBeforeEpoch(u64),
    /// The OS rejected the conversion (`localtime_r` returned NULL, or
    /// `localtime_s` returned non-zero).
    ConversionFailed,
}

impl std::fmt::Display for TimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimeError::OutOfRange(s) => write!(f, "epoch second {s} is out of range for time_t"),
            TimeError::ClockBeforeEpoch(s) => {
                write!(f, "system clock is {s}s before the epoch")
            }
            TimeError::ConversionFailed => write!(f, "OS local-time conversion failed"),
        }
    }
}

impl std::error::Error for TimeError {}

/// Fill a `libc::tm` from the OS for a given epoch offset, returning the
/// broken-down civil fields. The OS call itself lives in
/// [`crate::ffi::localtime_civil`] (audited OS boundary); here we only
/// translate the populated `tm` into civil fields.
fn local_broken_down(
    epoch_secs: libc::time_t,
) -> Result<(i32, u32, u32, u32, u32, u32), TimeError> {
    match crate::ffi::localtime_civil(epoch_secs) {
        Some(tm) => Ok(tm_to_civil(tm)),
        // A failed conversion is NOT the epoch: reporting it as such made an
        // unrenderable instant look like a valid event at 1970-01-01.
        None => Err(TimeError::ConversionFailed),
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

/// Convert `(year, month, day)` (civil, 1-based) to days since 1970-01-01.
///
/// Howard Hinnant's `days_from_civil`, the inverse of [`civil_from_days`].
/// Needed wherever a calendar date has to be compared as an INSTANT rather
/// than as text - a DT value with a UTC offset is one such case, because the
/// same instant has a different spelling in every zone.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_epoch_is_1970_01_01() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        for days in [-400, -1, 0, 1, 10_957, 18_321, 19_782, 20_686] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "round trip for {days}");
        }
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
        let t = local_now().expect("the clock is after the epoch on a sane host");
        // Year within a plausible modern window and fields in valid ranges.
        assert!((2020..=2100).contains(&t.year));
        assert!((1..=12).contains(&t.month));
        assert!((1..=31).contains(&t.day));
        assert!(t.hour <= 23);
        assert!(t.minute <= 59);
        assert!(t.second <= 60);
        assert!(t.millis <= 999);
    }

    /// A conversion failure must be an ERROR, never a 1970 timestamp.
    ///
    /// Windows `localtime_s` rejects a negative `time_t` with `EINVAL`, and
    /// glibc's `localtime_r` returns NULL for an unrepresentable instant too,
    /// so `-1` reaches the failure branch on both. The old signature had no
    /// error channel at all and returned `(1970, 1, 1, 0, 0, 0)`, which renders
    /// as a perfectly plausible log line.
    ///
    /// The `Ok` arm is kept for a libc that DOES convert `-1` (it would be
    /// 1969-12-31): that answer is legitimate, but 1970-01-01 00:00:00 is the
    /// swallowed failure this test forbids, so both arms are asserted.
    #[test]
    fn a_negative_instant_is_never_silently_the_epoch() {
        match from_epoch_secs(-1) {
            Err(e) => {
                assert!(
                    matches!(e, TimeError::ConversionFailed | TimeError::OutOfRange(_)),
                    "unexpected error variant: {e:?}"
                );
            }
            Ok(t) => {
                assert_ne!(
                    (t.year, t.month, t.day, t.hour, t.minute, t.second),
                    (1970, 1, 1, 0, 0, 0),
                    "a failed conversion was reported as the epoch"
                );
            }
        }
    }

    /// A normal instant still converts, so the fallible signature did not
    /// turn the happy path into an error.
    #[test]
    fn a_known_instant_converts() {
        // 2021-01-01T00:00:00Z. Asserting the DATE only, because the result is
        // local wall time and the hour depends on the host timezone.
        let t = from_epoch_secs(1_609_459_200).expect("2021-01-01 must convert");
        assert!(
            t.year == 2020 || t.year == 2021,
            "expected 2020-12-31 or 2021-01-01 local, got {t:?}"
        );
        assert!((1..=12).contains(&t.month));
        assert!((1..=31).contains(&t.day));
    }

    /// An extreme instant never panics and never comes back as a plausible
    /// modern date.
    ///
    /// `libc::time_t` is i64 on both supported targets, so `i64::MAX` fits and
    /// the OS conversion is what fails - the point of the check is that the
    /// caller is TOLD, not that a particular variant is returned. On a 32-bit
    /// `time_t` target the narrowing would itself produce `OutOfRange`.
    #[test]
    fn an_extreme_instant_is_reported_not_rendered() {
        match from_epoch_secs(i64::MAX) {
            Err(_) => {}
            Ok(t) => {
                assert!(
                    t.year > 2100,
                    "an extreme instant must not render as a modern date: {t:?}"
                );
            }
        }
        // The closing panic-free contract: this call must simply return.
        let _ = from_epoch_secs(i64::MIN);
    }
}
