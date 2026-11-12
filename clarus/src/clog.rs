// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! nginx-way logging: one plaintext file, one writer thread, own
//! level filter. `tracing` is absent from the release path.
//!
//! # Why
//! The old pipeline ran three artifact producers (tracing -> JSON/`clarus.log`,
//! journal-flush -> plaintext `clarus.log`, stderr). This module is the single
//! plaintext front-end whose output feeds the one writer thread:
//!   - `clog::info!/warn!/error!/debug!/trace!` - message macros whose LEVEL is
//!     decided in code; the argument grammar mirrors the `tracing::*!` calls
//!     they replace (leading structured-field tokens are tolerated and dropped,
//!     the trailing `("msg", args..)` tuple is formatted).
//!   - `clog::event!(LEVEL, TAG, ...)` - tagged events for the INFO whitelist
//!     (STARTUP_CONFIG, LISTEN, ACCESS, STOW_ACCEPTED, ...).
//!   - a global `log::Log` bridge so foreign crates (rusqlite_migration, ...)
//!     route WARN/ERROR into the file with `logger=<source>`; DEBUG/TRACE are
//!     dropped in production.
//!
//! # Filtering (owner: this module, not EnvFilter)
//! In DEBUG builds (`cfg(debug_assertions)`) every record goes to stderr,
//! preserving the old tracing-on-stderr behavior. In release builds:
//!   - DEBUG/TRACE are dropped;
//!   - INFO passes only when its tag is on the whitelist;
//!   - WARN/ERROR always pass.
//!
//! The writer channel is a bounded `sync_channel`; a full buffer silently drops
//! the line (workers never block on log I/O).

#![forbid(unsafe_code)]

use std::sync::mpsc::{SyncSender, TrySendError};

/// Severity of a log record.
///
/// Discriminant order IS the ceiling order: error < warn < info < debug <
/// trace, so `level > ceiling` means exactly "drop above the ceiling".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    /// The `[LEVEL]` tag used in the plaintext log (nginx error.log style).
    pub fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// INFO whitelist: only these constant tags survive the INFO filter in prod.
/// Everything else at INFO (and below) is dropped; WARN/ERROR always pass.
const INFO_WHITELIST: &[&str] = &[
    "STARTUP_CONFIG",
    "LISTEN",
    "ACCESS",
    "WADO_SERIES",
    "STOW_STUDY",
    "STOW_ACCEPTED",
    "STOW_REJECTED",
    "INDEX_ROLLOVER",
    "INDEX_PRELOADED",
    "HEALTH_ANOMALY",
    "SHUTDOWN",
    "SHADOW_MISMATCH",
    "BTREE_WRITE_FAILED",
    "STUDY_DELETED",
    "ERASE_COMPLETE",
    "TRANSCODE",
    "ACCESS_ROTATE",
    // One line per UPS event (workitem created, state changed). Added when the
    // event line was introduced: without a whitelist entry it was emitted at
    // INFO and then silently dropped in every RELEASE build, so the feature
    // worked in a developer's debug run and did nothing in production. The
    // release-only test in `tests/ups_event_log.rs` is what caught that.
    "UPS_EVENT",
    // One line per MWL search (station + number of steps matched) and one per
    // MPPS N-CREATE/N-SET (UID + operation + the state transition). Same
    // release-only trap as UPS_EVENT above: an INFO line whose tag is not
    // listed here never reaches a production log file.
    "MWL_SEARCH",
    "MPPS_EVENT",
    // One line per Storage Commitment retention sweep that expired results.
    // Same release-only trap: the sweep runs on a background thread, so a bare
    // INFO line was visible in a developer's debug run and invisible in
    // production, which is the only place the expiry matters.
    "COMMITMENT_RETENTION",
    // MWL/MPPS retention sweep summary and the UPS retention lines. Same
    // release-only trap as above: these run on background threads, so a bare
    // INFO line reached no production log - exactly where a deletion has to
    // leave a trace.
    "WORKFLOW_RETENTION",
    "UPS_RETENTION",
];

fn whitelisted(tag: &str) -> bool {
    INFO_WHITELIST.contains(&tag)
}

// -- Bounded writer channel -------------------------------------------

static TX: std::sync::OnceLock<SyncSender<Vec<u8>>> = std::sync::OnceLock::new();

/// Install the writer channel. Called once by `log_writer::init`.
pub fn set_sender(tx: SyncSender<Vec<u8>>) {
    let _ = TX.set(tx);
}

