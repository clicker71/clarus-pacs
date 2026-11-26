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
    // The writer acknowledges its final flush on this channel; the receiver
    // travels with the handle so `shutdown` can wait for it.
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
    let log_dir = log_dir.to_path_buf();

    thread::Builder::new()
        .name("log-writer".into())
        .spawn(move || {
            writer_loop(rx, &log_dir, ack_tx);
        })
        .expect("Failed to spawn log writer thread");

    clog::set_sender(tx.clone());
    clog::set_level(level);
    install_foreign_log_bridge();
    LogWriter {
        tx,
        ack: std::sync::Arc::new(std::sync::Mutex::new(ack_rx)),
    }
}

impl LogWriter {
    /// Stop accepting records, drain the ones already accepted, and wait for
    /// the writer thread to finish.
    ///
    /// Dropping the handles does NOT disconnect the ring: `clog::TX` is a
    /// `OnceLock`, so the static sender outlives every `LogWriter` and the
    /// writer thread never observes a disconnect on its own. Without this,
    /// a process that exits with records still queued loses exactly the tail
    /// an investigation reads first - the last WARN/ERROR before the crash.
    ///
    /// The sentinel is an empty `Vec`, which `clog` can never produce (every
    /// rendered line ends in a newline, so it is at least one byte).
    pub fn shutdown(self) {
        // `send` blocks when the ring is full, which is precisely when the
        // tail matters most; a blocking send is therefore correct here, and
        // the wait below bounds it.
        if self.tx.send(Vec::new()).is_err() {
            return; // Writer already gone: nothing left to drain.
        }
        // The writer signals after its final flush. `recv_timeout` rather than
        // `recv` because a wedged disk must not turn a shutdown into a hang.
        let ack = self.ack.lock().unwrap_or_else(|e| e.into_inner());
        let _ = ack.recv_timeout(Duration::from_secs(SHUTDOWN_FLUSH_SECS));
    }
}

/// How long [`LogWriter::shutdown`] waits for the writer's final flush.
///
/// The writer flushes on the sentinel immediately, so this only bounds a
/// blocked disk write; 5s matches the access journal's own barrier.
const SHUTDOWN_FLUSH_SECS: u64 = 5;

/// Bounded retry interval for reopening a daily file that failed to open.
///
/// A failed open (permissions not yet fixed, a full disk) must not disable
/// file logging for the rest of the day, and must not cost an `open` syscall
/// on every single message either. One second is far below any human reaction
/// to the failure and far above the message rate of a sick server.
const REOPEN_RETRY: Duration = Duration::from_secs(1);

/// Depth of the console-mirror queue.
///
/// The mirror runs on its own thread so a stalled stderr reader cannot stop
/// the file writer (see [`writer_loop`]). Sized like the main ring: a full
/// mirror queue drops console lines, which is the right loss - the FILE is
/// the durable record and it keeps flowing.
const CONSOLE_QUEUE: usize = 1024;

