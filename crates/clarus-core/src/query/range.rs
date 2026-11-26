// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shared DA/TM range matching for the QIDO hot path.
//!
//! PS3.4 C.2.2.2.5 requires range matching for date and time attributes:
//! the value form is `<lower>-<upper>` with either side allowed absent and
//! both ends INCLUSIVE (standard/ps3.4/c-2.2.2.5-range-matching.json).
//! StudyTime (0008,0030, TM), PerformedProcedureStepStartDate (0040,0244, DA)
//! and PerformedProcedureStepStartTime (0040,0245, TM) are required matching
//! keys (PS3.18 Table 10.6.1-5), so they must accept the range form and
//! compare inclusively - the single source for that comparison is here, not
//! three ad-hoc copies.
//!
//! DA (`YYYYMMDD`) and TM (`HHMMSS[.ffffff]`) values never contain `-`, so a
//! single split on the separator is unambiguous. DT is different: its
//! UTC-offset form may itself carry a `-`, which is exactly why the DT range
//! splitter lives in `ups/store.rs` and does not share this helper.

use crate::manifest::AttrValue;

/// Whether a stored DA value falls inside `pat` (inclusive range, open ends).
///
/// The stored DA is a numeric `YYYYMMDD` `UInt` or an 8-digit `Text`; any
/// other form (or an absent value) does not match. Malformed bounds or a
/// malformed pattern do not match either - a bad key must never degrade into
/// "match everything".
pub fn da_range_matches(have: &AttrValue, pat: &str) -> bool {
    let Some((lo, hi)) = split_range(pat) else {
        return false;
    };
    let value = match have {
        AttrValue::UInt(u) => Some(*u),
        AttrValue::Text(s) if s.len() == 8 => s.parse::<u64>().ok(),
        _ => None,
    };
    let Some(value) = value else { return false };
    let Some(lo) = parse_da_bound(lo) else {
        return false;
    };
    let Some(hi) = parse_da_bound(hi) else {
        return false;
    };
    // `20260831-20260801` asks for a window whose lower end is above its upper
    // end, so no value can be inside it (PS3.4 C.2.2.2.5: both ends are
    // inclusive, which leaves a reversed window empty by definition). The
    // closed comparison below already yields false for it; stating the
    // rejection explicitly keeps the intent local, so a refactor to two
    // independent one-sided comparisons cannot turn a reversed range key into
    // a half-open match-all.
    if let (Some(lo), Some(hi)) = (lo, hi)
        && lo > hi
    {
        return false;
    }
    match (lo, hi) {
        (Some(lo), Some(hi)) => value >= lo && value <= hi,
        (Some(lo), None) => value >= lo,
        (None, Some(hi)) => value <= hi,
        // A bare `-` bounds nothing; the server rejects it before this module
        // runs, and treating it as "match all" would turn a degenerate key
        // into a full scan.
        (None, None) => false,
    }
}

/// Whether a stored TM value falls inside `pat` (inclusive range, open ends).
///
/// The stored TM is a `Text` in any legal precision - `HH`, `HHMM`,
/// `HHMMSS[.ffffff]` (PS3.5 Table 6.2-1: trailing null TM components denote a
/// period, standard/ps3.5/tm-vr.json). A shortened stored value matches when
/// its PERIOD intersects the range, so `StudyTime=1000` matches `0900-1100`
/// (PS3.4 C.2.2.2.5.4: "a Study Time of \"1000-1800\""). Any other form (or
/// an absent value) does not match. The comparison is over whole microseconds
/// so a fractional bound aligns with a whole-second value and vice versa.
pub fn tm_range_matches(have: &AttrValue, pat: &str) -> bool {
    let Some((lo, hi)) = split_range(pat) else {
        return false;
    };
    let value = match have {
        AttrValue::Text(s) => parse_tm_period(s),
        _ => None,
    };
    let Some((vs, ve)) = value else { return false };
    let Some(lo) = parse_tm_bound(lo) else {
        return false;
    };
    let Some(hi) = parse_tm_bound(hi) else {
        return false;
    };
    // A reversed window names no time at all (PS3.4 C.2.2.2.5.4 bounds are
    // inclusive, so `lo > hi` contains nothing). The intersection test below
    // already yields false for it because it demands BOTH sides; the explicit
    // rejection keeps the empty window stated rather than emergent.
    if let (Some((lo_start, _)), Some((_, hi_end))) = (lo, hi)
        && lo_start > hi_end
    {
        return false;
    }
    match (lo, hi) {
        (Some((lo_start, _)), Some((_, hi_end))) => ve >= lo_start && vs <= hi_end,
        (Some((lo_start, _)), None) => ve >= lo_start,
        (None, Some((_, hi_end))) => vs <= hi_end,
        // A bare `-` bounds nothing; the server rejects it before this module
        // runs, and treating it as "match all" would turn a degenerate key
        // into a full scan.
        (None, None) => false,
    }
}