/// True when the writer channel is installed (the server may reach here before
/// logging is initialized).
pub fn is_ready() -> bool {
    TX.get().is_some()
}

// -- Level ceiling ([logging] log_level) --------------------------------

/// Configured ceiling from `[logging] log_level`. Set once by
/// `log_writer::init`; before that the default `info` applies (matching the
/// `clarus.conf` default), so pre-init and unit-test paths behave like
/// production.
static CEILING: std::sync::OnceLock<Level> = std::sync::OnceLock::new();

/// Install the level ceiling. Called once by `log_writer::init`.
pub fn set_level(level: Level) {
    let _ = CEILING.set(level);
}

/// The active ceiling (`info` until [`set_level`] runs).
fn ceiling() -> Level {
    CEILING.get().copied().unwrap_or(Level::Info)
}

/// True when `level` is above `ceiling` and the line must be dropped.
/// Ceiling order: error < warn < info < debug < trace.
fn exceeds_ceiling(level: Level, ceiling: Level) -> bool {
    level > ceiling
}

// -- Per-peer rate limiter (auth WARN suppression) -----------------------

// Auxiliary: pacing of ephemeral auth warnings. Without this, a misconfigured
// nginx or a spoofing scanner would emit one WARN per request and flood the
// file. Each offender peer may trigger at most one WARN per `WINDOW`.
const RATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// Upper bound of peers tracked at once (LRU eviction).
const RATE_CAP: usize = 1024;

struct RateBucket {
    key: Vec<u8>,
    last: std::time::Instant,
}

/// Thread-safe per-peer rate limiter. `Mutex` is held only for the tiny map
/// lookup/update; the allowed/denied decision is what the warn site reads.
static RATE: std::sync::OnceLock<std::sync::Mutex<Vec<RateBucket>>> = std::sync::OnceLock::new();

/// Returns `true` when this peer may emit a rate-limited warning now (its
/// previous warning, if any, is older than [`RATE_WINDOW`]). Tracks at most
/// [`RATE_CAP`] peers (LRU: the least-recently-warned is evicted first).
pub fn rate_limited_allow(key: &[u8]) -> bool {
    let now = std::time::Instant::now();
    let mut buckets = RATE
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // Reuse an existing bucket for this peer.
    if let Some(b) = buckets.iter_mut().find(|b| b.key == key) {
        if now.duration_since(b.last) < RATE_WINDOW {
            return false;
        }
        b.last = now;
        return true;
    }
    // New peer: admit (cap the table, evict the least-recently-used entry).
    if buckets.len() >= RATE_CAP
        && let Some((idx, _)) = buckets.iter().enumerate().min_by_key(|(_, b)| b.last)
    {
        buckets.swap_remove(idx);
    }
    buckets.push(RateBucket {
        key: key.to_vec(),
        last: now,
    });
    true
}

/// Record one dropped line in the monotonic `clarus_log_dropped` metric.
/// Called by BOTH `Full` arms of `push_line`/`push_plaintext` (Task B 22.08).
/// `Disconnected` is the normal process shutdown path - NOT counted.
#[inline]
fn record_drop() {
    crate::metrics::record_log_drop();
}

/// Push a fully-formatted line (with trailing newline) to the writer thread.
/// Silently drops when the bounded ring is full or the thread has exited;
/// a full-ring drop is counted in `clarus_log_dropped` (never-block stays,
/// the hot path never waits on the journal).
fn push_line(line: String) {
    let Some(tx) = TX.get() else {
        return;
    };
    let bytes = line.into_bytes();
    match tx.try_send(bytes) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => record_drop(),
        Err(TrySendError::Disconnected(_)) => {} // process shutdown - not a drop
    }
}

/// Push a batch of already-formatted plaintext lines (each newline-terminated)
/// to the SAME writer thread. Used by the journal-flush drain so the CBOR
/// journal events (`STOW_ACCEPTED`, ...) share the one daily-rotated file
/// instead of a second writer + file. Same bounded, never-blocks policy; a
/// full-ring drop is counted in `clarus_log_dropped`.
pub fn push_plaintext(bytes: Vec<u8>) {
    let Some(tx) = TX.get() else {
        return;
    };
    match tx.try_send(bytes) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => record_drop(),
        Err(TrySendError::Disconnected(_)) => {} // process shutdown - not a drop
    }
}

