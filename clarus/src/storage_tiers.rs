// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Storage tier migration schedule - nginx-style cron-driven configuration.
//!
//! Three independent migration policies, one per backend:
//! 1. **Cold (NAS/HDD)** - fast, local file-system tier.
//! 2. **S3 (cloud)** - cheap, remote object storage.
//! 3. **Emergency** - disk-full protection, ignores age.
//!
//! # Design principles (nginx-way)
//! - Admin reads cron, not Rust enums. `0 2 * * *` means "every night at 02:00".
//! - Invalid cron -> server refuses to start with a clear message.
//! - Zero external crates. 80 lines of parser, 40 lines of `next_after`.

#![forbid(unsafe_code)]

use chrono::{Datelike, Timelike};

// -- Five-field cron parser ----------------------------------------------

/// Parsed five-field cron expression (minute hour day-of-month month day-of-week).
///
/// Supports:
/// - `*` - any value
/// - `*/N` - every N units (step)
/// - `N` - exact value
/// - `N-M` - inclusive range
/// - `N,M,O` - comma-separated list
///
/// Examples:
/// - `0 2 * * *` - every day at 02:00
/// - `0 */6 * * *` - every 6 hours
/// - `*/15 * * * *` - every 15 minutes
/// - `0 2 * * 1-5` - weekdays at 02:00
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: Vec<u8>,
    hours: Vec<u8>,
    dom: Vec<u8>,
    month: Vec<u8>,
    dow: Vec<u8>,
}

impl CronSchedule {
    /// Parse a five-field cron expression.
    ///
    /// Returns `Err(description)` if the expression is invalid.
    /// The error message is suitable for logging at server startup.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron expression must have 5 fields, got {}: '{}'",
                fields.len(),
                expr
            ));
        }

        let minutes = parse_field(fields[0], 0, 59)?;
        let hours = parse_field(fields[1], 0, 23)?;
        let dom = parse_field(fields[2], 1, 31)?;
        let month = parse_field(fields[3], 1, 12)?;
        let dow = parse_field(fields[4], 0, 6)?;

        Ok(Self {
            minutes,
            hours,
            dom,
            month,
            dow,
        })
    }

    /// Check whether the given local datetime matches this schedule.
    pub fn matches(&self, dt: &chrono::DateTime<chrono::Local>) -> bool {
        let min = dt.minute() as u8;
        let hour = dt.hour() as u8;
        let day = dt.day() as u8;
        let mon = dt.month() as u8;
        // chrono: 0 = Sun, 1 = Mon, ..., 6 = Sat - matches our 0..6.
        let wday = dt.weekday().num_days_from_sunday() as u8;

        self.minutes.contains(&min)
            && self.hours.contains(&hour)
            && self.dom.contains(&day)
            && self.month.contains(&mon)
            && self.dow.contains(&wday)
    }

    /// Find the next matching datetime after `after` (local server time).
    ///
    /// Searches minute-by-minute, up to 2 years ahead.
    /// Returns `None` if no match is found (should never happen in practice).
    pub fn next_after(
        &self,
        after: &chrono::DateTime<chrono::Local>,
    ) -> Option<chrono::DateTime<chrono::Local>> {
        let mut candidate = *after + chrono::Duration::minutes(1);
        // Truncate to minute boundary
        candidate = candidate
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(candidate);

        // Search up to 2 years (~ 1M minutes) - cron always repeats.
        for _ in 0..1_051_200 {
            if self.matches(&candidate) {
                return Some(candidate);
            }
            candidate += chrono::Duration::minutes(1);
        }
        None
    }
}

impl std::fmt::Display for CronSchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {} {} {}",
            field_to_string(&self.minutes, 0, 59),
            field_to_string(&self.hours, 0, 23),
            field_to_string(&self.dom, 1, 31),
            field_to_string(&self.month, 1, 12),
            field_to_string(&self.dow, 0, 6),
        )
    }
}

// -- Cron field parsing --------------------------------------------------