/// Split a DA/TM range pattern into `(lo, hi)`, both borrowed.
///
/// A leading `-` is the open-lower form (`-hi`), an interior `-` a closed
/// range (`lo-hi`), and no `-` the exact form (`pat` == `[pat, pat]`). More
/// than one `-` is malformed: DA/TM values carry none, so a valid range has
/// exactly one separator. Both slices stay borrowed - no allocation.
fn split_range(pat: &str) -> Option<(&str, &str)> {
    match pat.find('-') {
        None => Some((pat, pat)),
        Some(i) => {
            // `i` is a byte index of an ASCII byte, so it is a char boundary.
            if pat[i + 1..].contains('-') {
                return None;
            }
            if i == 0 {
                Some(("", &pat[1..]))
            } else {
                Some((&pat[..i], &pat[i + 1..]))
            }
        }
    }
}

/// Parse one DA bound: `None` for an absent (open) end, `Some(d)` for a
/// canonical 8-digit `YYYYMMDD`. Anything else - including an eight-digit
/// value that is not a real calendar date - is malformed.
///
/// PS3.5 Section 6.2 defines DA as a fixed `YYYYMMDD` with the stated ranges
/// for month and day, so `20260231` is not a DA at all. It parses as a number
/// and would order correctly against other malformed values, which is exactly
/// how a typo silently widens a range instead of being refused.
fn parse_da_bound(s: &str) -> Option<Option<u64>> {
    if s.is_empty() {
        Some(None)
    } else if s.len() == 8 {
        let value = s.parse::<u64>().ok()?;
        if is_calendar_date(value) {
            Some(Some(value))
        } else {
            None
        }
    } else {
        None
    }
}

/// Whether `v` is a real `YYYYMMDD` calendar date.
///
/// The month/day lengths follow PS3.5 Table 6.2-1; February admits 29 days
/// unconditionally, because DA carries no time zone and no leap-second context
/// with which to reject it - refusing a real 29 February would be a worse
/// error than accepting the three non-leap years per century.
fn is_calendar_date(v: u64) -> bool {
    let year = v / 10_000;
    let month = (v / 100) % 100;
    let day = v % 100;
    if year < 1 || !(1..=12).contains(&month) {
        return false;
    }
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => 29,
    };
    (1..=max_day).contains(&day)
}

/// Parse one TM bound: `None` for an absent (open) end, `Some((start, end))`
/// for a valid TM period. Anything else is malformed.
///
/// A range's lower bound contributes its period START and its upper bound its
/// period END, so `0900-1100` reads "from 09:00:00 through 11:00:59.999999"
/// (PS3.4 C.2.2.2.5.4). The stored side is compared the same way: a stored
/// `1000` is the whole minute [10:00:00, 10:00:59.999999].
fn parse_tm_bound(s: &str) -> Option<Option<(u64, u64)>> {
    if s.is_empty() {
        Some(None)
    } else {
        parse_tm_period(s).map(Some)
    }
}

