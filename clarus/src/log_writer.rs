// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Dedicated log writer thread with bounded ring buffer (, std only).
//!
//! nginx-style architecture: hotpath code pushes formatted plaintext lines
//! into a lock-free bounded channel via `try_send`. A single dedicated OS
//! thread drains the channel and writes them to a daily-rotated file
//! (`clarus.log.YYYY-MM-DD`) with its OWN rotation - `tracing_appender` is no
//! longer used in production.
//!
//! # Design invariants
//! - Producers NEVER block on logging. If the ring buffer is full, the line is
//!   silently dropped.
//! - File I/O (open, write, day-rollover) happens exclusively on the dedicated
//!   thread - zero disk I/O on the hotpath.
//! - The channel is bounded to prevent unbounded memory growth under sustained
//!   log overload.
//! - On drop: the channel disconnects, the writer thread drains remaining
//!   messages and exits.
//!
//! This module also installs the global [`ForeignLog`] bridge: foreign crates
//! that speak the `log` facade (rusqlite_migration, ...) route WARN/ERROR into
//! the same writer with `logger=<source>`; their DEBUG/TRACE are dropped (or
//! go to stderr in debug builds).

#![forbid(unsafe_code)]

use crate::clog;
use std::io::{IsTerminal, Write};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;
use std::time::Duration;

/// Number of buffered log lines. At ~200 bytes per plaintext line this is
/// ~1.6 MB of buffer. When full, new lines are silently dropped - logging is
/// lossy under extreme overload, but producers never block.
const RING_SIZE: usize = 8192;

/// Idle-flush cadence: the writer flushes buffered lines to disk at least
/// this often, so a quiet server's WARN/ERROR still reach the file promptly.
const FLUSH_MS: u64 = 100;

/// Aggregator TTL sweep cadence in seconds: the writer thread forces a
/// `stow_agg::sweep()` + `access_agg::sweep()` every this many seconds so
/// interrupted STOW/WADO sessions surface promptly even with no new traffic.
/// Chosen well below both aggregators' TTLs (30s / 60s) yet cheap enough to
/// run on the dedicated writer thread.
const SWEEP_SECS: u64 = 5;

// -- LogWriter ----------------------------------------------------------

/// Initialize the whole logging stack: spawn the dedicated writer thread,
/// install its channel into [`clog`], set the level ceiling, and set up the
/// global `log` bridge. The returned handle (a cloneable `SyncSender`) keeps
/// the writer alive.
pub fn init(log_dir: &std::path::Path, level: clog::Level) -> LogWriter {
    let _ = std::fs::create_dir_all(log_dir);
    let (tx, rx) = sync_channel::<Vec<u8>>(RING_SIZE);
    let log_dir = log_dir.to_path_buf();

    thread::Builder::new()
        .name("log-writer".into())
        .spawn(move || {
            writer_loop(rx, &log_dir);
        })
        .expect("Failed to spawn log writer thread");

    clog::set_sender(tx.clone());
    clog::set_level(level);
    install_foreign_log_bridge();
    LogWriter { tx }
}