// -- Filter + format --------------------------------------------------

/// True when a record at `level` (with optional INFO tag) survives the
/// filter. The ceiling applies FIRST, in BOTH build modes; then in DEBUG
/// builds every remaining record passes (stderr sink), in release:
/// DEBUG/TRACE dropped, INFO requires a whitelisted tag, WARN/ERROR pass.
///
/// Split out of [`maybe_line`] (A4) so `emit`/`emit_tagged` decide BEFORE
/// formatting the message: a dropped record never pays `to_string()`.
fn filtered(ceiling: Level, level: Level, tag: Option<&str>) -> bool {
    if exceeds_ceiling(level, ceiling) {
        return false;
    }
    if cfg!(debug_assertions) {
        return true;
    }
    match level {
        Level::Debug | Level::Trace => false,
        Level::Info => tag.is_some_and(whitelisted),
        Level::Warn | Level::Error => true,
    }
}

/// Decide whether `level`/`tag` survives the filter and return the rendered
/// line. The ceiling is a parameter (not read from the global) so unit tests
/// can probe exact boundaries; `emit`/`emit_tagged` pass the configured value.
fn maybe_line(
    ceiling: Level,
    level: Level,
    msg: &str,
    tag: Option<(&str, &str)>,
) -> Option<String> {
    if !filtered(ceiling, level, tag.map(|t| t.0)) {
        return None;
    }
    Some(render(level, msg, tag))
}

/// Build a plaintext line: `YYYY/MM/DD HH:MM:SS [LEVEL] [tag] message`.
///
/// The prefix matches `clarus_core::journal::format_plaintext` so an event
/// never has two textual shapes. A whitelist tag (when present) is emitted as
/// the leading identifier before the message.
fn render(level: Level, msg: &str, tag: Option<(&str, &str)>) -> String {
    // wall-clock "now" + render via clarus-core::time (libc),
    // byte-identical %Y/%m/%d %H:%M:%S to the previous chrono format.
    let now = clarus_core::time::local_now().fmt_datetime();
    let mut out = format!("{now} [{}] ", level.tag());
    if let Some((t, payload)) = tag {
        out.push_str(t);
        if !payload.is_empty() {
            out.push(' ');
            out.push_str(payload);
        }
        out.push(' ');
    }
    out.push_str(msg);
    out.push('\n');
    out
}

/// Route one formatted message through the filter + writer.
///
/// A4: the level filter runs BEFORE `args.to_string()` - a dropped record
/// (e.g. DEBUG under the info ceiling) costs zero formatting.
pub fn emit(level: Level, args: std::fmt::Arguments<'_>) {
    if !filtered(ceiling(), level, None) {
        return;
    }
    let msg = args.to_string();
    if let Some(line) = maybe_line(ceiling(), level, &msg, None) {
        if cfg!(debug_assertions) {
            eprint!("{line}");
        } else {
            push_line(line);
        }
    }
}

/// Route one clearly-tagged event (whitelist path) through the filter+writer.
///
/// A4: tag + level filter run BEFORE `args.to_string()` (see [`emit`]).
pub fn emit_tagged(level: Level, tag: &str, payload: &str, args: std::fmt::Arguments<'_>) {
    if !filtered(ceiling(), level, Some(tag)) {
        return;
    }
    let msg = args.to_string();
    if let Some(line) = maybe_line(ceiling(), level, &msg, Some((tag, payload))) {
        if cfg!(debug_assertions) {
            eprint!("{line}");
        } else {
            push_line(line);
        }
    }
}

/// No-op for an empty macro expansion (no message present).
pub fn noop() {}

// -- Macro front-end --------------------------------------------------
//
// The level macros live at the CRATE ROOT via `#[macro_export]`, so they are
// invoked as `clarus_server::log_info!` from the LIBRARY modules and from the
// binary (`main.rs`), whose `crate::` is the binary itself and cannot see a
// `pub(crate) use` inside `mod clog`. Three things keep call sites green under
// `-D warnings`:
//  1. The message (`literal, args...`) is passed to `format_args!` VERBATIM
//     via `clog_msg_emit!`, so inline captures (`{e}`) and positional args
//     count as uses of the caller's variables (a `format_args!` built from
//     separate `$lit`/`$arg` fragments does NOT attribute them).
//  2. Structured-field VALUES are taken by reference and dropped (D4: a bare
//     `let _ = &value;` - no formatting, no evaluation beyond the reference),
//     so `error = %e` keeps `e` "used" without a move.
//  3. The expansion is a block expression, so it is valid both in statement
//     and expression positions (e.g. a match arm).