/// Parse a TM value into an inclusive `(start_micros, end_micros)` period.
///
/// PS3.5 Table 6.2-1: trailing null TM components denote a period, so a
/// shortened TM names a whole sub-division, not an instant
/// (standard/ps3.5/tm-vr.json):
/// - `HH`    -> the whole hour [H:00:00.000000, H:59:59.999999]
/// - `HHMM`  -> the whole minute [H:M:00.000000, H:M:59.999999]
/// - `HHMMSS` and `HHMMSS.ffffff` -> an instant (start == end); the fraction
///   is one to six digits and is scaled to six so `100000.5` == `100000.500000`
///   (a shorter fraction must not compare numerically smaller than the same
///   instant written with more digits).
///
/// DA does NOT reduce precision (PS3.5 6.2 DA row is a fixed `YYYYMMDD`, no
/// trailing-null-component clause), so [`parse_da_bound`] stays an 8-digit
/// parse and MUST NOT share this. Pure digit accumulation - no heap.
fn parse_tm_period(s: &str) -> Option<(u64, u64)> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    // HH needs two digits before `bytes[0]`/`bytes[1]` are read; every other
    // malformed length (3, 5, 7, a lone dot, longer than HHMMSS.ffffff) is
    // rejected by the `_` arm of the match below.
    if len < 2 {
        return None;
    }
    let h = {
        if !bytes[0].is_ascii_digit() || !bytes[1].is_ascii_digit() {
            return None;
        }
        (bytes[0] - b'0') as u64 * 10 + (bytes[1] - b'0') as u64
    };
    // PS3.5 Table 6.2-1 gives TM the ranges HH 00-23, MM 00-59, SS 00-59. A
    // component outside them is not a time at all, and the accumulation below
    // happily orders it ("2560" becomes 26:00, larger than every real time),
    // so a malformed bound would widen a range instead of being refused.
    if h > 23 {
        return None;
    }
    match len {
        2 => {
            let start = h * 3_600_000_000;
            Some((start, start + 3_599_999_999))
        }
        4 => {
            if !bytes[2].is_ascii_digit() || !bytes[3].is_ascii_digit() {
                return None;
            }
            let m = (bytes[2] - b'0') as u64 * 10 + (bytes[3] - b'0') as u64;
            if m > 59 {
                return None;
            }
            let start = (h * 3600 + m * 60) * 1_000_000;
            Some((start, start + 59_999_999))
        }
        6 => {
            if !bytes[2..6].iter().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let m = (bytes[2] - b'0') as u64 * 10 + (bytes[3] - b'0') as u64;
            let sec = (bytes[4] - b'0') as u64 * 10 + (bytes[5] - b'0') as u64;
            if m > 59 || sec > 59 {
                return None;
            }
            let micros = (h * 3600 + m * 60 + sec) * 1_000_000;
            Some((micros, micros))
        }
        8..=13 => {
            if bytes[6] != b'.' || !bytes[2..6].iter().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let m = (bytes[2] - b'0') as u64 * 10 + (bytes[3] - b'0') as u64;
            let sec = (bytes[4] - b'0') as u64 * 10 + (bytes[5] - b'0') as u64;
            if m > 59 || sec > 59 {
                return None;
            }
            let micros = (h * 3600 + m * 60 + sec) * 1_000_000;
            let mut frac: u64 = 0;
            let mut digits = 0u32;
            for &b in &bytes[7..] {
                if !b.is_ascii_digit() {
                    return None;
                }
                frac = frac * 10 + (b - b'0') as u64;
                digits += 1;
            }
            frac *= 10u64.pow(6 - digits);
            Some((micros + frac, micros + frac))
        }
        // len == 3, 5, 7 (a lone dot with no fraction) -> not a TM.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tm(v: &str) -> AttrValue {
        AttrValue::Text(v.to_string())
    }

    #[test]
    fn da_closed_range_is_inclusive_on_both_ends() {
        assert!(da_range_matches(
            &AttrValue::UInt(20260815),
            "20260801-20260831"
        ));
        assert!(da_range_matches(
            &AttrValue::UInt(20260801),
            "20260801-20260831"
        ));
        assert!(da_range_matches(
            &AttrValue::UInt(20260831),
            "20260801-20260831"
        ));
        assert!(!da_range_matches(
            &AttrValue::UInt(20260731),
            "20260801-20260831"
        ));
        assert!(!da_range_matches(
            &AttrValue::UInt(20260901),
            "20260801-20260831"
        ));
    }

    #[test]
    fn da_open_ends() {
        assert!(da_range_matches(&AttrValue::UInt(20260901), "20260801-"));
        assert!(da_range_matches(&AttrValue::UInt(20260701), "-20260801"));
        assert!(!da_range_matches(&AttrValue::UInt(20260701), "20260801-"));
        assert!(!da_range_matches(&AttrValue::UInt(20260901), "-20260801"));
    }

    #[test]
    fn da_exact_form_and_both_stored_forms() {
        // Exact `YYYYMMDD` is [v, v].
        assert!(da_range_matches(&AttrValue::UInt(20260815), "20260815"));
        assert!(da_range_matches(
            &AttrValue::Text("20260815".into()),
            "20260815"
        ));
        assert!(!da_range_matches(&AttrValue::UInt(20260816), "20260815"));
        // Text and UInt stored forms agree inside a range.
        assert!(da_range_matches(
            &AttrValue::Text("20260815".into()),
            "20260801-20260831"
        ));
    }

    #[test]
    fn da_malformed_pattern_or_absent_value_is_false() {
        // Non-numeric bounds.
        assert!(!da_range_matches(&AttrValue::UInt(20260815), "abc-def"));
        // Bounds that are not canonical 8-digit DA values.
        assert!(!da_range_matches(&AttrValue::UInt(20260815), "2026-2027"));
        // Two separators.
        assert!(!da_range_matches(
            &AttrValue::UInt(20260815),
            "20260801-20260815-x"
        ));
        // A stored value in neither DA form (Bytes) is absent for this key.
        assert!(!da_range_matches(&AttrValue::Bytes(vec![]), "20260801-"));
        // A bare `-` bounds nothing and must not match everything.
        assert!(!da_range_matches(&AttrValue::UInt(20260815), "-"));
    }

    #[test]
    fn tm_closed_range_is_inclusive_with_fraction() {
        assert!(tm_range_matches(&tm("100000"), "090000-110000"));
        assert!(tm_range_matches(&tm("090000"), "090000-110000"));
        assert!(tm_range_matches(&tm("110000"), "090000-110000"));
        assert!(!tm_range_matches(&tm("085959"), "090000-110000"));
        assert!(!tm_range_matches(&tm("110001"), "090000-110000"));
        // A whole-second bound aligns with a fractional value.
        assert!(tm_range_matches(&tm("100000.5"), "090000-110000"));
    }

    #[test]
    fn tm_open_ends_and_exact() {
        assert!(tm_range_matches(&tm("120000"), "090000-"));
        assert!(tm_range_matches(&tm("080000"), "-090000"));
        assert!(tm_range_matches(&tm("100000"), "100000"));
        assert!(!tm_range_matches(&tm("100001"), "100000"));
    }

    #[test]
    fn tm_fraction_forms_compare_by_microseconds() {
        // `100000.5` == 100000.500000: equal instants, not `500000` vs `5`.
        assert!(tm_range_matches(&tm("100000.500000"), "100000.5"));
        assert!(tm_range_matches(&tm("100000.5"), "100000.5-100000.6"));
        assert!(!tm_range_matches(&tm("100000.7"), "100000.5-100000.6"));
    }

    #[test]
    fn tm_reduced_precision_periods_match() {
        // PS3.4 C.2.2.2.5.4: a StudyTime range "1000-1800" is legal. PS3.5
        // Table 6.2-1: trailing null TM components denote a period, so a HHMM
        // bound names the whole minute it starts: "0900-1100" spans
        // [09:00:00.000000, 11:00:59.999999].
        assert!(tm_range_matches(&tm("100000"), "0900-1100"));
        // A stored reduced-precision value matches when its period intersects.
        assert!(tm_range_matches(&tm("1000"), "0900-1100"));
        // The upper HHMM bound's period end (11:00:59.999999) is inclusive.
        assert!(tm_range_matches(&tm("110059"), "0900-1100"));
        assert!(!tm_range_matches(&tm("110100"), "0900-1100"));
        assert!(!tm_range_matches(&tm("120000"), "0900-1100"));
        // 11:59:59 is OUTSIDE: HHMM "1100" names the minute 11:00, not the
        // whole hour 11.
        assert!(!tm_range_matches(&tm("115959"), "0900-1100"));
        // A single HHMM value names a whole minute: "1000" is
        // [10:00:00.000000, 10:00:59.999999].
        assert!(tm_range_matches(&tm("100030"), "1000"));
        assert!(!tm_range_matches(&tm("100100"), "1000"));
        // Same period stored and bound.
        assert!(tm_range_matches(&tm("1000"), "1000"));
        // A whole-hour (HH) bound names the whole hour.
        assert!(tm_range_matches(&tm("105959"), "10"));
        assert!(!tm_range_matches(&tm("110000"), "10"));
    }

    #[test]
    fn tm_malformed_pattern_or_absent_value_is_false() {
        // A HH bound is a valid whole-hour period, so "09-11" names
        // [09:00:00, 11:59:59.999999] and stored 10:00:00 MATCHES.
        assert!(tm_range_matches(&tm("100000"), "09-11"));
        // Two separators.
        assert!(!tm_range_matches(&tm("100000"), "090000-110000-x"));
        // A non-numeric fraction.
        assert!(!tm_range_matches(&tm("100000"), "090000.abc-"));
        // A stored UInt is not a TM (TM is always text).
        assert!(!tm_range_matches(&AttrValue::UInt(100000), "090000-110000"));
        // A bare `-` bounds nothing and must not match everything.
        assert!(!tm_range_matches(&tm("100000"), "-"));
    }

    /// A reversed DA window asks for nothing. No test can distinguish this
    /// guard from its absence: the closed form below is `>= lo && <= hi`, and
    /// `lo > hi` makes that predicate unsatisfiable, so the explicit check
    /// restates what the arithmetic already yields. Kept as a documented
    /// rejection so a future refactor to two independent one-sided
    /// comparisons cannot reintroduce the half-open reading.
    #[test]
    fn a_reversed_da_range_matches_nothing() {
        assert!(!da_range_matches(
            &AttrValue::UInt(20261215),
            "20260901-20260801"
        ));
        assert!(!da_range_matches(
            &AttrValue::UInt(20260101),
            "20260901-20260801"
        ));
        assert!(!da_range_matches(
            &AttrValue::UInt(20260815),
            "20260901-20260801"
        ));
        // The forward form still works, so the guard rejects only the inversion.
        assert!(da_range_matches(
            &AttrValue::UInt(20260815),
            "20260801-20260901"
        ));
    }

    /// A reversed TM window asks for nothing. As on the DA side the two-sided
    /// intersection already rejects it, so this pins the behaviour a future
    /// refactor must preserve rather than distinguishing the guard from its
    /// absence.
    #[test]
    fn a_reversed_tm_range_matches_nothing() {
        assert!(!tm_range_matches(&tm("100000"), "1100-0900"));
        assert!(!tm_range_matches(&tm("120000"), "1100-0900"));
        assert!(!tm_range_matches(&tm("083000"), "1100-0900"));
        assert!(!tm_range_matches(&tm("095959"), "1100-0900"));
        assert!(tm_range_matches(&tm("100000"), "0900-1100"));
    }

    /// PS3.5 Table 6.2-1 allows only real calendar dates; `20260231` parses as
    /// a number but is not a DA, and accepting it let a typo widen a range.
    #[test]
    fn an_impossible_da_bound_is_malformed() {
        assert!(!da_range_matches(&AttrValue::UInt(20260215), "20260231-"));
        assert!(!da_range_matches(&AttrValue::UInt(20260215), "-20260231"));
        assert!(!da_range_matches(&AttrValue::UInt(20260431), "20260431"));
        assert!(!da_range_matches(&AttrValue::UInt(20261301), "20261301"));
        // Real dates in the same months still parse.
        assert!(da_range_matches(&AttrValue::UInt(20260228), "20260228"));
        assert!(da_range_matches(&AttrValue::UInt(20260430), "20260430"));
    }

    /// PS3.5 Table 6.2-1 caps HH at 23, MM and SS at 59. An out-of-range
    /// component accumulates into a time no legal value can precede, so used as
    /// an upper bound it silently widened the window instead of being refused.
    #[test]
    fn an_out_of_range_tm_component_is_malformed() {
        // The upper bound is the observable side: "0000-2400" reads as
        // "through 24:00", which every legal stored time satisfies.
        assert!(!tm_range_matches(&tm("120000"), "0000-2400"));
        assert!(!tm_range_matches(&tm("100000"), "0000-0970"));
        assert!(!tm_range_matches(&tm("100000"), "0000-100070"));
        assert!(!tm_range_matches(&tm("100000"), "0000-100061.5"));
        // The legal extremes still parse.
        assert!(tm_range_matches(&tm("235959"), "23-"));
        assert!(tm_range_matches(&tm("100000"), "1000"));
    }
}