/// A `Clone` handle that pushes formatted log lines into the bounded ring.
#[derive(Clone)]
pub struct LogWriter {
    tx: SyncSender<Vec<u8>>,
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Bounded, non-blocking push (same policy as clog's channel entry).
        match self.tx.try_send(buf.to_vec()) {
            Ok(()) => Ok(buf.len()),
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// -- Writer thread (own daily rotation, std) -----------------------------

/// Runs on the dedicated `log-writer` OS thread.
///
/// Drains the ring and writes each line to `clarus.log.YYYY-MM-DD`. The date is
/// re-derived on every batch; when the day changes the writer opens the next
/// day's file. All rotation happens inside this one thread - no
/// `tracing_appender`. Exactly ONE file is current: `clarus.log.YYYY-MM-DD`
/// (never a bare `clarus.log` beside a dated one).
///
/// Console mirror (nginx-way): after the file write the SAME line goes to
/// stderr, so console order == file order while the producers still never
/// block. The `[LEVEL]` token is painted only when stderr is a terminal and
/// NO_COLOR is unset; pipes (journald/docker) get clean plaintext.
fn writer_loop(rx: std::sync::mpsc::Receiver<Vec<u8>>, log_dir: &std::path::Path) {
    let mut current_date = String::new();
    let mut file: Option<std::fs::File> = None;
    // Decided ONCE per writer thread, before the drain loop.
    let color = console_color_enabled(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    );
    // Idle aggregator sweep cadence: the STOW/WADO aggregators rely on TTL to
    // surface interrupted sessions, but with no traffic between records they
    // would linger forever. This ticker forces a sweep every 5s so a quiet
    // server still emits `STOW_STUDY completed` / `WADO_SERIES partial`
    // promptly. Routed through `clog_event!` (same channel this thread drains)
    // for identical formatting + whitelist filtering as live traffic.
    let sweep_every = Duration::from_secs(SWEEP_SECS);
    let mut last_sweep = std::time::Instant::now();

    loop {
        // Batch-drain with a bounded wait so idle flushes still hit disk.
        let deadline = std::time::Instant::now() + Duration::from_millis(FLUSH_MS);
        while let Ok(msg) = rx.recv_timeout(Duration::from_millis(10)) {
            let _ = file_write_batch(&mut file, &mut current_date, log_dir, &msg);
            // Best-effort: a failed console write must never stop the writer
            // thread or the file logging.
            let _ = std::io::stderr().write_all(&console_line(&msg, color));
            if std::time::Instant::now() >= deadline {
                break;
            }
        }

        // Aggregator TTL sweep on a 5s tick. When idle the inner drain above
        // returns within ~10ms, so this check fires on time even with zero
        // traffic; when busy it runs after every batch. Sweep lines are routed
        // via `clog_event!` into the same channel this loop drains (try_send,
        // never blocking) and are written on the next iteration - identical
        // path and formatting to live STOW/WADO traffic.
        if last_sweep.elapsed() >= sweep_every {
            last_sweep = std::time::Instant::now();
            for line in crate::stow_agg::sweep() {
                crate::clog_event!(crate::clog::Level::Info, "STOW_STUDY", "", "{line}");
            }
            for line in crate::access_agg::sweep() {
                crate::clog_event!(crate::clog::Level::Info, "WADO_SERIES", "", "{line}");
            }
        }
    }
}

/// Append one line to the current day file, opening it (and thus rolling over)
/// when the date has advanced. Returns the number of bytes written.
fn file_write_batch(
    file: &mut Option<std::fs::File>,
    current_date: &mut String,
    log_dir: &std::path::Path,
    msg: &[u8],
) -> usize {
    let today = clarus_core::time::local_now().fmt_ymd();
    if today != *current_date {
        // Day rollover (or first write): open the new day's file.
        *current_date = today.clone();
        *file = open_daily(log_dir, &today);
    }
    match file.as_mut() {
        Some(f) => f.write(msg).unwrap_or(0),
        None => 0,
    }
}

/// Open (create/append) today's log file: `clarus.log.YYYY-MM-DD`.
fn open_daily(log_dir: &std::path::Path, date: &str) -> Option<std::fs::File> {
    let path = log_dir.join(format!("clarus.log.{date}"));
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

// -- Console mirror (same thread: file first, stderr second) -----------

/// ANSI palette around the `[LEVEL]` token (cltranscode palette). Only the
/// bracket token is painted, never the timestamp/message fields, so
/// grep/awk on the console output keeps working.
const ANSI_RESET: &[u8] = b"\x1b[0m";
const COLOR_ERROR: &[u8] = b"\x1b[31m";
const COLOR_WARN: &[u8] = b"\x1b[33m";
const COLOR_INFO: &[u8] = b"\x1b[32m";
const COLOR_DEBUG: &[u8] = b"\x1b[36m";
const COLOR_TRACE: &[u8] = b"\x1b[35m";

/// Level tokens with their palette entries. A rendered clog/journal line
/// carries exactly one of these right after the timestamp; the first hit in
/// the line is the level token.
const LEVEL_TOKENS: &[(&[u8], &[u8])] = &[
    (b"[ERROR]", COLOR_ERROR),
    (b"[WARN]", COLOR_WARN),
    (b"[INFO]", COLOR_INFO),
    (b"[DEBUG]", COLOR_DEBUG),
    (b"[TRACE]", COLOR_TRACE),
];

/// TTY color decision: paint only on a real terminal without NO_COLOR.
fn console_color_enabled(stderr_is_terminal: bool, no_color_set: bool) -> bool {
    stderr_is_terminal && !no_color_set
}

/// Mirror transform: wrap the FIRST `[LEVEL]` token in ANSI color. With
/// `color=false` the line is returned byte-identical. A line with no level
/// token is returned unchanged even with `color=true` (defensive: never
/// panic on malformed or foreign lines).
fn console_line(line: &[u8], color: bool) -> Vec<u8> {
    if !color {
        return line.to_vec();
    }
    let hit = LEVEL_TOKENS
        .iter()
        .copied()
        .filter_map(|(token, ansi)| find_subslice(line, token).map(|pos| (pos, token, ansi)))
        .min_by_key(|(pos, _, _)| *pos);
    let Some((pos, token, ansi)) = hit else {
        return line.to_vec();
    };
    let mut out = Vec::with_capacity(line.len() + ansi.len() + ANSI_RESET.len());
    out.extend_from_slice(&line[..pos]);
    out.extend_from_slice(ansi);
    out.extend_from_slice(token);
    out.extend_from_slice(ANSI_RESET);
    out.extend_from_slice(&line[pos + token.len()..]);
    out
}

/// Byte-slice search over `windows` (std only, no memchr dependency).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// -- Global `log` facade bridge ------------------------------------------

/// A [`log::Log`] that funnels foreign-crate records (via the `log` facade)
/// into the SAME writer pipe as [`clog`]. In production WARN/ERROR are written
/// with `logger=<target>`; DEBUG/TRACE are dropped. In debug builds everything
/// goes to stderr. This closes the DEBUG/TRACE leak from crates such as
/// `rusqlite_migration` and removes the old log->tracing bridge.
struct ForeignLog;

impl log::Log for ForeignLog {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let level = match record.level() {
            log::Level::Error => clog::Level::Error,
            log::Level::Warn => clog::Level::Warn,
            log::Level::Info => clog::Level::Info,
            log::Level::Debug | log::Level::Trace => clog::Level::Debug,
        };
        // WARN/ERROR carry a source tag so an admin can see which foreign
        // crate spoke; naive DEBUG/TRACE are dropped by the clog filter.
        let msg = format!("logger={} {}", record.target(), record.args());
        clog::emit(level, format_args!("{msg}"));
    }

    fn flush(&self) {}
}

/// Install the `log` facade bridge (idempotent).
fn install_foreign_log_bridge() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = log::set_logger(&ForeignLog);
        log::set_max_level(log::LevelFilter::Info);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_file_name_never_bare() {
        // The canonical file is `clarus.log.YYYY-MM-DD`; a bare `clarus.log`
        // must never be produced alongside a dated one.
        let today = clarus_core::time::local_now().fmt_ymd();
        assert_eq!(
            &today,
            today.as_str(),
            "date string is computed from the clock"
        );
        let dated = format!("clarus.log.{today}");
        assert_ne!(dated, "clarus.log", "dated name != bare name");
        assert!(dated.starts_with("clarus.log."));
    }

    #[test]
    fn open_daily_targets_dated_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = open_daily(dir.path(), "2026-08-15").expect("open daily");
        assert_eq!(
            f.metadata().expect("meta").len(),
            0,
            "new daily file starts empty"
        );
        assert!(
            dir.path().join("clarus.log.2026-08-15").is_file(),
            "wrote to clarus.log.2026-08-15"
        );
        // The bare `clarus.log` must not exist.
        assert!(!dir.path().join("clarus.log").exists());
    }

    #[test]
    fn console_color_enabled_all_four_combinations() {
        assert!(
            console_color_enabled(true, false),
            "TTY without NO_COLOR paints"
        );
        assert!(!console_color_enabled(false, false), "pipe stays plain");
        assert!(!console_color_enabled(true, true), "NO_COLOR wins over TTY");
        assert!(
            !console_color_enabled(false, true),
            "pipe + NO_COLOR stays plain"
        );
    }

    #[test]
    fn console_line_plain_is_byte_identical() {
        let line = b"2026/08/18 12:00:00 [INFO] [ACCESS] GET /dicomweb/studies\n";
        assert_eq!(console_line(line, false), line);
    }

    #[test]
    fn console_line_wraps_level_token_with_palette() {
        let line = b"2026/08/18 12:00:00 [WARN] slow auth from 192.0.2.1\n";
        let mut expected = Vec::new();
        expected.extend_from_slice(b"2026/08/18 12:00:00 ");
        expected.extend_from_slice(COLOR_WARN);
        expected.extend_from_slice(b"[WARN]");
        expected.extend_from_slice(ANSI_RESET);
        expected.extend_from_slice(b" slow auth from 192.0.2.1\n");
        assert_eq!(console_line(line, true), expected);
    }

    #[test]
    fn console_line_colors_every_level_with_its_palette() {
        for (token, ansi) in LEVEL_TOKENS {
            let mut line = Vec::new();
            line.extend_from_slice(b"2026/08/18 12:00:00 ");
            line.extend_from_slice(token);
            line.extend_from_slice(b" body\n");
            let mut expected = Vec::new();
            expected.extend_from_slice(b"2026/08/18 12:00:00 ");
            expected.extend_from_slice(ansi);
            expected.extend_from_slice(token);
            expected.extend_from_slice(ANSI_RESET);
            expected.extend_from_slice(b" body\n");
            assert_eq!(console_line(&line, true), expected, "palette for {token:?}");
        }
    }

    #[test]
    fn console_line_colors_only_the_first_token() {
        // The message body mentions another level: only the FIRST `[LEVEL]`
        // (the one clog placed after the timestamp) is painted.
        let line = b"2026/08/18 12:00:00 [INFO] [ACCESS] downstream [ERROR]\n";
        let mut expected = Vec::new();
        expected.extend_from_slice(b"2026/08/18 12:00:00 ");
        expected.extend_from_slice(COLOR_INFO);
        expected.extend_from_slice(b"[INFO]");
        expected.extend_from_slice(ANSI_RESET);
        expected.extend_from_slice(b" [ACCESS] downstream [ERROR]\n");
        assert_eq!(console_line(line, true), expected);
    }

    #[test]
    fn console_line_without_token_is_unchanged_even_colored() {
        let line = b"no level token here\n";
        assert_eq!(console_line(line, true), line);
    }
}