/// Mark each leading field value used, by reference (no formatting).
#[macro_export]
macro_rules! clog_keep_fields {
    ([$($out:tt)*] % $name:ident , $($rest:tt)*) => {
        let _ = &$name;
        $crate::clog_keep_fields!([$($out)* $name] $($rest)*)
    };
    ([$($out:tt)*] $name:ident = % $value:expr , $($rest:tt)*) => {
        let _ = &$value;
        $crate::clog_keep_fields!([$($out)* $value] $($rest)*)
    };
    ([$($out:tt)*] $name:ident = $value:expr , $($rest:tt)*) => {
        let _ = &$value;
        $crate::clog_keep_fields!([$($out)* $value] $($rest)*)
    };
    ([$($out:tt)*] $name:ident , $($rest:tt)*) => {
        let _ = &$name;
        $crate::clog_keep_fields!([$($out)* $name] $($rest)*)
    };
    // Done: no fields remain (the accumulated `$out` is not needed).
    ([$($out:tt)*]) => {};
}

/// Core message formatter for the per-level shadows.
///
/// Peels leading structured-field tokens into an accumulator; on the first
/// string literal the remaining tokens (`literal, args...`) are handed to
/// `clog_msg_emit!` verbatim.
#[macro_export]
macro_rules! clog_expand {
    ($level:expr, [$($acc:tt)*] $lit:literal $($msg:tt)*) => {{
        $crate::clog_keep_fields!([] $($acc)*);
        $crate::clog_msg_emit!($level, $lit $($msg)*)
    }};
    ($level:expr, [$($acc:tt)*] $head:tt $($rest:tt)*) => {
        $crate::clog_expand!($level, [$($acc)* $head] $($rest)*)
    };
    ($level:expr, [$($acc:tt)*]) => {
        $crate::clog::noop()
    };
}

/// Emit a message from its raw tokens (literal FIRST, then args) so inline
/// captures in the literal are attributed to the caller's variables.
#[macro_export]
macro_rules! clog_msg_emit {
    ($level:expr, $($msg:tt)*) => {
        $crate::clog::emit($level, format_args!($($msg)*))
    };
}

/// Core tagged-event formatter (whitelist path).
#[macro_export]
macro_rules! clog_event_expand {
    ($level:expr, $tag:expr, $payload:expr, [$($acc:tt)*] $lit:literal $($msg:tt)*) => {{
        $crate::clog_keep_fields!([] $($acc)*);
        $crate::clog_msg_tag_emit!($level, $tag, $payload, $lit $($msg)*)
    }};
    ($level:expr, $tag:expr, $payload:expr, [$($acc:tt)*] $head:tt $($rest:tt)*) => {
        $crate::clog_event_expand!($level, $tag, $payload, [$($acc)* $head] $($rest)*)
    };
    ($level:expr, $tag:expr, $payload:expr, [$($acc:tt)*]) => {
        $crate::clog::noop()
    };
}

/// Tagged-emit from raw message tokens (literal first).
#[macro_export]
macro_rules! clog_msg_tag_emit {
    ($level:expr, $tag:expr, $payload:expr, $($msg:tt)*) => {
        $crate::clog::emit_tagged($level, $tag, $payload, format_args!($($msg)*))
    };
}

/// Log at DEBUG level. In production DEBUG/TRACE are dropped.
#[macro_export]
macro_rules! clog_debug {
    ($($rest:tt)*) => {
        $crate::clog_expand!($crate::clog::Level::Debug, [] $($rest)*)
    };
}

/// Log at INFO level (whitelist-filtered in production).
#[macro_export]
macro_rules! clog_info {
    ($($rest:tt)*) => {
        $crate::clog_expand!($crate::clog::Level::Info, [] $($rest)*)
    };
}

/// Log at WARN level.
#[macro_export]
macro_rules! clog_warn {
    ($($rest:tt)*) => {
        $crate::clog_expand!($crate::clog::Level::Warn, [] $($rest)*)
    };
}

/// Log at ERROR level.
#[macro_export]
macro_rules! clog_error {
    ($($rest:tt)*) => {
        $crate::clog_expand!($crate::clog::Level::Error, [] $($rest)*)
    };
}

