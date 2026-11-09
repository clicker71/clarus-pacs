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

/// Print an ASCII progress line with a bar. Stateless - call from any context.
pub fn progress(tier: &str, phase: &str, done: u64, total: u64, elapsed_s: u64, detail: &str) {
    let w = 20usize;
    let filled = (w as u64)
        .checked_mul(done)
        .and_then(|v| v.checked_div(total.max(1)))
        .unwrap_or(0)
        .min(w as u64) as usize;
    let bar = "#".repeat(filled) + &".".repeat(w.saturating_sub(filled));
    let pct = done
        .checked_mul(100)
        .and_then(|v| v.checked_div(total.max(1)))
        .unwrap_or(100) as u8;
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
    // `\r` returns carriage to column 0. Intermediate updates overwrite the
    // same line (`eprint!`, no newline); the final DONE line ends with `\n`.
    if status == " DONE" {
        eprintln!("\r{line}");
    } else {
        eprint!("\r{line}");
    }
}

/// Print a plain status line (no bar). For warnings and simple notices.
pub fn progress_plain(tier: &str, msg: &str) {
    eprintln!("[{tier}] {msg}");
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