/// Parse a single cron field into a sorted, deduplicated list of allowed values.
fn parse_field(field: &str, min: u8, max: u8) -> Result<Vec<u8>, String> {
    let mut values: Vec<u8> = Vec::new();

    // Handle step syntax: */5 or 1-10/2
    let (base, step) = if let Some(slash) = field.find('/') {
        let step_str = &field[slash + 1..];
        let step: u8 = step_str
            .parse()
            .map_err(|_| format!("invalid step '{}' in '{}'", step_str, field))?;
        if step == 0 {
            return Err(format!("step must be > 0 in '{}'", field));
        }
        (&field[..slash], step)
    } else {
        (field, 1u8)
    };

    if base == "*" {
        // */N or just *
        let mut v = min;
        while v <= max {
            values.push(v);
            v = v.saturating_add(step);
        }
    } else {
        // Parse comma-separated parts
        for part in base.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!("empty value in '{}'", field));
            }
            if let Some(dash) = part.find('-') {
                let lo: u8 = part[..dash]
                    .parse()
                    .map_err(|_| format!("invalid range start in '{}'", part))?;
                let hi: u8 = part[dash + 1..]
                    .parse()
                    .map_err(|_| format!("invalid range end in '{}'", part))?;
                if lo < min || hi > max || lo > hi {
                    return Err(format!(
                        "range {}-{} out of bounds [{}, {}]",
                        lo, hi, min, max
                    ));
                }
                let mut v = lo;
                while v <= hi {
                    values.push(v);
                    v = v.saturating_add(step);
                }
            } else {
                let v: u8 = part
                    .parse()
                    .map_err(|_| format!("invalid value '{}' in '{}'", part, field))?;
                if v < min || v > max {
                    return Err(format!(
                        "value {} out of bounds [{}, {}] in '{}'",
                        v, min, max, field
                    ));
                }
                values.push(v);
            }
        }
    }

    if values.is_empty() {
        return Err(format!("no valid values in '{}'", field));
    }

    values.sort();
    values.dedup();
    Ok(values)
}