/// Tagged event (whitelist path).
/// `clog_event!(clog::Level::Info, "TAG", "payload", "msg", args)`.
#[macro_export]
macro_rules! clog_event {
    ($level:expr, $tag:expr, $payload:expr, $($rest:tt)*) => {
        $crate::clog_event_expand!($level, $tag, $payload, [] $($rest)*)
    };
}

#[cfg(test)]
mod rate_tests {
    use super::*;

    #[test]
    fn first_call_always_allowed() {
        assert!(rate_limited_allow(b"192.0.2.1"));
    }

    #[test]
    fn second_call_within_window_denied() {
        let k = b"192.0.2.2";
        assert!(rate_limited_allow(k));
        assert!(!rate_limited_allow(k), "same peer within 60s is denied");
    }

    #[test]
    fn distinct_peers_independent() {
        assert!(rate_limited_allow(b"192.0.2.10"));
        assert!(rate_limited_allow(b"192.0.2.11"));
        // Within the same 60s window the second call for each peer is denied,
        // and different peers do not block each other.
        assert!(!rate_limited_allow(b"192.0.2.10"));
    }
}

#[cfg(test)]
mod level_tests {
    use super::*;

    #[test]
    fn ceiling_warn_passes_warn_and_error_drops_the_rest() {
        // Ceiling `warn`: WARN/ERROR pass, INFO (even whitelisted),
        // DEBUG and TRACE are dropped.
        assert!(!exceeds_ceiling(Level::Warn, Level::Warn));
        assert!(!exceeds_ceiling(Level::Error, Level::Warn));
        assert!(exceeds_ceiling(Level::Info, Level::Warn));
        assert!(exceeds_ceiling(Level::Debug, Level::Warn));
        assert!(exceeds_ceiling(Level::Trace, Level::Warn));
    }

    #[test]
    fn ceiling_error_passes_only_error() {
        assert!(!exceeds_ceiling(Level::Error, Level::Error));
        for other in [Level::Warn, Level::Info, Level::Debug, Level::Trace] {
            assert!(exceeds_ceiling(other, Level::Error), "{other:?} dropped");
        }
    }

    #[test]
    fn ceiling_trace_drops_nothing() {
        for level in [
            Level::Error,
            Level::Warn,
            Level::Info,
            Level::Debug,
            Level::Trace,
        ] {
            assert!(!exceeds_ceiling(level, Level::Trace), "{level:?} passes");
        }
    }

    #[test]
    fn default_ceiling_info_drops_debug_and_trace_in_any_build() {
        // Before `log_writer::init` the ceiling is INFO (config default), so
        // DEBUG/TRACE lines never render - in BOTH build modes.
        assert!(maybe_line(ceiling(), Level::Debug, "x", None).is_none());
        assert!(maybe_line(ceiling(), Level::Trace, "x", None).is_none());
    }

    #[test]
    fn maybe_line_ceiling_warn_drops_whitelisted_info() {
        // Ceiling `warn`: the INFO whitelist is irrelevant because INFO is
        // already above the ceiling; WARN/ERROR still render.
        let dropped = maybe_line(Level::Warn, Level::Info, "x", Some(("ACCESS", "1.2.3")));
        assert!(
            dropped.is_none(),
            "whitelisted INFO is above the warn ceiling"
        );
        assert!(maybe_line(Level::Warn, Level::Warn, "x", None).is_some());
        assert!(maybe_line(Level::Warn, Level::Error, "x", None).is_some());
    }

    #[test]
    fn maybe_line_ceiling_error_passes_only_error() {
        assert!(maybe_line(Level::Error, Level::Error, "x", None).is_some());
        assert!(maybe_line(Level::Error, Level::Warn, "x", None).is_none());
        assert!(maybe_line(Level::Error, Level::Info, "x", Some(("ACCESS", "1.2.3"))).is_none());
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;

    /// `record_drop()` (the shared counter for BOTH `Full` arms of
    /// `push_line`/`push_plaintext`) increments the monotonic
    /// `clarus_log_dropped` metric. No real channel overflow is needed -
    /// we exercise the counter directly.
    #[test]
    fn record_drop_increments_log_dropped_counter() {
        let before = crate::metrics::CLARUS_LOG_DROPPED.load(std::sync::atomic::Ordering::Relaxed);
        record_drop();
        record_drop();
        let after = crate::metrics::CLARUS_LOG_DROPPED.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(after - before, 2, "two record_drop() calls -> +2");
    }
}