/// A `Clone` handle that pushes formatted log lines into the bounded ring.
#[derive(Clone)]
pub struct LogWriter {
    tx: SyncSender<Vec<u8>>,
    /// Final-flush acknowledgement from the writer thread.
    ///
    /// Cloned handles share the one receiver, so whichever handle performs the
    /// shutdown waits for the same acknowledgement; the mutex only serialises
    /// that wait, never the producer path.
    ack: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Bounded, non-blocking push (same policy as clog's channel entry).
        //
        // A record that was NOT queued must not be reported as written: `Ok`
        // here is the only signal a caller has that its bytes were accepted,
        // and returning `Ok(buf.len())` for a full ring or a dead writer told
        // every caller that a record it never wrote had been delivered. The
        // drop policy belongs to `clog` (which counts full-ring drops and
        // treats `Disconnected` as shutdown), not to this adapter.
        match self.tx.try_send(buf.to_vec()) {
            Ok(()) => Ok(buf.len()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "log ring full: record not queued",
            )),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "log writer stopped: record not queued",
            )),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Nothing is buffered in this adapter: the ring is the buffer and the
        // writer thread flushes it on its own cadence. `Ok` here is honest -
        // it makes no claim about the disk.
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
fn writer_loop(
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    log_dir: &std::path::Path,
    ack: std::sync::mpsc::Sender<()>,
) {
    let mut state = WriterState::new(log_dir.to_path_buf());
    // Decided ONCE per writer thread, before the drain loop.
    let color = console_color_enabled(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    );
    // The console mirror runs on its OWN thread behind a bounded queue.
    // Writing stderr inline from this loop made a stalled reader - a full
    // pipe whose consumer stopped - block the ONLY thread that writes the
    // daily file and runs both aggregation sweeps. The ring then filled and
    // request threads started dropping records: one stuck reader cost the
    // durable log. A full mirror queue now drops console lines only.
    let mirror = spawn_console_mirror(color);
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
        let mut disconnect = false;
        loop {
            let msg = match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(msg) => msg,
                // The channel is gone. Returning immediately - rather than
                // spinning on an empty receiver - is the difference between
                // an exiting process and one that burns a core forever.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    disconnect = true;
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            };
            if msg.is_empty() {
                // Shutdown sentinel: everything queued before it is already
                // in `state`'s file. Flush and acknowledge, then stop.
                state.flush();
                let _ = ack.send(());
                return;
            }
            state.append(&msg);
            if let Some(m) = mirror.as_ref() {
                // Best-effort: a full mirror queue drops the CONSOLE line and
                // leaves the file write untouched.
                let _ = m.try_send(msg);
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        if disconnect {
            state.flush();
            return;
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

/// Upper bound on the bytes a single log write may accept.
///
/// Zero in production (one `write` per line, as before). A test sets it to make
/// a short write happen, because a regular-file append of a few hundred bytes
/// is never partial on any platform this server runs on - which is exactly why
/// "the byte count was ignored" survived until now.
#[cfg(test)]
static CHUNK_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The slice a single `write` call is allowed to attempt.
fn accepted_slice(rest: &[u8]) -> &[u8] {
    #[cfg(test)]
    {
        let cap = CHUNK_BYTES.load(std::sync::atomic::Ordering::Relaxed) as usize;
        if cap > 0 {
            return &rest[..cap.min(rest.len())];
        }
    }
    rest
}

/// Test-only stall applied to the writer's drain loop.
///
/// Zero in production. It exists so a shutdown test can distinguish "the
/// barrier drained the tail" from "the idle loop happened to flush in time":
/// with the writer stalled, only the sentinel path can produce the record.
#[cfg(test)]
static STALL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Wait out the configured test stall, if any.
#[inline]
fn test_stall() {
    #[cfg(test)]
    {
        let ms = STALL_MS.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}

/// Owns the current daily file and the last successful open attempt.
struct WriterState {
    log_dir: std::path::PathBuf,
    current_date: String,
    file: Option<std::fs::File>,
    /// When the last open attempt failed, so a persistent failure retries at
    /// [`REOPEN_RETRY`] rather than on every message.
    reopen_after: Option<std::time::Instant>,
}

impl WriterState {
    fn new(log_dir: std::path::PathBuf) -> Self {
        Self {
            log_dir,
            current_date: String::new(),
            file: None,
            reopen_after: None,
        }
    }

    /// Append one line, opening (or rolling over to) the day's file first.
    fn append(&mut self, msg: &[u8]) {
        test_stall();
        // A failed conversion must NOT be treated as a date: rendering it as
        // the epoch would roll the log over onto a 1970 file. Dropping this
        // line keeps the file series correct.
        let Ok(now) = clarus_core::time::local_now() else {
            return;
        };
        let today = now.fmt_ymd();

        if today != self.current_date {
            // Day rollover (or first write): a new day always gets a fresh
            // open attempt, whatever the previous day's outcome was.
            self.current_date = today.clone();
            self.file = self.try_open(&today);
        } else if self.file.is_none() {
            // The previous attempt for THIS day did not produce a handle.
            // Leaving the writer to skip the attempt for the rest of the day
            // meant a transient permission or space failure silenced file
            // logging until midnight, even after the operator fixed it.
            //
            // A failed OPEN is throttled to `REOPEN_RETRY`: the directory is
            // missing or unwritable, so an attempt per line would cost a
            // failing syscall per line. A failed WRITE clears this deadline
            // (below), because the file itself is fine and the next line
            // should not be lost waiting for it.
            let due = self
                .reopen_after
                .is_none_or(|at| std::time::Instant::now() >= at);
            if due {
                self.file = self.try_open(&today);
            }
        }

        if let Some(f) = self.file.as_mut() {
            // `write` may accept only a prefix; the byte count used to be
            // ignored, so the next message was appended straight onto a
            // half-written line and the two merged. A partial line is worse
            // than none: it corrupts a record that then looks complete.
            if Self::write_line(f, msg).is_err() {
                // A failed write leaves the record boundary unknown, so the
                // handle is dropped and the day's reopen attempt starts from a
                // fresh append instead of continuing on top of a prefix.
                //
                // No retry throttle here: the file itself opened and is
                // appendable, so the next line must be attempted on the next
                // message. Throttling this case lost lines that would have
                // written fine (the transient failure had already passed).
                self.file = None;
                self.reopen_after = None;
            }
        }
    }

    /// Write `msg` completely, looping over partial accepts.
    ///
    /// The seam has two independent knobs: `SITE_LOG_WRITE` fails the call
    /// outright, and [`CHUNK_BYTES`] caps how much one call may accept (0 =
    /// no cap). The cap is what makes the short-write bug observable: a small
    /// append to a regular file is never partial in practice, so without it
    /// "ignore the byte count" cannot be distinguished from "loop".
    fn write_line(f: &mut std::fs::File, msg: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if crate::io_fault::take_failure(crate::io_fault::SITE_LOG_WRITE).is_some() {
            return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
        }
        // The explicit loop is `write_all`'s contract. A zero-length accept is
        // `WriteZero` - the same error `write_all` reports - never a spin.
        let mut rest = msg;
        while !rest.is_empty() {
            let slice = accepted_slice(rest);
            let n = f.write(slice)?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
            }
            rest = &rest[n..];
        }
        Ok(())
    }

    /// Attempt the day's open, recording the retry deadline on failure.
    fn try_open(&mut self, date: &str) -> Option<std::fs::File> {
        match open_daily(&self.log_dir, date) {
            Some(f) => {
                self.reopen_after = None;
                Some(f)
            }
            None => {
                self.reopen_after = Some(std::time::Instant::now() + REOPEN_RETRY);
                None
            }
        }
    }

    /// Flush the current file, if any.
    fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
    }
}

/// Spawn the console-mirror thread, returning its queue.
///
/// `None` when the thread cannot be started: the console is decoration, the
/// file is the record, so a missing mirror must not stop logging.
fn spawn_console_mirror(color: bool) -> Option<SyncSender<Vec<u8>>> {
    let (tx, rx) = sync_channel::<Vec<u8>>(CONSOLE_QUEUE);
    thread::Builder::new()
        .name("log-console".into())
        .spawn(move || {
            let mut out = std::io::stderr();
            while let Ok(msg) = rx.recv() {
                // A failing console write is reported nowhere and must not
                // stop the loop: stderr is best-effort by design here.
                let _ = out.write_all(&console_line(&msg, color));
            }
        })
        .ok()
        .map(|_| tx)
}

/// Open (create/append) today's log file: `clarus.log.YYYY-MM-DD`.
fn open_daily(log_dir: &std::path::Path, date: &str) -> Option<std::fs::File> {
    // Test-only: make the open fail, so the reopen-retry path has a trigger.
    #[cfg(test)]
    if crate::io_fault::take_failure(crate::io_fault::SITE_LOG_OPEN).is_some() {
        return None;
    }
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
        // WARN/ERROR carry a source tag so an admin can see which foreign
        // crate spoke; naive DEBUG/TRACE are dropped by the clog filter.
        let level = bridge_level(record.level());
        let msg = format!("logger={} {}", record.target(), record.args());
        clog::emit(level, format_args!("{msg}"));
    }

    fn flush(&self) {}
}

/// Map a `log` facade level onto a [`clog`] level.
///
/// TRACE stays TRACE. Collapsing it into DEBUG meant a debug build that asked
/// for TRACE could never see a foreign TRACE line: it was rendered under the
/// wrong level token and filtered as DEBUG.
fn bridge_level(level: log::Level) -> clog::Level {
    match level {
        log::Level::Error => clog::Level::Error,
        log::Level::Warn => clog::Level::Warn,
        log::Level::Info => clog::Level::Info,
        log::Level::Debug => clog::Level::Debug,
        log::Level::Trace => clog::Level::Trace,
    }
}

/// The `log` facade's own maximum.
///
/// It is a SEPARATE ceiling from `clog`'s, and it runs first: a facade set to
/// `Info` rejects a foreign DEBUG/TRACE record before `clog` can apply a
/// permissive debug-build ceiling, so a `log_level = debug` build still lost
/// every foreign debug line. Release keeps `Info`, which is what that build
/// ships.
fn foreign_max_level() -> log::LevelFilter {
    if cfg!(debug_assertions) {
        log::LevelFilter::Trace
    } else {
        log::LevelFilter::Info
    }
}

/// Install the `log` facade bridge (idempotent).
fn install_foreign_log_bridge() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = log::set_logger(&ForeignLog);
        log::set_max_level(foreign_max_level());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // -- Writer state: open retry, complete writes -------------------------

    /// The seam is process-global, so every test that arms it must run alone.
    static WRITER_SEAM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn seam<F: FnOnce()>(f: F) {
        let _g = WRITER_SEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::io_fault::clear();
        f();
        crate::io_fault::clear();
    }

    /// A failed open must not silence file logging for the rest of the day.
    ///
    /// The old code set `current_date` on every message but left `file` as
    /// `None`, so once an open failed the writer never attempted another one -
    /// a transient permission or space failure cost the whole day of logs,
    /// even after the operator fixed it. Driven through `append`, because that
    /// is where the decision lives: testing `try_open` alone would pass with
    /// the retry branch deleted.
    #[test]
    fn a_failed_open_is_retried_and_recovered() {
        seam(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let today = clarus_core::time::local_now().expect("clock").fmt_ymd();
            let path = dir.path().join(format!("clarus.log.{today}"));
            let mut state = WriterState::new(dir.path().to_path_buf());

            // First message: the open fails. Nothing is written, and the
            // writer must remember to retry rather than give up on the day.
            crate::io_fault::arm(
                crate::io_fault::SITE_LOG_OPEN,
                std::io::ErrorKind::StorageFull,
            );
            state.append(b"2026/08/18 12:00:00 [INFO] lost to a failed open\n");
            assert!(state.file.is_none(), "the armed open must fail");
            assert_eq!(
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                0,
                "nothing can be written through a missing handle"
            );

            // The seam fails once per arming, so the retry re-opens and the
            // message is written - the "operator fixed the permissions" case.
            // The retry is throttled by REOPEN_RETRY (a failed directory
            // open must not cost a syscall per line), so the test waits out
            // the interval instead of assuming an immediate re-attempt.
            std::thread::sleep(REOPEN_RETRY + Duration::from_millis(50));
            state.append(b"2026/08/18 12:00:01 [INFO] written after the retry\n");
            state.flush();
            let written = std::fs::read(&path).expect("the day file now exists");
            assert_eq!(
                written, b"2026/08/18 12:00:01 [INFO] written after the retry\n",
                "a later message must reopen the day, not stay disabled"
            );
        });
    }

    /// A message is written COMPLETE, or not at all.
    ///
    /// `write` may accept a prefix. The writer ignored its byte count and moved
    /// on, so the next message was appended onto a half-written line and the
    /// two merged into one record that looked whole. The single-byte accept
    /// below forces the short-write path on a real file; the content and the
    /// line count then pin that the loop ran to completion.
    #[test]
    fn every_message_lands_on_its_own_complete_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let today = clarus_core::time::local_now().expect("clock").fmt_ymd();
        let mut state = WriterState::new(dir.path().to_path_buf());
        let first = b"2026/08/18 12:00:00 [INFO] first record\n";
        let second = b"2026/08/18 12:00:01 [WARN] second record\n";

        // One byte accepted per call: without the loop only the first byte of
        // each message would reach the medium.
        CHUNK_BYTES.store(1, std::sync::atomic::Ordering::Relaxed);
        state.append(first);
        state.append(second);
        CHUNK_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
        state.flush();

        let written = std::fs::read(dir.path().join(format!("clarus.log.{today}")))
            .expect("the day file exists");
        assert_eq!(
            written,
            [first.as_slice(), second.as_slice()].concat(),
            "two messages must be two complete records, not one prefix each"
        );
        assert_eq!(
            written.iter().filter(|&&b| b == b'\n').count(),
            2,
            "each message ends its own line"
        );
    }

    /// A failed write drops the handle rather than appending onto a prefix.
    ///
    /// The seam makes the write fail without touching the file. What must hold
    /// is that the writer stops using that handle: appending the next record to
    /// a medium that already swallowed part of the previous one is how a
    /// forged-looking line gets into a log.
    #[test]
    fn a_failed_write_drops_the_handle() {
        seam(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let today = clarus_core::time::local_now().expect("clock").fmt_ymd();
            let mut state = WriterState::new(dir.path().to_path_buf());
            crate::io_fault::arm(
                crate::io_fault::SITE_LOG_WRITE,
                std::io::ErrorKind::StorageFull,
            );
            state.append(b"2026/08/18 12:00:00 [ERROR] this write fails\n");
            assert!(
                state.file.is_none(),
                "a failed write must not keep the handle for the next record"
            );
            let path = dir.path().join(format!("clarus.log.{today}"));
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            assert_eq!(len, 0, "the failed record must not be half-written");
        });
    }

    /// `Write::write` must not report a record as queued when it was not.
    ///
    /// With the ring full, the old implementation returned `Ok(buf.len())`, so
    /// every caller was told a record had been delivered that was dropped. The
    /// drop policy lives in `clog`, which counts it; the adapter's job is to
    /// say what happened.
    #[test]
    fn a_full_ring_is_reported_as_an_error_not_a_success() {
        let (tx, rx) = sync_channel::<Vec<u8>>(1);
        let mut w = LogWriter {
            tx,
            ack: std::sync::Arc::new(std::sync::Mutex::new(std::sync::mpsc::channel::<()>().1)),
        };
        // Fill the single slot.
        let n = w.write(b"first\n").expect("the slot is free");
        assert_eq!(n, 6, "a delivered record reports its full length");
        let err = w.write(b"second\n").expect_err("the ring is full");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        drop(rx);
        let err = w.write(b"third\n").expect_err("the writer is gone");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// A debug build asks the facade for TRACE and keeps TRACE as TRACE.    ///    ///    /// The facade had its own `Info` maximum, so a foreign DEBUG/TRACE record
    /// was rejected before `clog`'s (permissive, in debug) ceiling ever saw it,
    /// and the level mapping collapsed TRACE into DEBUG so the remaining trace
    /// lines were mislabelled.
    #[test]
    fn the_foreign_bridge_keeps_trace_and_a_debug_build_limit() {
        #[cfg(debug_assertions)]
        assert_eq!(
            foreign_max_level(),
            log::LevelFilter::Trace,
            "a debug build must let foreign TRACE through to the clog ceiling"
        );
        #[cfg(not(debug_assertions))]
        assert_eq!(
            foreign_max_level(),
            log::LevelFilter::Info,
            "release keeps the shipped Info limit"
        );

        // The mapping must not collapse TRACE into DEBUG. Asserted against the
        // PRODUCTION function, so deleting the `Trace` arm fails here.
        assert_eq!(bridge_level(log::Level::Trace), clog::Level::Trace);
        assert_eq!(bridge_level(log::Level::Debug), clog::Level::Debug);
        assert_eq!(bridge_level(log::Level::Error), clog::Level::Error);
    }

    /// Records queued before `shutdown` must be on disk when it returns.
    ///
    /// Dropping the handles could not disconnect the ring - `clog::TX` is a
    /// `OnceLock` - so the writer never saw a disconnect and the tail went
    /// down with the process: exactly the lines an investigation reads first.
    /// This drives the REAL writer thread through `shutdown` and reads the file
    /// afterwards, so deleting the barrier (`Ok(())` returned without waiting)
    /// leaves the file short.
    #[test]
    fn shutdown_waits_for_the_queued_tail_to_reach_disk() {
        let _g = WRITER_SEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let line = b"2026/08/18 12:00:00 [INFO] the tail that must survive\n";
        let (tx, rx) = sync_channel::<Vec<u8>>(RING_SIZE);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
        let log_dir = dir.path().to_path_buf();
        let writer = thread::Builder::new()
            .name("log-writer-test".into())
            .spawn(move || writer_loop(rx, &log_dir, ack_tx))
            .expect("spawn writer");

        // The writer's idle loop flushes on its own cadence, so on an idle
        // queue the file would be complete whether or not `shutdown` waits.
        // Stall the writer first, so ONLY the barrier can produce the tail on
        // the way out: the writer is still asleep when `shutdown` returns.
        STALL_MS.store(1_500, std::sync::atomic::Ordering::Relaxed);
        tx.send(b"2026/08/18 12:00:00 [INFO] stall marker\n".to_vec())
            .expect("queue the stall marker");
        // Let the writer pick the marker up and enter the stall, so the record
        // under test is still queued when `shutdown` is called.
        std::thread::sleep(Duration::from_millis(150));
        tx.send(line.to_vec()).expect("queue the tail");
        let handle = LogWriter {
            tx,
            ack: std::sync::Arc::new(std::sync::Mutex::new(ack_rx)),
        };
        handle.shutdown();
        // Reaching here means the barrier returned; the tail must be on disk
        // at this instant, which the stalled writer could not have produced on
        // its own.
        let today = clarus_core::time::local_now().expect("clock").fmt_ymd();
        let written = std::fs::read(dir.path().join(format!("clarus.log.{today}")))
            .expect("the day file exists after the barrier");
        let text = String::from_utf8_lossy(&written);
        assert!(
            text.contains("the tail that must survive"),
            "the record queued before shutdown must be on disk when it returns, got {text:?}"
        );
        STALL_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        writer
            .join()
            .expect("the writer thread exits on the sentinel");
    }
}