/// Reconstruct a cron field string (for Display).
///
/// `*/N` means "every Nth starting at `min`", so the shorthand is only
/// faithful when the first value IS `min`. `[2,14]` has a step of 12 and used
/// to render `*/12`, which reads back as `[0,12]` - a different schedule than
/// the one that was configured. Anything starting elsewhere keeps the explicit
/// list.
fn field_to_string(vals: &[u8], min: u8, max: u8) -> String {
    let full: Vec<u8> = (min..=max).collect();
    if vals == full {
        return "*".to_string();
    }
    // `*/N` reads back as the WHOLE `min..=max` stepped by N, so the shorthand
    // is only faithful when `vals` is exactly that expansion. Comparing
    // against a `take(vals.len())` prefix accepted `[0,2,4,6,8,10]` (hours)
    // as `*/2`, which re-parses to 12 values - a different schedule than the
    // one configured.
    if vals.len() >= 2 {
        let step = vals[1] - vals[0];
        if step > 1 && vals[0] == min {
            let expected: Vec<u8> = (min..=max).step_by(step as usize).collect();
            if expected == vals {
                return format!("*/{}", step);
            }
        }
    }
    vals.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

// -- Backend schedule ----------------------------------------------------

/// Migration schedule for a single storage backend (cold or S3).
#[derive(Debug, Clone)]
pub struct BackendSchedule {
    /// Cron schedule for planned migration. `None` = migration disabled.
    pub schedule: Option<CronSchedule>,
    /// Minimum study age in days before migration eligibility.
    /// Studies younger than this are never migrated.
    pub min_age_days: u32,
}

impl BackendSchedule {
    /// Create a disabled backend schedule.
    pub fn disabled() -> Self {
        Self {
            schedule: None,
            min_age_days: 90,
        }
    }

    /// Whether planned migration is enabled for this backend.
    pub fn is_enabled(&self) -> bool {
        self.schedule.is_some() && self.min_age_days > 0
    }
}

// -- Migration schedule (full) -------------------------------------------

/// Complete migration schedule, parsed from `[storage_tiers]`.
///
/// Three independent policies:
/// - **cold** - local NAS/HDD cold tier
/// - **s3** - cloud object storage
/// - **emergency** - disk-full protection
#[derive(Debug, Clone)]
pub struct MigrationSchedule {
    /// Emergency migration: free space threshold in GB. 0 = disabled.
    pub emergency_free_gb: u32,
    /// How often to check disk space (seconds). Range: [60, 3600].
    pub emergency_check_secs: u64,
    /// Local cold storage schedule. `None` = not configured.
    pub cold: Option<BackendSchedule>,
    /// S3 cloud storage schedule. `None` = not configured.
    #[cfg(feature = "s3")]
    pub s3: Option<BackendSchedule>,
}

impl MigrationSchedule {
    /// Create a fully disabled schedule (no migration at all).
    pub fn disabled() -> Self {
        Self {
            emergency_free_gb: 0,
            emergency_check_secs: 300,
            cold: None,
            #[cfg(feature = "s3")]
            s3: None,
        }
    }

    /// Whether ANY migration policy is active.
    pub fn is_any_enabled(&self) -> bool {
        self.emergency_free_gb > 0
            || self.cold.as_ref().map(|b| b.is_enabled()).unwrap_or(false)
            || {
                #[cfg(feature = "s3")]
                {
                    self.s3.as_ref().map(|b| b.is_enabled()).unwrap_or(false)
                }
                #[cfg(not(feature = "s3"))]
                false
            }
    }
}

// -- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Cron parsing --------------------------------------------------

    #[test]
    fn parse_basic_expressions() {
        let c = CronSchedule::parse("0 2 * * *").unwrap();
        assert_eq!(c.minutes, vec![0]);
        assert_eq!(c.hours, vec![2]);
        assert_eq!(c.dom, (1..=31).collect::<Vec<_>>());
        assert_eq!(c.month, (1..=12).collect::<Vec<_>>());
        assert_eq!(c.dow, (0..=6).collect::<Vec<_>>());
    }

    #[test]
    fn parse_every_6_hours() {
        let c = CronSchedule::parse("0 */6 * * *").unwrap();
        assert_eq!(c.minutes, vec![0]);
        assert_eq!(c.hours, vec![0, 6, 12, 18]);
    }

    #[test]
    fn parse_every_15_minutes() {
        let c = CronSchedule::parse("*/15 * * * *").unwrap();
        assert_eq!(c.minutes, vec![0, 15, 30, 45]);
    }

    /// Display must round-trip the schedule, not merely the shape of the run.
    ///
    /// `*/N` is "every N starting at min", so a run that starts elsewhere has
    /// no shorthand: `[2,14]` rendered `*/12`, which parses back to `[0,12]`.
    #[test]
    fn a_step_that_does_not_start_at_min_is_not_rendered_as_a_star_step() {
        assert_eq!(field_to_string(&[2, 14], 0, 59), "2,14");
        assert_eq!(field_to_string(&[1, 3, 5, 7, 9], 0, 59), "1,3,5,7,9");
        // A run that DOES start at min keeps the shorthand.
        assert_eq!(field_to_string(&[0, 12, 24, 36, 48], 0, 59), "*/12");
        assert_eq!(field_to_string(&[0, 6, 12, 18], 0, 23), "*/6");
        // Full range is still `*`.
        assert_eq!(field_to_string(&(0..=59).collect::<Vec<_>>(), 0, 59), "*");
    }

    /// The rendered string must parse back to the same value set - the
    /// property the shorthand above exists for.
    /// A truncated run that starts at `min` must NOT take the shorthand: the
    /// shorthand means the whole `min..=max` stepped by N, so rendering
    /// `[0,2,4,6,8,10]` (hours) as `*/2` re-parses to twelve values.
    #[test]
    fn a_truncated_run_is_not_rendered_as_a_star_step() {
        assert_eq!(field_to_string(&[0, 2, 4, 6, 8, 10], 0, 23), "0,2,4,6,8,10");
        // The full expansion keeps the shorthand.
        assert_eq!(
            field_to_string(&(0..=23).step_by(2).collect::<Vec<_>>(), 0, 23),
            "*/2"
        );
    }

    #[test]
    fn a_rendered_field_parses_back_to_the_same_values() {
        for vals in [
            vec![2u8, 14],
            vec![1, 3, 5, 7, 9],
            vec![0, 12, 24, 36, 48],
            vec![30],
        ] {
            let rendered = field_to_string(&vals, 0, 59);
            let spec = format!("{rendered} * * * *");
            let parsed = CronSchedule::parse(&spec)
                .unwrap_or_else(|e| panic!("the rendered field must parse back ({rendered}): {e}"));
            assert_eq!(
                parsed.minutes, vals,
                "round trip changed the schedule: {rendered}"
            );
        }
    }

    #[test]
    fn parse_step_minutes_stay_in_bounds() {
        // "*/15 2 * * *": minute step values never exceed the 0..59 range.
        let c = CronSchedule::parse("*/15 2 * * *").unwrap();
        assert_eq!(c.minutes, vec![0, 15, 30, 45]);
        assert_eq!(c.hours, vec![2]);
    }

    #[test]
    fn parse_range_step() {
        // "0 1-10/2 * * *": range-with-step -> 1,3,5,7,9 (odd hours).
        let c = CronSchedule::parse("0 1-10/2 * * *").unwrap();
        assert_eq!(c.hours, vec![1, 3, 5, 7, 9]);
    }

    #[test]
    fn parse_list_with_dow_range() {
        // "0 2,14 * * 1-5": hour list + weekday range.
        let c = CronSchedule::parse("0 2,14 * * 1-5").unwrap();
        assert_eq!(c.hours, vec![2, 14]);
        assert_eq!(c.dow, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_range() {
        let c = CronSchedule::parse("0 2 * * 1-5").unwrap();
        assert_eq!(c.dow, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_list() {
        let c = CronSchedule::parse("0 2,8,16 * * *").unwrap();
        assert_eq!(c.hours, vec![2, 8, 16]);
    }

    #[test]
    fn parse_combined_range_and_list() {
        let c = CronSchedule::parse("0 2-4,8-10 * * *").unwrap();
        assert_eq!(c.hours, vec![2, 3, 4, 8, 9, 10]);
    }

    #[test]
    fn reject_invalid_field_count() {
        assert!(CronSchedule::parse("0 2 * *").is_err());
        assert!(CronSchedule::parse("0 2 * * * *").is_err());
        assert!(CronSchedule::parse("").is_err());
    }

    #[test]
    fn reject_out_of_range() {
        assert!(CronSchedule::parse("60 * * * *").is_err());
        assert!(CronSchedule::parse("* 24 * * *").is_err());
        assert!(CronSchedule::parse("* * 0 * *").is_err());
        assert!(CronSchedule::parse("* * 32 * *").is_err());
        assert!(CronSchedule::parse("* * * 0 *").is_err());
        assert!(CronSchedule::parse("* * * * 7").is_err());
    }

    #[test]
    fn reject_invalid_syntax() {
        assert!(CronSchedule::parse("abc * * * *").is_err());
        assert!(CronSchedule::parse("* * * * abc").is_err());
        assert!(CronSchedule::parse("*/0 * * * *").is_err());
    }

    #[test]
    fn reject_hour_out_of_bounds_message() {
        let err = CronSchedule::parse("0 70 * * *").unwrap_err();
        assert!(
            err.contains("out of bounds"),
            "expected 'out of bounds' in error, got: {}",
            err
        );
    }

    #[test]
    fn reject_zero_step_message() {
        let err = CronSchedule::parse("*/0 * * * *").unwrap_err();
        assert!(
            err.contains("step must be > 0"),
            "expected 'step must be > 0' in error, got: {}",
            err
        );
    }

    #[test]
    fn reject_empty_list_element() {
        assert!(CronSchedule::parse("0 1,,2 * * *").is_err());
        // A trailing comma leaves an empty element too.
        assert!(CronSchedule::parse("1,2, * * * *").is_err());
    }

    #[test]
    fn roundtrip_display() {
        let original = "0 2 * * 1-5";
        let c = CronSchedule::parse(original).unwrap();
        let displayed = c.to_string();
        let c2 = CronSchedule::parse(&displayed).unwrap();
        assert_eq!(c, c2);
    }

    #[test]
    fn roundtrip_star() {
        let original = "* * * * *";
        let c = CronSchedule::parse(original).unwrap();
        assert_eq!(c.to_string(), "* * * * *");
    }

    #[test]
    fn roundtrip_step() {
        let c = CronSchedule::parse("*/15 * * * *").unwrap();
        assert_eq!(c.to_string(), "*/15 * * * *");
    }

    #[test]
    fn roundtrip_display_simple_exact() {
        // Simple expression round-trips to the exact input string.
        let original = "0 2 * * *";
        let c = CronSchedule::parse(original).unwrap();
        assert_eq!(c.to_string(), original);
    }

    #[test]
    fn display_step_and_list_observed() {
        // `*/N` means "every N starting at `min`", so only a run that STARTS at
        // `min` has that shorthand. A run starting elsewhere keeps its explicit
        // list: rendering `2,14` as `*/12` would read back as `0,12` - a
        // different schedule than the one configured (see
        // `a_rendered_field_parses_back_to_the_same_values`).
        let c = CronSchedule::parse("0 1-10/2 * * *").unwrap();
        assert_eq!(
            c.to_string(),
            "0 1,3,5,7,9 * * *",
            "1-10/2 starts at 1, so it has no `*/N` form"
        );

        let c = CronSchedule::parse("0 0-10/2 * * *").unwrap();
        assert_eq!(
            c.to_string(),
            "0 0,2,4,6,8,10 * * *",
            "0-10/2 is a TRUNCATED run: `*/2` would read back as 0,2,..,22"
        );

        let c = CronSchedule::parse("0 2,14 * * 1-5").unwrap();
        assert_eq!(c.to_string(), "0 2,14 * * 1,2,3,4,5");
    }

    // -- Cron matching -------------------------------------------------

    fn dt(ymd: &str, hm: &str) -> chrono::DateTime<chrono::Local> {
        let s = format!("{}T{}:00", ymd, hm);
        chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S")
            .unwrap()
            .and_local_timezone(chrono::Local)
            .single()
            .unwrap()
    }

    #[test]
    fn matches_every_day_at_2am() {
        let c = CronSchedule::parse("0 2 * * *").unwrap();
        assert!(c.matches(&dt("2026-01-15", "02:00")));
        assert!(!c.matches(&dt("2026-01-15", "02:01")));
        assert!(!c.matches(&dt("2026-01-15", "01:00")));
        // Works on any day
        assert!(c.matches(&dt("2026-07-21", "02:00")));
    }

    #[test]
    fn matches_weekdays_only() {
        let c = CronSchedule::parse("0 2 * * 1-5").unwrap();
        // Monday = 1
        assert!(c.matches(&dt("2026-01-19", "02:00"))); // Monday
        assert!(!c.matches(&dt("2026-01-17", "02:00"))); // Saturday
        assert!(!c.matches(&dt("2026-01-18", "02:00"))); // Sunday
    }

    #[test]
    fn dom_dow_strict_and_contract() {
        use chrono::Datelike;

        // dom AND dow is a STRICT conjunction (Vixie-OR is NOT implemented).
        // Contract: "0 0 13 * 5" fires ONLY on Friday the 13th.
        let c = CronSchedule::parse("0 0 13 * 5").unwrap();

        // Calendar facts are verified with chrono, not guessed.
        let friday13 = dt("2026-02-13", "00:00");
        let friday20 = dt("2026-02-20", "00:00");
        let thu13 = dt("2026-08-13", "00:00");
        let fri14 = dt("2026-08-14", "00:00");
        let friday12 = dt("2026-06-12", "00:00");
        let saturday13 = dt("2026-06-13", "00:00");
        let march13 = dt("2026-03-13", "00:00");

        assert_eq!(
            friday13.weekday(),
            chrono::Weekday::Fri,
            "2026-02-13 = Friday"
        );
        assert_eq!(
            friday20.weekday(),
            chrono::Weekday::Fri,
            "2026-02-20 = Friday"
        );
        assert_eq!(
            thu13.weekday(),
            chrono::Weekday::Thu,
            "2026-08-13 = Thursday"
        );
        assert_eq!(fri14.weekday(), chrono::Weekday::Fri, "2026-08-14 = Friday");
        assert_eq!(
            friday12.weekday(),
            chrono::Weekday::Fri,
            "2026-06-12 = Friday"
        );
        assert_eq!(
            saturday13.weekday(),
            chrono::Weekday::Sat,
            "2026-06-13 = Saturday"
        );
        assert_eq!(
            march13.weekday(),
            chrono::Weekday::Fri,
            "2026-03-13 = Friday"
        );

        // Fires for Friday the 13th (both dom and dow match).
        assert!(c.matches(&friday13));
        assert!(c.matches(&march13));
        // Does NOT fire for a Friday that is NOT the 13th (same month as a
        // Friday the 13th, 2026-02): dow=Friday but dom != 13.
        assert!(!c.matches(&friday20));
        // Does NOT fire for Friday the 12th (2026-06-12): dow=Friday but dom != 13.
        assert!(!c.matches(&friday12));
        // Does NOT fire for the 13th that is NOT a Friday (Thursday 2026-08-13).
        assert!(!c.matches(&thu13));
        // Does NOT fire for the 13th on a Saturday (2026-06-13).
        assert!(!c.matches(&saturday13));
    }

    #[test]
    fn matches_every_6_hours() {
        let c = CronSchedule::parse("0 */6 * * *").unwrap();
        assert!(c.matches(&dt("2026-01-15", "00:00")));
        assert!(c.matches(&dt("2026-01-15", "06:00")));
        assert!(c.matches(&dt("2026-01-15", "12:00")));
        assert!(c.matches(&dt("2026-01-15", "18:00")));
        assert!(!c.matches(&dt("2026-01-15", "03:00")));
    }

    // -- next_after ----------------------------------------------------

    #[test]
    fn next_after_basic() {
        let c = CronSchedule::parse("0 2 * * *").unwrap();
        let now = dt("2026-01-15", "01:30");
        let next = c.next_after(&now).unwrap();
        assert_eq!(next, dt("2026-01-15", "02:00"));
    }

    #[test]
    fn next_after_wraps_to_next_day() {
        let c = CronSchedule::parse("0 2 * * *").unwrap();
        let now = dt("2026-01-15", "03:00");
        let next = c.next_after(&now).unwrap();
        assert_eq!(next, dt("2026-01-16", "02:00"));
    }

    #[test]
    fn next_after_minute_boundary_from_half_past() {
        // From 02:30, "0 2 * * *" fires tomorrow at 02:00 (minute boundary).
        let c = CronSchedule::parse("0 2 * * *").unwrap();
        let now = dt("2026-01-15", "02:30");
        let next = c.next_after(&now).unwrap();
        assert_eq!(next, dt("2026-01-16", "02:00"));
    }

    #[test]
    fn next_after_every_15_min() {
        let c = CronSchedule::parse("*/15 * * * *").unwrap();
        let now = dt("2026-01-15", "00:03");
        let next = c.next_after(&now).unwrap();
        assert_eq!(next, dt("2026-01-15", "00:15"));
    }

    #[test]
    fn next_after_exact_match_moves_forward() {
        let c = CronSchedule::parse("0 2 * * *").unwrap();
        let now = dt("2026-01-15", "02:00");
        let next = c.next_after(&now).unwrap();
        // Should be next day, not same minute
        assert_eq!(next, dt("2026-01-16", "02:00"));
    }

    // -- BackendSchedule -----------------------------------------------

    #[test]
    fn disabled_backend() {
        let b = BackendSchedule::disabled();
        assert!(!b.is_enabled());
    }

    #[test]
    fn enabled_backend() {
        let b = BackendSchedule {
            schedule: Some(CronSchedule::parse("0 2 * * *").unwrap()),
            min_age_days: 90,
        };
        assert!(b.is_enabled());
    }

    #[test]
    fn backend_disabled_when_schedule_is_none() {
        let b = BackendSchedule {
            schedule: None,
            min_age_days: 90,
        };
        assert!(!b.is_enabled());
    }

    #[test]
    fn backend_disabled_when_min_age_is_zero() {
        let b = BackendSchedule {
            schedule: Some(CronSchedule::parse("0 2 * * *").unwrap()),
            min_age_days: 0,
        };
        // min_age_days = 0 means "never migrate", even with a schedule
        assert!(!b.is_enabled());
    }

    // -- MigrationSchedule --------------------------------------------

    #[test]
    fn fully_disabled_schedule() {
        let ms = MigrationSchedule::disabled();
        assert!(!ms.is_any_enabled());
    }

    #[test]
    fn emergency_only_enabled() {
        let ms = MigrationSchedule {
            emergency_free_gb: 50,
            emergency_check_secs: 300,
            cold: None,
            #[cfg(feature = "s3")]
            s3: None,
        };
        assert!(ms.is_any_enabled());
    }
}
