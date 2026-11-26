// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ASCII progress helpers - extracted from the removed `recovery` module
//! for reuse in CLI-style console output.
//!
//! Stateless and thread-safe: no shared state, only `eprint!`/`eprintln!`.
//! `\r` carriage return overwrites the same line on POSIX terminals;
//! Windows cmd.exe does not convert `\n` to `\r\n` on stderr, so the final
//! DONE line uses `eprintln!` and intermediate updates use `eprint!`.

#![forbid(unsafe_code)]

use std::time::Duration;

/// Format a `Duration` as a human-readable string.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m {}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

/// The bar width, in characters.
const BAR_WIDTH: usize = 20;

/// Percentage and filled-bar width for a progress reading, both derived from
/// the SAME clamped ratio.
///
/// Clamping `done` to `total` first is what makes the two agree. `done = 3,
/// total = 1` used to fill the whole bar while `300 as u8` truncated to 44%;
/// at `done = total = u64::MAX` the `checked_mul(100)` overflow fell back to
/// `100` while the bar fallback produced 0 - an empty bar reading "100% DONE".
/// A `u128` intermediate keeps the multiply exact for every `u64` pair, and a
/// zero total is an explicit answer rather than a division-by-zero fallback.
fn bar_and_percent(done: u64, total: u64) -> (usize, u8) {
    if total == 0 {
        // Nothing to measure: no fill, and 0% rather than a claimed 100%.
        return (0, 0);
    }
    let done = done.min(total);
    let ratio = (done as u128) * 100 / (total as u128);
    let filled = ((done as u128) * (BAR_WIDTH as u128) / (total as u128)) as usize;
    // `ratio <= 100` because `done <= total`, so the cast cannot truncate.
    (filled.min(BAR_WIDTH), ratio as u8)
}

/// Print an ASCII progress line with a bar. Stateless - call from any context.
pub fn progress(tier: &str, phase: &str, done: u64, total: u64, elapsed_s: u64, detail: &str) {
    let (filled, pct) = bar_and_percent(done, total);
    let bar = "#".repeat(filled) + &".".repeat(BAR_WIDTH.saturating_sub(filled));
    let status = if done >= total && total > 0 {
        " DONE"
    } else {
        ""
    };
    let m = elapsed_s / 60;
    let s = elapsed_s % 60;
    let elapsed_str = if m > 0 {
        format!("{}m{}s", m, s)
    } else {
        format!("{}s", s)
    };
    let line = format!("[{tier}] {phase:>24} [{bar}] {pct:>3}%  {detail} ({elapsed_str}){status}");
    // `\r` returns carriage to column 0 but does NOT erase what was already
    // there, so a shorter update left the tail of the longer one visible - and
    // the final DONE line inherited that stale suffix. `\x1b[K` clears to end
    // of line after the carriage return. It is emitted only when the stream is
    // a terminal, so a redirected log stays free of escape sequences.
    let live = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let clear = if live { "\x1b[K" } else { "" };
    // `\r` returns carriage to column 0. Intermediate updates overwrite the
    // same line (`eprint!`, no newline); the final DONE line ends with `\n`.
    if status == " DONE" {
        eprintln!("\r{line}{clear}");
    } else {
        eprint!("\r{line}{clear}");
    }
}

/// Print a plain status line (no bar). For warnings and simple notices.
///
/// Starts on a FRESH line: appending it to an in-progress `progress` update
/// would merge a notice into a half-drawn bar.
pub fn progress_plain(tier: &str, msg: &str) {
    eprintln!("\n[{tier}] {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_secs() {
        assert_eq!(format_duration(Duration::from_secs(3)), "3s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(135)), "2m 15s");
        assert_eq!(format_duration(Duration::from_secs(2712)), "45m 12s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 1m 1s");
    }

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
    }
}
