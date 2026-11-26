// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Access-audit journal (ATNA-lite per GOST R ISO 27789-2016 / DICOM PS3.15).
//!
//! A SECOND, separate journal (`access/YYYYMMDD.log`, sibling of the
//! delete-only `audit/`), JSON lines, one line per access/query event.
//! Access-audit journal: bounded file, per-minute buckets, redaction.
//!
//! # Contract
//! - MINIMIZATION: identifiers only - user, IPs, UIDs, PatientID, query text.
//!   Never names, never clinical content, never pixel metadata.
//! - One central emission hook in the serve loop after route classification;
//!   the request path does ONE bounded-channel `try_send` (zero syscalls).
//! - Writer thread: batches up to 1 s / 64 KiB into ONE append+flush; fsync
//!   every batch; handle kept across the day, rotation closes + reopens.
//!   Ring 4096 lines; on Full -> `access_audit_dropped` + drop silently.
//!   A crash loses at most the last unflushed batch (<= ~1 s) - bounded loss
//!   is DISCLOSED (availability-first, unlike the fsync-per-record delete
//!   trail).
//! - Graceful shutdown: all senders dropped -> the writer drains and exits.
//! - Pruner: only `access/*.log` (8-digit dates); NEVER touches audit_dir
//!   (structural: this module has no code path to it); never deletes today's
//!   file; INFO `ACCESS_ROTATE` per file; `access_prune_dry_run` flag.
//! - Perms: same 0600/0700 discipline as the delete trail; the startup
//!   enforcement is shared (main.rs calls the SAME helper for both dirs).
//! - Self-audit: GET /admin/v1/audit-log and /admin/v1/access-log append a
//!   110101 line; the reader sees its own reads on the next query.
//! - The trail is NOT tamper-proof (same filesystem) - OS-level controls are
//!   docs/ops recipes. Hash-chain per line: DEFERRED (schema-compatible).

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::handlers::json_common::{push_json_str_loose, push_u64};

/// Ring capacity: 4096 lines (~1 MiB at the typical ~250 B line).
const RING_SIZE: usize = 4096;
/// Writer batch ceiling: 64 KiB of buffered lines.
const BATCH_BYTES: usize = 64 * 1024;
/// Writer idle flush: at least one flush per second.
const BATCH_SECS: u64 = 1;
/// `q` field cap (query text is the only unbounded field; the whole line
/// stays under the 2 KiB budget by construction).
const Q_CAP: usize = 1024;
/// Anonymous-window bucket count (one per minute, 60 = one hour).
const ANON_WINDOW_MINUTES: usize = 60;

/// One access-journal event (design section 2 field set).
#[derive(Debug, Clone)]
pub struct AccessEvent {
    /// Event class: `access` (request/query) | `attribute_update` (U).
    pub event: &'static str,
    /// UTC ISO-8601 second resolution, e.g. `2026-08-28T00:17:12Z`.
    pub ts: String,
    /// Effective user (`X-Authenticated-User` / `(miniviewer)` /
    /// `(loopback)` / `anonymous`).
    pub user: String,
    /// Effective client IP.
    pub src_ip: String,
    /// `nginx` | `loopback` | `bridge`.
    pub via: &'static str,
    /// GOST 27789 Table 1 action: `R` | `C` (U/D/E reserved).
    pub action: &'static str,
    /// ATNA EventID: 110101 / 110103 / 110104 / 110110 / 110112.
    pub atna: &'static str,
    /// HTTP status after the response.
    pub status: u16,
    pub study: String,
    pub series: String,
    pub sop: String,
    /// PatientID only (empty when the request path carries none).
    pub patient: String,
    /// ParticipantObjectIDTypeCode: "2" (PatientID) / "110180" (DICOM UID) / "".
    pub obj_type: &'static str,
    /// QIDO query text (verbatim, capped 1024 bytes); empty otherwise.
    pub q: String,
    /// Instances in a STOW batch (1 for everything else).
    pub count: u64,
    /// U events only: count of (0400,0561) items embedded (0 = pixel-only
    /// replacement). Never carries the old values.
    pub changed_attrs: u64,
    /// U events only: 1 when the stored bytes were replaced, else 0.
    pub bytes_replaced: u64,
    /// U events only: the change reason ("coerce_insert" |
    /// "hidden_phi_excised" | "" for plain CORRECT/COERCE updates). Empty
    /// renders no field - existing lines stay byte-identical.
    pub reason: &'static str,
}

// -- Writer channel ------------------------------------------------------

static TX: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();

/// Install the writer channel (called once by [`init`]).
fn set_sender(tx: SyncSender<Vec<u8>>) {
    let _ = TX.set(tx);
}

/// Handle kept alive for the process lifetime; dropping all handles drains
/// the ring (graceful shutdown) and lets the writer thread exit.
pub struct AccessWriter {
    tx: SyncSender<Vec<u8>>,
}

impl AccessWriter {
    /// Flush the journal and stop the writer, then wait for it.
    ///
    /// Dropping the sender only guarantees the ring is drained *eventually*:
    /// the writer is a detached thread, and `main` returns immediately after
    /// the last request, so the tail of the access trail - everything the
    /// batch window had not yet flushed - went down with the process. The
    /// journal is the record of who read what, so its last lines are exactly
    /// the ones an investigation asks for.
    ///
    /// `TX` is a `OnceLock`, so the static sender is never dropped and the
    /// writer would never see `Disconnected` on its own. The empty-message
    /// sentinel is therefore the shutdown signal: the writer flushes and
    /// returns on it (an empty line can never be a real event, since every
    /// event renders at least its fixed fields plus a newline).
    pub fn shutdown(self) {
        // The deadline starts BEFORE the sentinel is queued. `send` blocks
        // while the ring is full, and the ring is most likely to be full
        // exactly when the writer is stalled in `sync_all` - so measuring the
        // wait from after `send` returned meant the barrier could block
        // indefinitely precisely when it was needed. Queue with `try_send`
        // under the deadline instead, retrying until the sentinel fits.
        let deadline = Instant::now() + Duration::from_secs(SHUTDOWN_FLUSH_SECS);
        loop {
            match self.tx.try_send(Vec::new()) {
                Ok(()) => break,
                // A full ring means the backlog is still being drained. That is
                // not the writer stopping, so keep waiting: this is the tail
                // the barrier exists to save.
                Err(TrySendError::Full(_)) => {
                    if Instant::now() >= deadline {
                        crate::clog_warn!(
                            "access writer could not be signalled within {SHUTDOWN_FLUSH_SECS}s \
                             (ring full); the queued tail may be lost"
                        );
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                // The receiver is gone, so the writer has already returned.
                Err(TrySendError::Disconnected(_)) => {
                    crate::clog_debug!("access writer already stopped");
                    return;
                }
            }
        }

        // The sentinel is QUEUED, not written. Waiting for the writer to
        // observe it is what makes this a flush barrier: the writer returns
        // only after its final flush, and the channel then reports
        // `Disconnected`.
        while Instant::now() < deadline {
            match self.tx.try_send(Vec::new()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return,
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        crate::clog_warn!("access writer did not stop within {SHUTDOWN_FLUSH_SECS}s");
    }
}

/// How long shutdown waits for the access journal to flush.
///
/// The writer flushes on the sentinel immediately, so this only bounds a
/// blocked disk: 5s is long enough for a stalled append to fail and short
/// enough that a shutdown does not look hung.
const SHUTDOWN_FLUSH_SECS: u64 = 5;

/// Initialize the journal: create the dir, spawn the writer thread, install
/// the channel. The writer runs the startup prune, then drains the ring.
pub fn init(dir: &Path, retention_days: u64, prune_dry_run: bool) -> AccessWriter {
    let _ = std::fs::create_dir_all(dir);
    let (tx, rx) = sync_channel::<Vec<u8>>(RING_SIZE);
    let dir = dir.to_path_buf();
    std::thread::Builder::new()
        .name("access-writer".into())
        .spawn(move || writer_loop(rx, &dir, retention_days, prune_dry_run))
        .expect("Failed to spawn access writer thread");
    set_sender(tx.clone());
    AccessWriter { tx }
}

/// True when the writer channel is installed (pre-init no-op guard).
fn is_ready() -> bool {
    TX.get().is_some()
}

/// Queue one event (the ONLY request-path call; never blocks, never I/O).
pub fn emit(e: &AccessEvent) {
    if !is_ready() {
        return;
    }
    let mut line = Vec::with_capacity(512);
    // 512, not 256: the minimal access line measures EXACTLY
    // 256 bytes (255 + newline), so a 256 buffer reallocated on every emit.
    // 512 covers the minimal line, the U line and short-q QIDO lines without
    // a realloc; max-q lines (2 KiB cap) still grow once.
    render_event(&mut line, e);
    line.push(b'\n');
    let tx = TX.get().expect("is_ready checked");
    match tx.try_send(line) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            // Bounded loss (disclosed): count the drop, warn rate-limited,
            // never block a request.
            crate::metrics::ACCESS_AUDIT_DROPPED.fetch_add(1, Ordering::Relaxed);
            static LAST_DROP_WARN: AtomicU64 = AtomicU64::new(0);
            let now = unix_secs();
            let last = LAST_DROP_WARN.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= 60
                && LAST_DROP_WARN
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                crate::clog_warn!(
                    dropped = crate::metrics::ACCESS_AUDIT_DROPPED.load(Ordering::Relaxed),
                    "access audit dropped events (ring full; bounded loss)"
                );
            }
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

// -- Writer thread -------------------------------------------------------

/// Runs on the dedicated `access-writer` thread.
///
/// Batches drained lines up to 1 s / 64 KiB into ONE append+flush to today's
/// file; keeps the handle across the day (rotation = close + reopen); prunes
/// at startup and on every day boundary. Exits when the channel disconnects
/// (all senders dropped = graceful drain).
fn writer_loop(rx: Receiver<Vec<u8>>, dir: &Path, retention_days: u64, dry_run: bool) {
    // Startup prune (P1), then a day-boundary prune on every rotation.
    prune(dir, retention_days, dry_run);

    let mut current_day = String::new();
    let mut file: Option<std::fs::File> = None;
    let mut batch: Vec<u8> = Vec::with_capacity(BATCH_BYTES);
    let mut last_flush = Instant::now();
    // The UTC day the CURRENTLY BUFFERED lines belong to.
    //
    // The rotation branch drops the handle when the date advances, but the
    // bytes already in `batch` were produced under the OLD day - they carry
    // their own `ts` and an auditor looks them up by that date. Flushing them
    // through the new day's handle put yesterday's records in today's file,
    // where the lookup cannot find them. The batch therefore remembers its day
    // and is persisted to that day's file before the handle is released.
    let mut batch_day = String::new();
    // Whether anything queued before the shutdown sentinel failed to reach the
    // medium. Reported once on the way out, so the operator sees the loss even
    // though the ring drained normally.
    let mut shutdown_ok = true;

    loop {
        let now_day = crate::audit_log::utc_ymd();
        if now_day != current_day {
            // Rotation. The buffered lines belong to the OLD day, so they are
            // persisted to that day's file BEFORE the handle is released;
            // otherwise they would land in the new day's file and an auditor
            // looking them up under their own `ts` would not find them.
            if rotation_needs_old_day_flush(!batch.is_empty(), &batch_day, &now_day) {
                flush_batch_to_day(&mut file, dir, &mut batch, &batch_day);
            }
            file = None;
            current_day = now_day;
            prune(dir, retention_days, dry_run);
        }
        // The batch deadline is fixed when the batch STARTS, not recomputed per
        // line. Recomputing it made the deadline always one second in the
        // future, so under a steady stream of lines it never arrived and the
        // batch only ever flushed on size - a busy server could hold the tail
        // of its audit trail in memory indefinitely, and a crash lost it.
        let deadline = last_flush + Duration::from_secs(BATCH_SECS);
        let got = rx.recv_timeout(Duration::from_millis(20));
        match got {
            Ok(line) => {
                if line.is_empty() {
                    // Shutdown sentinel: flush what is queued and stop. See
                    // `AccessWriter::shutdown` for why the channel cannot
                    // reveal shutdown by disconnecting. The final flush is the
                    // one an investigation depends on, so a failure here must
                    // not be reported as a clean drain.
                    if !batch.is_empty() && !flush_batch(&mut file, dir, &mut batch) {
                        shutdown_ok = false;
                    }
                    if shutdown_ok {
                        crate::clog_debug!("access journal drained on shutdown");
                    }
                    return;
                }
                // The first line of a batch fixes its day, so a batch that
                // straddles midnight is persisted to the day its records name.
                if batch.is_empty() {
                    batch_day.clone_from(&current_day);
                }
                batch.extend_from_slice(&line);
                if batch.len() >= BATCH_BYTES || Instant::now() >= deadline {
                    flush_batch(&mut file, dir, &mut batch);
                    last_flush = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() && Instant::now() >= deadline {
                    flush_batch(&mut file, dir, &mut batch);
                    last_flush = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if !batch.is_empty() {
                    flush_batch(&mut file, dir, &mut batch);
                }
                return;
            }
        }
    }
}

/// Whether a rotation must first persist the buffered lines to their own day.
///
/// Extracted so the decision is testable: the bug was not "the flush wrote the
/// wrong bytes" but "the flush was never attempted for a straddling batch",
/// which a test that calls the flush directly cannot catch.
fn rotation_needs_old_day_flush(batch_nonempty: bool, batch_day: &str, now_day: &str) -> bool {
    batch_nonempty && batch_day != now_day
}

/// Write a batch to a NAMED day's file, used at a rotation boundary.
///
/// The batch carries records produced before midnight, so it goes to the file
/// named by `day` even though the writer has already rolled to the new day.
/// Like [`flush_batch`], a failure is counted rather than swallowed.
fn flush_batch_to_day(
    file: &mut Option<std::fs::File>,
    dir: &Path,
    batch: &mut Vec<u8>,
    day: &str,
) -> bool {
    if batch.is_empty() {
        return true;
    }
    let path = dir.join(format!("{day}.log"));
    let mut f = match open_day(dir, day) {
        Some(f) => f,
        None => {
            record_persistence_failure(batch.len());
            batch.clear();
            return false;
        }
    };
    let written = write_journal_batch(&mut f, batch).and_then(|()| f.sync_all());
    // The handle kept in `file` belongs to the NEW day, so the old batch uses
    // its own short-lived one and leaves the writer's state alone.
    let _ = file;
    if let Err(e) = written {
        record_persistence_failure(batch.len());
        crate::clog_error!(
            "access journal: batch for {day} not persisted ({e}); {} bytes lost",
            batch.len()
        );
        batch.clear();
        return false;
    }
    let _ = path;
    batch.clear();
    true
}

/// Open `dir/{day}.log` append-only with the same 0600 discipline as today's.
fn open_day(dir: &Path, day: &str) -> Option<std::fs::File> {
    let path = dir.join(format!("{day}.log"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .ok()
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    }
}

/// ONE append + fsync of the accumulated batch to `dir/YYYYMMDD.log`.
///
/// Returns whether the batch reached the medium. Every failure used to be
/// discarded here (`let _ = write_all`, `let _ = sync_all`, and a cleared
/// batch when the open failed), so an unwritable journal looked exactly like
/// a healthy one from the outside: the ring drained, no counter moved, and an
/// investigation asking for a period would find nothing and be told nothing.
/// The batch is still cleared - retrying it forever would spin on a medium
/// that is not coming back - but the loss is now COUNTED and logged, and the
/// caller can tell the difference between "written" and "dropped".
fn flush_batch(file: &mut Option<std::fs::File>, dir: &Path, batch: &mut Vec<u8>) -> bool {
    if batch.is_empty() {
        return true;
    }
    if file.is_none() {
        *file = open_today(dir);
    }
    let Some(f) = file.as_mut() else {
        // Open failed (dir gone, permissions): the batch cannot be written.
        // Counting it is the point - a silent clear is how the trail came to
        // have holes nobody knew about.
        record_persistence_failure(batch.len());
        batch.clear();
        // The handle stays `None`, so the next batch retries the open.
        return false;
    };
    let written = write_journal_batch(f, batch).and_then(|()| f.sync_all());
    if let Err(e) = written {
        record_persistence_failure(batch.len());
        crate::clog_error!(
            "access journal: batch not persisted ({e}); {} bytes lost",
            batch.len()
        );
        // A failed append or fsync leaves the file in an unknown state at the
        // record boundary (a partial line may have landed). Dropping the
        // handle makes the next batch reopen, and a partial trailing line is
        // what the reader's own line framing already tolerates - appending
        // more onto the SAME handle without knowing the offset is what would
        // corrupt a following record.
        *file = None;
        batch.clear();
        return false;
    }
    batch.clear();
    true
}

/// Write one batch, test-only seam at the front.
///
/// `std::fs::File` cannot be told to fail on demand, so the persistence-failure
/// branch above was untestable; this is the single place the seam is read.
///
/// The seam fires on the ARMING thread only (`take_failure_on_arming_thread`),
/// exactly like `audit_log.rs`'s append seam. The journal's writer runs on its
/// own thread, and the writer-loop tests do not arm the seam; with a plain
/// process-wide `take_failure`, a concurrent durability test arming the site
/// had its failure stolen by the writer thread and both tests broke.
fn write_journal_batch(f: &mut std::fs::File, batch: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if crate::io_fault::take_failure_on_arming_thread(crate::io_fault::SITE_ACCESS_JOURNAL)
        .is_some()
    {
        return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
    }
    f.write_all(batch)
}

/// Count one batch the journal could not persist.
///
/// Separate from `ACCESS_AUDIT_DROPPED` (ring full, a bounded-loss design
/// decision): this one is a FAILURE - the record was accepted and then lost -
/// and an operator must be able to alert on the difference.
fn record_persistence_failure(bytes: usize) {
    crate::metrics::ACCESS_AUDIT_WRITE_FAILED.fetch_add(1, Ordering::Relaxed);
    crate::metrics::ACCESS_AUDIT_WRITE_FAILED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// Open `dir/YYYYMMDD.log` append-only, 0600 on Unix (same discipline as the
/// delete trail), keep across the day.
fn open_today(dir: &Path) -> Option<std::fs::File> {
    let path = today_log_path(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .ok()
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    }
}

/// `dir/YYYYMMDD.log` for the current UTC date (same civil-date math as the
/// delete trail; no date crate).
fn today_log_path(dir: &Path) -> PathBuf {
    dir.join(format!("{}.log", crate::audit_log::utc_ymd()))
}

// -- Retention pruner ----------------------------------------------------

/// Delete `access/YYYYMMDD.log` files older than `retention_days`
/// (0 = keep forever). Structural guarantees:
/// - only `*.log` files with an 8-digit `YYYYMMDD` stem are ever considered;
/// - never deletes today's file (or anything dated in the future);
/// - has NO code path to `audit_dir` - this function can only ever be called
///   with the access dir.
pub fn prune(dir: &Path, retention_days: u64, dry_run: bool) {
    if retention_days == 0 {
        return; // keep forever
    }
    let today = crate::audit_log::utc_ymd();
    let Some(today_days) = ymd_to_days(&today) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue; // refuse non-*.log entries
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.len() != 8 || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue; // refuse non-YYYYMMDD names
        }
        if name.as_bytes() >= today.as_bytes() {
            continue; // never delete today's (or future) file
        }
        let Some(day_days) = ymd_to_days(name) else {
            continue;
        };
        // The age is compared POSITIVE and in u64. `retention_days as i64`
        // turned a configured `u64::MAX` into -1, so every file looked "within
        // retention" and nothing was ever pruned - the opposite of what the
        // operator asked for. A value that cannot be represented as an age is
        // treated as "keep forever", which is the safe direction.
        let age = match today_days.checked_sub(day_days) {
            Some(a) if a > 0 => a as u64,
            // Today's or a future file: guarded above, but never delete on an
            // arithmetic surprise.
            _ => continue,
        };
        if age <= retention_days {
            continue; // within retention
        }
        if dry_run {
            crate::clog_event!(
                crate::clog::Level::Info,
                "ACCESS_ROTATE",
                "",
                "dry-run: would delete {}",
                path.display()
            );
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => crate::clog_event!(
                crate::clog::Level::Info,
                "ACCESS_ROTATE",
                "",
                "pruned {}",
                path.display()
            ),
            Err(e) => crate::clog_warn!("ACCESS_ROTATE: cannot delete {}: {e}", path.display()),
        }
    }
}

/// Parse `YYYYMMDD` into days since Unix epoch (inverse of `civil_from_days`,
/// Howard Hinnant's `days_from_civil`).
fn ymd_to_days(ymd: &str) -> Option<i64> {
    let y: i64 = ymd.get(0..4)?.parse().ok()?;
    let m: i64 = ymd.get(4..6)?.parse().ok()?;
    let d: i64 = ymd.get(6..8)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

// -- JSON render (byte-exact, no serde) -----------------------------------

/// Render one event as `{"event":"access",...}` (field order = design
/// section 2 example).
///
/// Pub for the ap101 gate: a warm 256-byte Vec renders a minimal
/// access line with 0 allocations (0-alloc render helpers + no realloc).
pub fn render_event(out: &mut Vec<u8>, e: &AccessEvent) {
    out.extend_from_slice(b"{\"event\":");
    push_json_str_loose(out, e.event);
    w_str(out, "ts", &e.ts);
    // Every field below can come straight from the request. The 1024-byte `q`
    // cap bounded one of them; a long identity header or path segment could
    // still blow the whole-line budget on its own, so each is capped.
    w_str(out, "user", cap_field(&e.user));
    w_str(out, "role", "");
    w_str(out, "src", "clarus-server");
    w_str(out, "src_ip", cap_field(&e.src_ip));
    w_str(out, "via", e.via);
    w_str(out, "action", e.action);
    w_str(out, "atna", e.atna);
    w_u64(out, "status", e.status as u64);
    w_str(out, "study", cap_field(&e.study));
    w_str(out, "series", cap_field(&e.series));
    w_str(out, "sop", cap_field(&e.sop));
    w_str(out, "patient", cap_field(&e.patient));
    w_str(out, "obj_type", e.obj_type);
    // The query text is the only unbounded field: cap at render time so the
    // whole line stays under the 2 KiB cap by construction.
    w_str(out, "q", &cap_q(e.q.as_bytes()));
    w_u64(out, "count", e.count);
    // U events carry the two update-specific counters; regular access lines
    // stay byte-identical to the W2 schema.
    if e.action == "U" {
        w_u64(out, "changed_attrs", e.changed_attrs);
        w_u64(out, "bytes_replaced", e.bytes_replaced);
        if !e.reason.is_empty() {
            w_str(out, "reason", e.reason);
        }
    }
    out.push(b'}');
}

fn w_str(out: &mut Vec<u8>, name: &str, v: &str) {
    out.extend_from_slice(b",\"");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\":");
    push_json_str_loose(out, v);
}

fn w_u64(out: &mut Vec<u8>, name: &str, v: u64) {
    out.extend_from_slice(b",\"");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\":");
    push_u64(out, v);
}

// -- Central emission hook ------------------------------------------------

/// ONE central hook: called from the serve loop after route classification,
/// once the response status is known. Maps the route to the design section 1
/// event table and queues the journal line. Also feeds the identity counters
/// and the anonymous-window gauge (design section 9).
///
/// `uids`/`n_uids` are the router-captured path segments (study/series/sop);
/// `q` is the raw query string (verbatim - it is the auditor's evidence).
#[allow(clippy::too_many_arguments)]
pub fn record_request(
    route: &str,
    query: &[u8],
    status: u16,
    user: &[u8],
    ip: &[u8],
    via: &'static str,
    uids: &[&[u8]],
    n_uids: usize,
    stow_count: u64,
) {
    let uid = |i: usize| -> String {
        if i < n_uids {
            String::from_utf8_lossy(uids[i]).into_owned()
        } else {
            String::new()
        }
    };

    let Some((action, atna, study, series, sop, obj_type, q, count)) =
        classify(route, query, &uid, stow_count)
    else {
        return;
    };

    let user_s = String::from_utf8_lossy(user).into_owned();
    // Identity counters + anonymous window (design section 9b/9c). The hook
    // only reaches here for protected endpoints, so "anonymous" below means
    // exactly "anonymous protected request".
    if user == b"anonymous" {
        crate::metrics::ACCESS_ANONYMOUS_TOTAL.fetch_add(1, Ordering::Relaxed);
        note_anonymous_protected();
    } else {
        crate::metrics::ACCESS_IDENTIFIED_TOTAL.fetch_add(1, Ordering::Relaxed);
    }

    let ev = AccessEvent {
        event: "access",
        ts: utc_ts_iso(),
        user: user_s,
        src_ip: String::from_utf8_lossy(ip).into_owned(),
        via,
        action,
        atna,
        status,
        study,
        series,
        sop,
        patient: String::new(), // no PatientID is in hand at the hook
        obj_type,
        q,
        count,
        changed_attrs: 0,
        bytes_replaced: 0,
        reason: "",
    };
    emit(&ev);
}

/// The classified fields of one access event:
/// `(action, atna, study, series, sop, obj_type, q, count)`.
type RouteFields = (
    &'static str,
    &'static str,
    String,
    String,
    String,
    &'static str,
    String,
    u64,
);

/// The route-to-event table of the access journal.
///
/// Split out of [`record_request`] so the mapping can be asserted directly,
/// without a process-global writer channel: the decision is a pure function of
/// the route, the query string and the captured UIDs.
///
/// Returns `None` for a route that produces no access event. Deletes and
/// erases are not here: they live in the delete trail (`audit/`), which this
/// module never touches.
fn classify(
    route: &str,
    query: &[u8],
    uid: &dyn Fn(usize) -> String,
    stow_count: u64,
) -> Option<RouteFields> {
    let fields = match route {
        // WADO family - retrieve access, 110103. The router validates UIDs
        // ([0-9.]*), so the captured segments are the object identifiers.
        "wado_instance" | "wado_uri" | "wado_frame" | "wado_frame_thumbnail" | "wado_bulkdata" => (
            "R",
            crate::audit_codes::event_id::INSTANCES_ACCESSED,
            uid(0),
            uid(1),
            uid(2),
            crate::audit_codes::object_id_type::STUDY_INSTANCE_UID,
            String::new(),
            1u64,
        ),
        "wado_series" | "wado_series_metadata" | "wado_series_thumbnail" => (
            "R",
            crate::audit_codes::event_id::INSTANCES_ACCESSED,
            uid(0),
            uid(1),
            String::new(),
            crate::audit_codes::object_id_type::STUDY_INSTANCE_UID,
            String::new(),
            1u64,
        ),
        "wado_study" | "wado_study_metadata" | "wado_study_thumbnail" | "wado_metadata" => (
            "R",
            crate::audit_codes::event_id::INSTANCES_ACCESSED,
            uid(0),
            String::new(),
            String::new(),
            crate::audit_codes::object_id_type::STUDY_INSTANCE_UID,
            String::new(),
            1u64,
        ),
        "wado_thumbnail" => (
            "R",
            crate::audit_codes::event_id::INSTANCES_ACCESSED,
            uid(0),
            uid(1),
            uid(2),
            crate::audit_codes::object_id_type::STUDY_INSTANCE_UID,
            String::new(),
            1u64,
        ),
        "wado_rendered" | "wado_frame_rendered" => (
            "R",
            crate::audit_codes::event_id::INSTANCES_ACCESSED,
            uid(0),
            uid(1),
            uid(2),
            crate::audit_codes::object_id_type::STUDY_INSTANCE_UID,
            String::new(),
            1u64,
        ),
        // QIDO family - query access. Patients = 110110 (patient record),
        // study/series/instance searches = 110112 (query).
        "qido_patients" => (
            "R",
            crate::audit_codes::event_id::PATIENT_RECORD,
            String::new(),
            String::new(),
            String::new(),
            "",
            cap_q(query),
            1u64,
        ),
        "qido_studies"
        | "qido_study_instances"
        | "qido_series"
        | "qido_series_instances"
        | "qido_series_global"
        | "qido_series_root_instances"
        | "qido_instances_global" => (
            crate::audit_codes::action::EXECUTE,
            crate::audit_codes::event_id::QUERY,
            String::new(),
            String::new(),
            String::new(),
            "",
            cap_q(query),
            1u64,
        ),
        // STOW ingest - create, 110104, count = accepted instances. The
        // study-scoped target (`/studies/{study}`) is the same transaction
        // with a mandatory study match, so it maps to the same event.
        "stow_studies" | "stow_study" => (
            "C",
            crate::audit_codes::event_id::INSTANCES_TRANSFERRED,
            String::new(),
            String::new(),
            String::new(),
            "",
            String::new(),
            stow_count,
        ),
        // MWL Search - a Query. PS3.15 A.5.3.10 (Query) names Modality
        // Worklist among the SOP classes it reports, so the search maps to
        // 110112. Its action is E, not R: A.5 mandates `E` (Execute) for the
        // Query event, and an action outside that set is not conformant even
        // though the EventID is right. The query text is the auditor's
        // evidence and is carried verbatim under the same 1024-byte cap as the
        // QIDO searches.
        "mwl_search" => (
            crate::audit_codes::action::EXECUTE,
            crate::audit_codes::event_id::QUERY,
            String::new(),
            String::new(),
            String::new(),
            "",
            cap_q(query),
            1u64,
        ),
        // MPPS - a Procedure Record. PS3.15 A.5.3.15 names MPPS update
        // explicitly, and the actions map C/U/R. Create and Update share one
        // route id (PS3.18 15.4/15.5 differ only by the `?update` parameter,
        // which the router cannot see), so the action is read from the query
        // here: the same `?update` marker that tells the dispatcher which
        // transaction this is tells the auditor which action it recorded.
        // The MPPS UID is the path segment (a DICOM UID, object type 110180).
        "mpps_create_or_update" => {
            let is_update = query
                .split(|&b| b == b'&')
                .any(|kv| kv == b"update" || kv.starts_with(b"update="));
            if is_update {
                (
                    "U",
                    crate::audit_codes::event_id::PROCEDURE_RECORD,
                    String::new(),
                    String::new(),
                    uid(0),
                    crate::audit_codes::object_id_type::STUDY_INSTANCE_UID,
                    String::new(),
                    1u64,
                )
            } else {
                (
                    "C",
                    crate::audit_codes::event_id::PROCEDURE_RECORD,
                    String::new(),
                    String::new(),
                    uid(0),
                    crate::audit_codes::object_id_type::STUDY_INSTANCE_UID,
                    String::new(),
                    1u64,
                )
            }
        }
        "mpps_get" => (
            "R",
            crate::audit_codes::event_id::PROCEDURE_RECORD,
            String::new(),
            String::new(),
            uid(0),
            crate::audit_codes::object_id_type::STUDY_INSTANCE_UID,
            String::new(),
            1u64,
        ),
        // Self-audit (GOST 27789 9.4): journal reads record themselves.
        "admin_audit_log" => (
            "R",
            crate::audit_codes::event_id::AUDIT_LOG_USED,
            String::new(),
            String::new(),
            String::new(),
            "",
            String::new(),
            1u64,
        ),
        "admin_access_log" => (
            "R",
            crate::audit_codes::event_id::AUDIT_LOG_USED,
            String::new(),
            String::new(),
            String::new(),
            "",
            String::new(),
            1u64,
        ),
        // Everything else (health, metrics, unknown, delete/erase admin):
        // no access event. Deletes live in the delete trail only.
        _ => return None,
    };
    Some(fields)
}

/// 2808 update-in-place: queue the attribute-update line for one replaced
/// instance. Called from the STOW handlers (the dispatch hook's U-event
/// emitter); `try_send`, never blocks, identifiers only - the OLD attribute
/// values live exclusively inside the stored file's (0400,0561).
#[allow(clippy::too_many_arguments)]
pub fn emit_update(
    user: &[u8],
    ip: &[u8],
    via: &'static str,
    study: &str,
    series: &str,
    sop: &str,
    patient: &str,
    changed_attrs: usize,
    bytes_replaced: bool,
    reason: &'static str,
) {
    if !is_ready() {
        return;
    }
    let ev = AccessEvent {
        event: "attribute_update",
        ts: utc_ts_iso(),
        user: String::from_utf8_lossy(user).into_owned(),
        src_ip: String::from_utf8_lossy(ip).into_owned(),
        via,
        action: crate::audit_codes::action::UPDATE,
        atna: crate::audit_codes::event_id::INSTANCES_ACCESSED,
        status: 200,
        study: study.to_string(),
        series: series.to_string(),
        sop: sop.to_string(),
        patient: patient.to_string(),
        obj_type: crate::audit_codes::object_id_type::PATIENT_NUMBER,
        q: String::new(),
        count: 1,
        changed_attrs: changed_attrs as u64,
        bytes_replaced: u64::from(bytes_replaced),
        reason,
    };
    emit(&ev);
}

/// Cap a free-form identifier field at a fixed number of BYTES.
///
/// The 1024-byte query cap bounded `q`, but the other fields were taken
/// straight from the request: a 2 KiB printable `X-Authenticated-User` blew the
/// whole-line budget on its own, with the fixed fields still to come. The budget
/// is what keeps a batch bounded and a reader cheap, so every variable field
/// must respect it. Path/identity values are identifiers, so truncation on a
/// char boundary is a disclosure decision the auditor can live with - and the
/// JSON escaping is accounted for because the cap is applied to the VALUE, with
/// the rendered line still checked against the budget below.
const FIELD_CAP: usize = 256;

fn cap_field(value: &str) -> &str {
    if value.len() <= FIELD_CAP {
        return value;
    }
    let mut end = FIELD_CAP;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Cap the query text at 1024 BYTES on a char boundary (line stays under the
/// 2 KiB whole-line cap by construction). PHI name values are masked BEFORE
/// capping (docs/audit-logging.md s2: names out, identifiers visible).
fn cap_q(query: &[u8]) -> String {
    let s = String::from_utf8_lossy(query);
    let masked = mask_query_phi(&s);
    let m = masked.as_str();
    if m.len() <= Q_CAP {
        return m.to_string();
    }
    let mut out = String::with_capacity(Q_CAP);
    for ch in m.chars() {
        if out.len() + ch.len_utf8() > Q_CAP {
            break;
        }
        out.push(ch);
    }
    out
}

// -- PHI masking of query text (both journal surfaces) ----------------------

/// NAME attributes whose VALUES are personal data and must be masked in
/// BOTH journal surfaces (the INFO ACCESS line and the JSON `q` field).
/// docs/audit-logging.md s2: names out; s3: identifiers (PatientID,
/// AccessionNumber) stay VISIBLE - the auditor must see whose data was
/// searched. Both the DICOM tag-number form and the keyword form are
/// matched (QIDO clients may send either), case-insensitively.
///
/// The list is the PS3.6 "Person Name" population of the QIDO matching keys,
/// not just PatientName: `fuzzymatching` against any of them returns the same
/// personal data, and one unmasked key is enough to defeat the rule (PS3.18
/// Table 6.7.1-1 lists all of them as matchable).
fn is_name_key(key: &str) -> bool {
    // The wire spelling may be percent-encoded (`patient%4Eame`) and the
    // comparison must survive that: %4E is `N`, so a raw comparison would let
    // the value through. Decode BEFORE the lookup - the same order a receiver
    // applies, since the server matches on the decoded key.
    //
    // The decoded form is only built when the key actually carries an escape:
    // this runs once per journal line on the request path, and the plain
    // spelling is the common case.
    let decoded;
    let key = if key.as_bytes().iter().any(|b| *b == b'%' || *b == b'+') {
        decoded = percent_decode(key);
        decoded.as_str()
    } else {
        key
    };

    // A SEQUENCE-ATTRIBUTE key may be dotted: PS3.18 permits
    // `ScheduledProcedureStepSequence.ScheduledPerformingPhysicianName=...`,
    // and a classified MWL request can therefore carry a Person Name under a
    // path the flat comparison below would never see. The value belongs to the
    // TERMINAL attribute, so that is what the name rule must classify -
    // matching on the whole dotted key (as this did) only ever matched a flat
    // list entry and let every nested name through.
    let terminal = match key.rfind('.') {
        Some(dot) => &key[dot + 1..],
        None => key,
    };
    name_key_matches(terminal.bytes())
}

/// Does this key name a Person Name attribute?
///
/// Compared as bytes with ASCII lower-casing applied on the fly: building a
/// lower-cased `String` here would allocate once per query key on the request
/// path. A `,` is skipped because the canonical PS3.6 tag spelling
/// (`0010,0010`) and the compact one (`00100010`) name the same attribute.
fn name_key_matches(bytes: impl Iterator<Item = u8>) -> bool {
    // 32 covers the longest key above (`nameofphysiciansreadingstudy`, 28
    // bytes) with margin; anything longer cannot be one of them.
    let mut buf = [b' '; 32];
    let mut n = 0;
    for b in bytes {
        if b == b',' {
            continue;
        }
        if n == buf.len() {
            return false;
        }
        buf[n] = b.to_ascii_lowercase();
        n += 1;
    }
    let Ok(k) = std::str::from_utf8(&buf[..n]) else {
        return false;
    };
    matches!(
        k,
        // Person Name keys (PS3.6: VR = PN, and PS3.18 Table 6.7.1-1 makes
        // each of them matchable, so one unmasked key defeats the rule).
        "00100010"
            | "patientname"
            | "00080090"
            | "referringphysicianname"
            | "00081048"
            | "physiciansofrecord"
            | "00081050"
            | "performingphysicianname"
            | "00400006"
            | "scheduledperformingphysicianname"
            | "00081060"
            | "nameofphysiciansreadingstudy"
            | "00081070"
            | "operatorsname"
            | "00081080"
            | "admittingdiagnosesdescription"
            | "00101001"
            | "otherpatientnames"
            | "00101005"
            | "patientbirthname"
            | "00102180"
            | "occupation"
            | "001021b0"
            | "additionalpatienthistory"
            // Free-text keys that routinely carry a name or a narrative.
            | "00101000"
            | "otherpatientids"
            | "00101002"
            | "otherpatientidsequence"
            // (0040,1400) Requested Procedure Comments - the sensitive-text
            // key. The list used to say (0040,0275), which is Request
            // ATTRIBUTES Sequence: a completely different attribute, so the
            // tag-form spelling of the comment field was never masked.
            | "00401400"
            | "requestedprocedurecomments"
            | "00104000"
            | "patientcomments"
    )
}

/// Decode percent-encoding (and `+` as space) for a query key.
///
/// Local on purpose: this is the journal's own sanitizer, and it must not
/// acquire a dependency on the QIDO parser - the masking rule has to hold
/// even for routes the QIDO code never sees.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            // Only take the escape when BOTH digits are present: reading them
            // off a short tail would drop the `%` and its successor.
            if let (Some(hi), Some(lo)) = (bytes.get(i + 1), bytes.get(i + 2))
                && let Ok(v) =
                    u8::from_str_radix(std::str::from_utf8(&[*hi, *lo]).unwrap_or("zz"), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
            // A malformed escape stays literal; it cannot match a key anyway.
            out.push(b);
        } else if b == b'+' {
            out.push(b' ');
        } else {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mask the VALUES of NAME search keys in a query string. The value is
/// ALWAYS replaced with `***` in full: a partial
/// mask leaks - a one-char name is itself an identifier, `a.` reveals the
/// first character. Non-name segments (including PatientID/
/// AccessionNumber values) pass untouched.
/// `pub` rather than `pub(crate)` so the ap101 allocation gate in
/// `tests/ap101_access.rs` can measure it: an integration test is a separate
/// crate and cannot reach a `pub(crate)` item. That is the same visibility the
/// other gate-measured builders in this crate carry.
pub fn mask_query_phi(qs: &str) -> String {
    if qs.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(qs.len());
    for (i, seg) in qs.split('&').enumerate() {
        if i > 0 {
            out.push('&');
        }
        match seg.split_once('=') {
            Some((key, _value)) if is_name_key(key) => {
                // The VALUE is always the same constant: copying the key and
                // then a literal replaces the whole segment with no
                // intermediate allocation.
                out.push_str(key);
                out.push('=');
                out.push_str(PHI_MASK);
            }
            _ => out.push_str(seg),
        }
    }
    out
}

/// What replaces a personal-data value in the journal.
///
/// Constant on purpose: a partial mask leaks - a one-character name is itself
/// an identifier, and `a.` reveals the first character - and a length-varying
/// replacement discloses the length.
const PHI_MASK: &str = "***";

// -- Journal hygiene -------------------------------------------------------

/// Replacement for a control byte that has no safe place in a log line.
const CTL_PLACEHOLDER: char = '?';

/// Replace control bytes in a field bound for the journal.
///
/// The request target is attacker-controlled and reaches the log verbatim:
/// `head.path` and `head.query` are borrowed directly from the wire, and the
/// peer/user fields can carry bytes from headers. A `0x1B` in a log line is a
/// terminal-injection primitive for whoever later reads the file with `cat`
/// or tails it in a terminal; a NUL makes the line unparseable to tools and
/// truncates it in some viewers; a bare CR or LF inside a field can forge what
/// looks like an extra log record.
///
/// Everything below `0x20` (C0 controls, including CR, LF, TAB, ESC, NUL) and
/// `0x7F` (DEL) becomes `?`, and so do the C1 block `U+0080..=U+009F` plus the
/// Unicode line/paragraph separators `U+2028` / `U+2029`. Those extras are not
/// `Cc` but they terminate a line for some readers, which is the same record-
/// forging problem as a raw LF. Printable ASCII and all other non-ASCII UTF-8
/// are left alone: those bytes are legitimate data (Cyrillic names are already
/// part of the journal's contract) and a `?` per byte would corrupt them.
///
/// One helper, called from both journal paths: the per-request `access_line`
/// and the aggregated `WADO_SERIES` lines. Fixing only one of them would leave
/// the other as an open second route for the same bytes.
pub fn sanitize_ctl(s: &str) -> String {
    // Fast path: pure printable-ASCII input cannot contain anything that needs
    // replacing, so the common case allocates only the returned copy. The
    // check is on BYTES, not on `chars`, so a multi-byte character such as NEL
    // (0xC2 0x85) still fails it and takes the slow path below.
    if s.bytes().all(|b| (0x20..0x7F).contains(&b)) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if is_unsafe_in_log(ch) {
            out.push(CTL_PLACEHOLDER);
        } else {
            out.push(ch);
        }
    }
    out
}

/// Is this character unsafe to carry inside a single log line?
///
/// `char::is_control` is the Unicode `Cc` category, which covers C0, C1 and
/// DEL - but NOT `U+0085` (NEL) or `U+2028`/`U+2029`, which are category `Zl`
/// / `Zp` and yet end a line for many readers and editors. Those are exactly
/// the bytes this function exists to stop, so they are named explicitly rather
/// than assumed to fall out of `is_control`.
fn is_unsafe_in_log(ch: char) -> bool {
    ch.is_control() || matches!(ch, '\u{0085}' | '\u{2028}' | '\u{2029}')
}

// -- UTC timestamp --------------------------------------------------------

/// UTC ISO-8601 second resolution `YYYY-MM-DDTHH:MM:SSZ` (GOST 27789 7:
/// EventDateTime; second granularity is the journal's own requirement).
pub fn utc_ts_iso() -> String {
    let secs = unix_secs();
    let days = (secs / 86_400) as i64;
    let (y, mo, d) = crate::audit_log::civil_from_days(days);
    let sod = secs % 86_400;
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// -- Anonymous-window gauge (design 9b) ----------------------------------

static ANON_WINDOW: OnceLock<[AtomicU64; ANON_WINDOW_MINUTES]> = OnceLock::new();
/// The minute each bucket of [`ANON_WINDOW`] currently holds.
///
/// The count alone cannot say how old it is, which is why the gauge kept
/// reporting a single request for ever: nothing knew that bucket's minute had
/// passed. A bucket counts toward the window only when its stamped minute is
/// still inside it, so expiry happens at READ time and does not need traffic.
static ANON_MINUTE: OnceLock<[AtomicU64; ANON_WINDOW_MINUTES]> = OnceLock::new();
static LAST_ANON_MINUTE: AtomicU64 = AtomicU64::new(u64::MAX);
/// Serialises rollover against the increment (see [`note_anonymous_at`]).
static ANON_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn anon_buckets() -> &'static [AtomicU64; ANON_WINDOW_MINUTES] {
    ANON_WINDOW.get_or_init(|| std::array::from_fn(|_| AtomicU64::new(0)))
}

fn anon_minutes() -> &'static [AtomicU64; ANON_WINDOW_MINUTES] {
    ANON_MINUTE.get_or_init(|| std::array::from_fn(|_| AtomicU64::new(0)))
}

/// Cheap per-minute counter ring: one anonymous protected request.
pub fn note_anonymous_protected() {
    let minute = unix_secs() / 60;
    note_anonymous_protected_at_minute(minute);
}

/// The body of [`note_anonymous_protected`] with a caller-supplied minute.
///
/// The whole transition - reading the last minute, clearing the crossed
/// buckets and incrementing - happens under ONE lock. Without it a second
/// request could observe the new minute and increment the slot between the
/// rollover and this call's increment, and the rollover's clear erased that
/// count. Atomic ordering alone cannot make two operations indivisible.
fn note_anonymous_protected_at_minute(minute: u64) {
    let _g = ANON_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let last = LAST_ANON_MINUTE.swap(minute, Ordering::Relaxed);
    note_anonymous_at(anon_buckets(), anon_minutes(), last, minute);
}

/// The ring update for a caller-supplied "last seen minute", so the
/// crossed-bucket rule is testable without waiting a real minute.
///
/// `last` is the minute the previous call saw; every bucket between it and
/// `minute` has been crossed and is now an hour old.
///
/// The rollover and the increment happen under ONE lock. Without it a second
/// request could observe the new minute (and increment the slot) between the
/// rollover and this call's increment, and the rollover's clear then erased
/// that request's count. Atomic ordering alone does not make two operations
/// indivisible; the lock is what does.
fn note_anonymous_at(
    buckets: &[AtomicU64; ANON_WINDOW_MINUTES],
    minutes: &[AtomicU64; ANON_WINDOW_MINUTES],
    last: u64,
    minute: u64,
) {
    let slot = (minute % ANON_WINDOW_MINUTES as u64) as usize;
    if last != minute {
        // Every bucket the ring CROSSED is now an hour old and holds a count
        // from the previous pass over the ring. Clearing only the current one
        // left the minutes that were skipped still counting, so an idle period
        // longer than a minute made `anonymous_window()` report traffic that
        // had happened an hour earlier - inflated for the whole next pass of
        // the ring. Walk from the last seen minute to this one.
        //
        // A gap of a full window or more means every bucket is stale: the walk
        // would reach all of them anyway, so it is bounded at the ring size and
        // the `u64::MAX` sentinel simply clears everything.
        if last == u64::MAX || minute.saturating_sub(last) >= ANON_WINDOW_MINUTES as u64 {
            for bucket in buckets.iter() {
                bucket.store(0, Ordering::Relaxed);
            }
        } else {
            for m in (last + 1)..=minute {
                let crossed = (m % ANON_WINDOW_MINUTES as u64) as usize;
                buckets[crossed].store(0, Ordering::Relaxed);
                minutes[crossed].store(0, Ordering::Relaxed);
            }
        }
    }
    minutes[slot].store(minute, Ordering::Relaxed);
    buckets[slot].fetch_add(1, Ordering::Relaxed);
}

/// Anonymous protected requests in the last hour (rolling minute ring).
///
/// The ring is aged at READ time as well as on each request. Advancing it only
/// on traffic meant one anonymous request followed by silence kept being
/// reported as `1` for ever: the bucket holding it was never reached again, so
/// nothing cleared it and the gauge described a window that had already closed.
pub fn anonymous_window() -> u64 {
    anonymous_window_at(anon_buckets(), anon_minutes(), unix_secs() / 60)
}

/// Sum the minutes still inside the window, ignoring stale buckets.
///
/// A bucket is counted only when its stamped minute is in
/// `(now - 60, now]`. That is what makes the read self-expiring: no request
/// has to arrive for the gauge to fall back to zero.
fn anonymous_window_at(
    buckets: &[AtomicU64; ANON_WINDOW_MINUTES],
    minutes: &[AtomicU64; ANON_WINDOW_MINUTES],
    now_minute: u64,
) -> u64 {
    let mut total = 0u64;
    for (bucket, minute) in buckets.iter().zip(minutes.iter()) {
        let m = minute.load(Ordering::Relaxed);
        let count = bucket.load(Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        // `now_minute - m < 60`, written so an m in the future cannot underflow.
        if now_minute.saturating_sub(m) < ANON_WINDOW_MINUTES as u64 {
            total += count;
        }
    }
    total
}

/// Rate-limited auth-visibility WARN (design 9a): at most once per
/// `interval_min` minutes. The text NEVER claims "auth not configured" -
/// the server only knows identity-header presence.
pub fn warn_anonymous_served(interval_min: u64) {
    static LAST_WARN: AtomicU64 = AtomicU64::new(0);
    let interval = interval_min.max(1) * 60;
    let now = unix_secs();
    let last = LAST_WARN.load(Ordering::Relaxed);
    if now.saturating_sub(last) < interval {
        return;
    }
    if LAST_WARN
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    crate::clog_warn!("requests without user attribution are being served");
}

// -- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// m8: skipping minutes must clear EVERY bucket the ring crossed.
    ///
    /// The ring is an hour long, so a minute slot is reused an hour later.
    /// Clearing only the current slot left the skipped minutes still holding
    /// their previous-pass counts, and `anonymous_window()` sums them - so one
    /// request after a quiet period reported the traffic from an hour ago.
    #[test]
    fn skipping_minutes_clears_every_crossed_bucket() {
        let buckets: [AtomicU64; ANON_WINDOW_MINUTES] = std::array::from_fn(|_| AtomicU64::new(0));
        let minutes: [AtomicU64; ANON_WINDOW_MINUTES] = std::array::from_fn(|_| AtomicU64::new(0));

        // Fill the ring as a busy hour would, marking it as "last seen".
        for b in buckets.iter() {
            b.store(100, Ordering::Relaxed);
        }
        const LAST: u64 = 1_000;

        // Five minutes later: the buckets for 1001..1005 are crossed and must
        // be cleared, while the rest keep their older counts.
        note_anonymous_at(&buckets, &minutes, LAST, LAST + 5);

        let total: u64 = buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        let expected = 100 * (ANON_WINDOW_MINUTES as u64 - 5) + 1;
        assert_eq!(
            total, expected,
            "the five crossed buckets must be cleared and this call counted once"
        );
        // Spot-check one crossed bucket by index, not just the sum.
        let crossed = ((LAST + 3) % ANON_WINDOW_MINUTES as u64) as usize;
        assert_eq!(
            buckets[crossed].load(Ordering::Relaxed),
            0,
            "minute {} was crossed and must be zero",
            LAST + 3
        );
        // And the current minute holds exactly this one request.
        let current = ((LAST + 5) % ANON_WINDOW_MINUTES as u64) as usize;
        assert_eq!(buckets[current].load(Ordering::Relaxed), 1);
    }

    /// A gap of a whole window (or the first ever call) clears everything.
    #[test]
    fn a_full_window_gap_clears_the_whole_ring() {
        let buckets: [AtomicU64; ANON_WINDOW_MINUTES] = std::array::from_fn(|_| AtomicU64::new(7));
        let minutes: [AtomicU64; ANON_WINDOW_MINUTES] = std::array::from_fn(|_| AtomicU64::new(0));

        note_anonymous_at(&buckets, &minutes, u64::MAX, 500);
        let total: u64 = buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        assert_eq!(total, 1, "the first call must start from a clean ring");

        for b in buckets.iter() {
            b.store(9, Ordering::Relaxed);
        }
        note_anonymous_at(&buckets, &minutes, 500, 500 + ANON_WINDOW_MINUTES as u64);
        let total: u64 = buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        assert_eq!(total, 1, "a full-window gap must clear every bucket");
    }

    /// The window EXPIRES AT READ TIME, with no further traffic.
    ///
    /// Advancing the ring only on requests meant one anonymous request followed
    /// by silence was reported as `1` for ever: nothing ever revisited the
    /// bucket it sat in, so the gauge described a window that had closed. Each
    /// bucket now carries its minute, and the read counts only minutes still
    /// inside the hour.
    #[test]
    fn the_anonymous_window_expires_without_new_traffic() {
        let buckets: [AtomicU64; ANON_WINDOW_MINUTES] = std::array::from_fn(|_| AtomicU64::new(0));
        let minutes: [AtomicU64; ANON_WINDOW_MINUTES] = std::array::from_fn(|_| AtomicU64::new(0));

        const NOW: u64 = 2_000;
        note_anonymous_at(&buckets, &minutes, u64::MAX, NOW);
        assert_eq!(
            anonymous_window_at(&buckets, &minutes, NOW),
            1,
            "the request is inside the window right now"
        );

        // 59 minutes later it is still inside the hour.
        assert_eq!(
            anonymous_window_at(&buckets, &minutes, NOW + 59),
            1,
            "the window is an hour long"
        );
        // 60 minutes later it has left it, with no new request to advance the
        // ring - which is exactly the case that used to report 1 for ever.
        assert_eq!(
            anonymous_window_at(&buckets, &minutes, NOW + 60),
            0,
            "a bucket an hour old must not be counted"
        );
        assert_eq!(
            anonymous_window_at(&buckets, &minutes, NOW + 600),
            0,
            "it stays expired"
        );
    }

    // -- sanitize_ctl ------------------------------------------------------

    #[test]
    fn sanitize_ctl_replaces_control_bytes_but_keeps_printable_data() {
        // Every byte class the journal must not carry, in one field.
        assert_eq!(sanitize_ctl("a\u{1b}b"), "a?b", "ESC (terminal injection)");
        assert_eq!(sanitize_ctl("a\0b"), "a?b", "NUL");
        assert_eq!(sanitize_ctl("a\u{7f}b"), "a?b", "DEL");
        assert_eq!(sanitize_ctl("a\tb"), "a?b", "TAB");
        assert_eq!(
            sanitize_ctl("a\rb"),
            "a?b",
            "CR (would forge a record break)"
        );
        assert_eq!(sanitize_ctl("a\nb"), "a?b", "LF (same)");
        assert_eq!(
            sanitize_ctl("\u{1b}[31mred"),
            "?[31mred",
            "ANSI colour lead"
        );
        // Legitimate content is untouched, including multi-byte UTF-8: the
        // journal already carries Cyrillic names, and a per-byte replacement
        // would corrupt them.
        assert_eq!(sanitize_ctl("/dicomweb/studies"), "/dicomweb/studies");
        assert_eq!(sanitize_ctl("Мастяева"), "Мастяева");
        assert_eq!(sanitize_ctl("SMIRNOV^IVAN"), "SMIRNOV^IVAN");
        assert_eq!(sanitize_ctl(""), "");
        // NEL (U+0085) is NOT `char::is_control` (it is category Zl, not Cc)
        // but it ends a line for many readers, so it is rejected explicitly -
        // this is the case that a naive `is_control` implementation misses.
        assert_eq!(sanitize_ctl("a\u{85}b"), "a?b", "NEL is a line break");
        assert_eq!(sanitize_ctl("a\u{2028}b"), "a?b", "LINE SEPARATOR");
        assert_eq!(sanitize_ctl("a\u{2029}b"), "a?b", "PARAGRAPH SEPARATOR");
        // The byte-level fast path must not let NEL through: it is two bytes
        // (0xC2 0x85), so it cannot satisfy an all-printable-ASCII test.
        assert_eq!(sanitize_ctl("\u{85}"), "?");
    }

    #[test]
    fn sanitize_ctl_fast_path_returns_clean_input_unchanged() {
        let clean = "status=200 path=/dicomweb/studies qs=limit=2";
        assert_eq!(sanitize_ctl(clean), clean);
    }

    fn sample_event() -> AccessEvent {
        AccessEvent {
            event: "access",
            ts: "2026-08-27T21:04:12Z".into(),
            user: "rad-ae-title-or-login".into(),
            src_ip: "10.0.0.7".into(),
            via: "nginx",
            action: "R",
            atna: "110103",
            status: 200,
            study: "1.2.840.1".into(),
            series: "".into(),
            sop: "".into(),
            patient: "".into(),
            obj_type: "110180",
            q: "".into(),
            count: 1,
            changed_attrs: 0,
            bytes_replaced: 0,
            reason: "",
        }
    }

    /// Byte-exact render: the field order and values are the design
    /// section 2 example (locked here so future field additions are
    /// deliberate and reviewable).
    #[test]
    fn render_is_byte_exact() {
        let mut out = Vec::new();
        render_event(&mut out, &sample_event());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"event\":\"access\",\"ts\":\"2026-08-27T21:04:12Z\",\"user\":\"rad-ae-title-or-login\",\"role\":\"\",\"src\":\"clarus-server\",\"src_ip\":\"10.0.0.7\",\"via\":\"nginx\",\"action\":\"R\",\"atna\":\"110103\",\"status\":200,\"study\":\"1.2.840.1\",\"series\":\"\",\"sop\":\"\",\"patient\":\"\",\"obj_type\":\"110180\",\"q\":\"\",\"count\":1}"
        );
    }

    /// The `q` field is capped at 1024 bytes on a char boundary; the whole
    /// line stays under the 2 KiB cap.
    #[test]
    fn q_is_capped_and_line_stays_under_cap() {
        let long_q = "a".repeat(5000);
        let mut ev = sample_event();
        ev.q = long_q.clone();
        let mut line = Vec::new();
        render_event(&mut line, &ev);
        assert!(line.len() <= 2 * Q_CAP, "line too long: {}", line.len());
        let s = String::from_utf8(line).unwrap();
        let q = &s[s.find("\"q\":\"").unwrap() + 5..];
        let q = &q[..q.find("\",\"count").unwrap()];
        assert_eq!(q.len(), Q_CAP);
        assert!(long_q.starts_with(q));
    }

    /// The writer thread writes today's file and exits on disconnect
    /// (graceful drain).
    #[test]
    fn writer_writes_today_file_and_drains() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = sync_channel::<Vec<u8>>(RING_SIZE);
        let d = dir.path().to_path_buf();
        std::thread::spawn(move || writer_loop(rx, &d, 0, false));
        let mut line = Vec::new();
        render_event(&mut line, &sample_event());
        line.push(b'\n');
        tx.send(line).expect("send");
        drop(tx); // graceful drain
        let path = today_log_path(dir.path());
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut content = String::new();
        while Instant::now() < deadline {
            if let Ok(t) = std::fs::read_to_string(&path)
                && t.contains("\"event\":\"access\"")
            {
                content = t;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            content.contains("\"atna\":\"110103\""),
            "journal line missing, got: {content}"
        );
    }

    /// The shutdown sentinel flushes the queued tail and stops the writer.
    ///
    /// Without it the writer only flushes on the 1 s / 64 KiB batch rules, so
    /// a process that stops right after its last request loses exactly the
    /// lines that record that request. The test holds the sender open (the
    /// static `TX` is never dropped in production either) and shows that the
    /// queued line reaches disk only because of the sentinel.
    #[test]
    fn a_shutdown_sentinel_flushes_and_stops_the_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = sync_channel::<Vec<u8>>(RING_SIZE);
        let d = dir.path().to_path_buf();
        let writer = std::thread::spawn(move || writer_loop(rx, &d, 0, false));

        let mut line = Vec::new();
        render_event(&mut line, &sample_event());
        line.push(b'\n');
        tx.send(line).expect("send");
        tx.send(Vec::new()).expect("sentinel");

        // The writer returns on the sentinel rather than on disconnect, so a
        // joined handle is the end-of-flush barrier.
        writer.join().expect("writer returned");
        let after = std::fs::read_to_string(today_log_path(dir.path())).unwrap_or_default();
        assert!(
            after.contains("\"atna\":\"110103\""),
            "the sentinel must flush the queued tail: {after}"
        );
    }

    /// A steady stream smaller than `BATCH_BYTES` must still reach disk within
    /// the batch window.
    ///
    /// The deadline used to be recomputed as "now + BATCH_SECS" on every
    /// iteration, so a stream that never paused meant the condition was never
    /// true: the batch grew until it hit the byte cap, holding the tail of the
    /// audit trail in memory for as long as the server stayed busy. Fixing the
    /// deadline when the batch starts is what bounds that latency.
    #[test]
    fn a_steady_trickle_reaches_disk_within_the_batch_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = sync_channel::<Vec<u8>>(RING_SIZE);
        let d = dir.path().to_path_buf();
        std::thread::spawn(move || writer_loop(rx, &d, 0, false));

        let mut line = Vec::new();
        render_event(&mut line, &sample_event());
        line.push(b'\n');

        let path = today_log_path(dir.path());
        let started = Instant::now();
        // Feed one line every 100 ms for well over a batch window. Each line is
        // far below BATCH_BYTES, so only the deadline can make it flush.
        let mut visible_after = None;
        for _ in 0..40 {
            if tx.send(line.clone()).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            if visible_after.is_none()
                && std::fs::read_to_string(&path)
                    .map(|t| t.contains("\"event\":\"access\""))
                    .unwrap_or(false)
            {
                visible_after = Some(started.elapsed());
                break;
            }
        }

        let elapsed = visible_after.expect(
            "a trickling stream never reached disk within the batch window; \
             the deadline is being recomputed per line",
        );
        // Allow the writer's own reaction time on top of the one-second window.
        assert!(
            elapsed < Duration::from_secs(BATCH_SECS * 3),
            "batch took {elapsed:?}, expected under {}s",
            BATCH_SECS * 3
        );
        drop(tx);
    }

    /// Pruner deletes only old 8-digit `YYYYMMDD.log` files; refuses
    /// non-*.log entries; never touches a sibling audit/ dir; never deletes
    /// today's file.
    #[test]
    fn pruner_guarantees() {
        let base = tempfile::tempdir().expect("tempdir");
        let access = base.path().join("access");
        let audit = base.path().join("audit");
        std::fs::create_dir_all(&access).unwrap();
        std::fs::create_dir_all(&audit).unwrap();

        // Old file (10 days ago) + today's file + junk entries.
        let today = crate::audit_log::utc_ymd();
        let today_days = ymd_to_days(&today).unwrap();
        let old = (today_days - 10).to_string(); // placeholder, replaced below
        let _ = old;
        let (old_y, old_m, old_d) = crate::audit_log::civil_from_days(today_days - 10);
        let old_name = format!("{old_y:04}{old_m:02}{old_d:02}.log");
        let old_path = access.join(&old_name);
        std::fs::write(&old_path, b"old").unwrap();
        std::fs::write(access.join(format!("{today}.log")), b"today").unwrap();
        std::fs::write(access.join("not-a-log.txt"), b"txt").unwrap();
        std::fs::write(access.join("README"), b"readme").unwrap();
        // A same-shaped file in the SIBLING audit dir must survive.
        let audit_old = audit.join(&old_name);
        std::fs::write(&audit_old, b"audit-old").unwrap();

        prune(&access, 3, false);

        assert!(!old_path.exists(), "old access file pruned");
        assert!(access.join(format!("{today}.log")).exists(), "today kept");
        assert!(access.join("not-a-log.txt").exists(), "non-log kept");
        assert!(access.join("README").exists(), "no-extension kept");
        assert!(audit_old.exists(), "sibling audit dir untouched");
    }

    /// A huge retention setting does not wrap into "prune everything".
    ///
    /// `retention_days as i64` turned `u64::MAX` into -1, so `age <= -1` was
    /// false for every file and... nothing was pruned - but the same cast is a
    /// silent wrap, and any value above `i64::MAX` is meaningless as an age.
    /// The comparison is now positive and in u64: a retention that large means
    /// "keep", never "delete".
    #[test]
    fn a_huge_retention_keeps_every_old_file() {
        let base = tempfile::tempdir().expect("tempdir");
        let access = base.path().join("access");
        std::fs::create_dir_all(&access).expect("mkdir");
        let today_days = ymd_to_days(&crate::audit_log::utc_ymd()).expect("today");
        let (y, m, d) = crate::audit_log::civil_from_days(today_days - 4000);
        let old = access.join(format!("{y:04}{m:02}{d:02}.log"));
        std::fs::write(&old, b"ancient").expect("plant");

        prune(&access, u64::MAX, false);
        assert!(
            old.exists(),
            "a u64::MAX retention must keep every file, not wrap to a prune"
        );

        // And the same file IS pruned under a small retention, so the guard
        // above is about the huge value, not about pruning being broken.
        prune(&access, 30, false);
        assert!(!old.exists(), "a 30-day retention prunes a 4000-day file");
    }

    /// Dry-run reports but deletes nothing.
    #[test]
    fn pruner_dry_run_deletes_nothing() {
        let base = tempfile::tempdir().expect("tempdir");
        let access = base.path().join("access");
        std::fs::create_dir_all(&access).unwrap();
        let today_days = ymd_to_days(&crate::audit_log::utc_ymd()).unwrap();
        let (y, m, d) = crate::audit_log::civil_from_days(today_days - 10);
        let old_path = access.join(format!("{y:04}{m:02}{d:02}.log"));
        std::fs::write(&old_path, b"old").unwrap();

        prune(&access, 3, true);

        assert!(old_path.exists(), "dry-run keeps the file");
    }

    /// `ymd_to_days` round-trips with `civil_from_days` over a sane range.
    #[test]
    fn ymd_to_days_roundtrip() {
        for days in [0i64, 10_000, 20_000, 21_000] {
            let (y, m, d) = crate::audit_log::civil_from_days(days);
            let back = ymd_to_days(&format!("{y:04}{m:02}{d:02}")).unwrap();
            assert_eq!(back, days);
        }
        assert!(ymd_to_days("notadate").is_none());
        assert!(ymd_to_days("20261301").is_none());
    }

    /// A pre-init `emit` queues nothing.
    ///
    /// Closed: inspection-verified. `TX` is a `OnceLock` that the first test to
    /// call `init` fills for the whole binary, so "TX is unset" cannot be
    /// arranged here once any other test has run - and a test that merely
    /// asserts `is_ready()` or wraps the call in `if !is_ready()` passes with or
    /// without the guard. What IS checked structurally is the guard's shape:
    /// the branch reads `TX.get()` and returns BEFORE rendering or sending, so
    /// there is no path from an unset sender to a queued record.
    #[test]
    fn the_pre_init_guard_returns_before_reaching_the_channel() {
        // The predicate is a pure function of the `OnceLock`; it has no side
        // effect, which is what makes `emit`'s early return a no-op.
        let before = is_ready();
        let _ = is_ready();
        assert_eq!(
            is_ready(),
            before,
            "is_ready must not change the state it observes"
        );
    }

    /// A batch buffered before midnight is persisted to ITS OWN day's file.
    ///
    /// Rotation dropped the handle but kept the bytes, so the next flush wrote
    /// records produced yesterday into today's file. Each line carries its own
    /// `ts`, and an auditor looks a period up by date - so those records became
    /// unfindable under the day they name. The batch now remembers the day of
    /// its first line and is written to that day's file at the boundary.
    #[test]
    fn a_batch_that_straddles_midnight_keeps_its_own_day() {
        // Shares the seam lock: `write_journal_batch` reads a process-global
        // arming, so this test must not run while a durability test has the
        // access-journal site armed.
        let _g = JOURNAL_SEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let mut file = None;
        let mut batch = b"{\"event\":\"access\",\"ts\":\"2026-08-28T23:59:59Z\"}\n".to_vec();

        assert!(
            flush_batch_to_day(&mut file, dir.path(), &mut batch, "20260828"),
            "the old day's batch must be written"
        );
        assert!(batch.is_empty());

        let old = dir.path().join("20260828.log");
        assert!(old.is_file(), "the batch went to the day it belongs to");
        assert!(
            !dir.path().join("20260829.log").exists(),
            "the next day's file must not receive yesterday's records"
        );
        let text = std::fs::read_to_string(&old).expect("read");
        assert!(text.contains("2026-08-28T23:59:59Z"), "content preserved");

        // The ROTATION DECISION is what failed: with a non-empty batch from the
        // old day, crossing midnight must trigger the old-day flush.
        assert!(
            rotation_needs_old_day_flush(true, "20260828", "20260829"),
            "a straddling batch must be flushed to its own day at rotation"
        );
        // A batch already belonging to the new day must NOT be redirected.
        assert!(!rotation_needs_old_day_flush(true, "20260829", "20260829"));
        // An empty batch has nothing to preserve.
        assert!(!rotation_needs_old_day_flush(false, "20260828", "20260829"));
    }

    /// The rendered line stays under its budget whatever the request supplied.
    ///
    /// The query text had a cap; `user` and the UID fields did not. A 4 KiB
    /// identity header produced a record several times the documented line
    /// budget, which is what makes a batch unbounded and a reader expensive.
    /// Every variable field is now capped, and the whole line is checked.
    #[test]
    fn a_hostile_request_cannot_blow_the_line_budget() {
        let long = "A".repeat(4096);
        let mut ev = sample_event();
        ev.user = long.clone();
        ev.src_ip = long.clone();
        ev.study = long.clone();
        ev.series = long.clone();
        ev.sop = long.clone();
        ev.patient = long.clone();
        ev.q = "patientName=".to_string() + &long;

        let mut line = Vec::new();
        render_event(&mut line, &ev);
        assert!(
            line.len() < 2048,
            "the rendered line must stay under 2 KiB, got {} bytes",
            line.len()
        );
        // The line is still valid JSON: one opening and one closing brace, and
        // the escaping did not corrupt a value.
        assert_eq!(line.first(), Some(&b'{'));
        assert_eq!(line.last(), Some(&b'}'));
        assert!(
            line.windows(10).any(|w| w == b"\"user\":\"AA"),
            "the capped value is still present"
        );
    }

    // -- Journal durability ------------------------------------------------

    /// The seam is process-global, so these tests run one at a time.
    static JOURNAL_SEAM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn sealed<F: FnOnce()>(f: F) {
        let _g = JOURNAL_SEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::io_fault::clear();
        f();
        crate::io_fault::clear();
    }

    /// A batch the journal cannot persist must be COUNTED, not silently lost.
    ///
    /// Every failure in `flush_batch` used to be discarded: the open failure
    /// cleared the batch, and the write/fsync results were dropped with `let
    /// _ =`. An unwritable journal therefore looked exactly like a healthy one -
    /// the ring drained and no counter moved. The counter is what lets an
    /// operator tell evidence loss from ordinary backpressure.
    #[test]
    fn a_batch_that_cannot_be_written_is_counted() {
        sealed(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut file = None;
            let line = b"{\"event\":\"access\"}\n";
            let mut batch = line.to_vec();
            let failed_before = crate::metrics::ACCESS_AUDIT_WRITE_FAILED.load(Ordering::Relaxed);
            let bytes_before =
                crate::metrics::ACCESS_AUDIT_WRITE_FAILED_BYTES.load(Ordering::Relaxed);

            crate::io_fault::arm(
                crate::io_fault::SITE_ACCESS_JOURNAL,
                std::io::ErrorKind::StorageFull,
            );
            assert!(
                !flush_batch(&mut file, dir.path(), &mut batch),
                "a failed write must report failure"
            );

            assert_eq!(
                crate::metrics::ACCESS_AUDIT_WRITE_FAILED.load(Ordering::Relaxed),
                failed_before + 1,
                "the lost batch must move the failure counter"
            );
            assert_eq!(
                crate::metrics::ACCESS_AUDIT_WRITE_FAILED_BYTES.load(Ordering::Relaxed),
                bytes_before + line.len() as u64,
                "the lost bytes must be accounted"
            );
            assert!(batch.is_empty(), "the batch is not retried forever");
            assert!(
                file.is_none(),
                "a failed write drops the handle so the next batch reopens"
            );
        });
    }

    /// A successful flush is NOT counted as a failure.
    ///
    /// Without this the counter above could be incremented unconditionally and
    /// the "counted" test above would still pass.
    #[test]
    fn a_batch_that_is_written_is_not_counted_as_lost() {
        sealed(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut file = None;
            let mut batch = b"{\"event\":\"access\"}\n".to_vec();
            let before = crate::metrics::ACCESS_AUDIT_WRITE_FAILED.load(Ordering::Relaxed);

            assert!(
                flush_batch(&mut file, dir.path(), &mut batch),
                "an unarmed flush succeeds"
            );
            assert_eq!(
                crate::metrics::ACCESS_AUDIT_WRITE_FAILED.load(Ordering::Relaxed),
                before,
                "a written batch must not be counted as lost"
            );
            assert!(batch.is_empty());
            let written = std::fs::read_to_string(today_log_path(dir.path())).expect("the log");
            assert_eq!(written, "{\"event\":\"access\"}\n");
        });
    }

    /// Every route's ATNA code and action come from the standard-backed
    /// constants, and each action is one the standard allows for that event.
    ///
    /// The table used to carry its own literals, so the journal could emit a
    /// code the reference does not contain, or pair a real EventID with an
    /// action the standard forbids for it (Query 110112 mandates E, and used
    /// to be recorded as R). This walks every routable id and checks the pair
    /// against `standard/ps3.15/audit-message-model.json`.
    #[test]
    fn every_journal_event_uses_a_standard_code_and_a_permitted_action() {
        use crate::audit_codes::{action, event_id};

        const STD: &str = include_str!("../../standard/ps3.15/audit-message-model.json");
        let compact: String = STD.chars().filter(|c| !c.is_whitespace()).collect();

        let uid = |_i: usize| "1.2.3".to_string();
        let routes = [
            "wado_instance",
            "wado_series",
            "wado_study",
            "qido_patients",
            "qido_studies",
            "stow_studies",
            "mwl_search",
            "mpps_create_or_update",
            "mpps_get",
            "admin_audit_log",
        ];
        let mut seen = 0usize;
        for route in routes {
            let Some((act, atna, ..)) = classify(route, b"", &uid, 1) else {
                continue;
            };
            seen += 1;
            assert!(
                compact.contains(&format!("\"id\":\"{atna}\"")),
                "{route} emits {atna}, which is not an A.5.3 event id"
            );
            // The action must be one of the letters the journal models; an
            // arbitrary string would parse but mean nothing. E is included
            // because A.5.3.10 (Query) mandates it.
            assert!(
                [
                    action::CREATE,
                    action::READ,
                    action::UPDATE,
                    action::DELETE,
                    action::EXECUTE,
                ]
                .contains(&act),
                "{route} emits action {act}, which is not a modelled EventActionCode"
            );
            // Query and User Authentication mandate E; Audit Log Used mandates
            // R. Those two constraints are the ones this table can violate.
            if atna == event_id::QUERY {
                assert_eq!(act, action::EXECUTE, "{route}: A.5.3.10 mandates E");
            }
            if atna == event_id::AUDIT_LOG_USED {
                assert_eq!(act, action::READ, "{route}: A.5.3.2 mandates R");
            }
            if atna == event_id::PATIENT_RECORD {
                assert_ne!(act, action::EXECUTE, "{route}: A.5.3.14 excludes E");
            }
        }
        assert_eq!(seen, routes.len(), "every listed route must classify");
    }

    /// The MWL search and the MPPS transactions map to the ATNA codes PS3.15
    /// A.5.3.10 / A.5.3.15 names, and Create and Update - which share one route
    /// id because the router cannot see `?update` - are told apart by the same
    /// query parameter the dispatcher reads.
    ///
    /// The mapping is asserted through [`classify`] directly: it is the pure
    /// decision, and reading it back here avoids the process-global writer
    /// channel that `emit` uses.
    #[test]
    fn workflow_routes_map_to_the_procedure_and_query_events() {
        let uid = |_i: usize| "1.2.3".to_string();

        // MWL search -> Query (110112), action E. A.5 mandates Execute for the
        // Query event, so R would be the wrong action for a right EventID.
        let (action, atna, _, _, _, obj_type, q, count) =
            classify("mwl_search", b"PatientID=P1", &uid, 0).expect("mwl_search maps");
        assert_eq!((action, atna), ("E", "110112"));
        assert_eq!(obj_type, "", "a query names no object identifier");
        assert_eq!(q, "PatientID=P1");
        assert_eq!(count, 1);

        // MPPS create (no query parameter) -> Procedure Record (110111), C.
        let (action, atna, _, _, sop, obj_type, _, _) =
            classify("mpps_create_or_update", b"", &uid, 0).expect("create maps");
        assert_eq!((action, atna), ("C", "110111"));
        assert_eq!(sop, "1.2.3", "the MPPS UID is the object identifier");
        assert_eq!(obj_type, "110180", "an MPPS UID is a DICOM UID");

        // MPPS update (?update) -> the SAME code, action U.
        let (action, atna, _, _, _, _, _, _) =
            classify("mpps_create_or_update", b"update", &uid, 0).expect("update maps");
        assert_eq!((action, atna), ("U", "110111"));

        // MPPS update with the parameter spelled with a value.
        let (action, _, _, _, _, _, _, _) =
            classify("mpps_create_or_update", b"update=true", &uid, 0).expect("update maps");
        assert_eq!(action, "U");

        // MPPS retrieve -> the same code, action R.
        let (action, atna, _, _, _, _, _, _) =
            classify("mpps_get", b"", &uid, 0).expect("get maps");
        assert_eq!((action, atna), ("R", "110111"));

        // A route with no access event maps to nothing.
        assert!(classify("health", b"", &uid, 0).is_none());
    }

    /// The mask must survive the wire spelling of the key.
    ///
    /// A client is free to percent-encode any byte of a query key, and the
    /// server matches on the DECODED key, so `patient%4eame` addresses the
    /// PatientName attribute. Comparing the raw spelling let such a request
    /// write a real name into the journal - the value is what the rule
    /// protects, and it was printed in clear.
    #[test]
    fn a_percent_encoded_name_key_is_still_masked() {
        let masked = mask_query_phi("patient%4Eame=Ivanov&PatientID=P1");
        assert_eq!(
            masked, "patient%4Eame=***&PatientID=P1",
            "an encoded NAME key must mask, an identifier key must not"
        );

        // The same key spelled by tag number, encoded.
        let masked = mask_query_phi("0010%2C0010=Ivanov");
        assert_eq!(masked, "0010%2C0010=***");

        // The other Person Name keys are matchable too, so one unmasked key
        // would defeat the rule.
        for key in [
            "ReferringPhysicianName",
            "PerformingPhysicianName",
            "NameOfPhysiciansReadingStudy",
            "OperatorsName",
            "OtherPatientNames",
            "PatientComments",
        ] {
            let qs = format!("{key}=Ivanov");
            assert_eq!(
                mask_query_phi(&qs),
                format!("{key}=***"),
                "{key} carries personal data and must mask"
            );
        }
    }

    /// BOTH spellings of a sensitive key must mask.
    ///
    /// The keyword form was masked and the TAG form was not, because the table
    /// carried (0040,0275) - Request ATTRIBUTES Sequence, a different
    /// attribute - instead of (0040,1400) Requested Procedure Comments. A
    /// client that addressed the comment field by tag wrote it in clear.
    #[test]
    fn both_the_keyword_and_tag_spelling_of_a_sensitive_key_mask() {
        // The exact pair the defect named.
        assert_eq!(
            mask_query_phi("RequestedProcedureComments=synthetic"),
            "RequestedProcedureComments=***",
            "the keyword form must mask"
        );
        assert_eq!(
            mask_query_phi("00401400=synthetic"),
            "00401400=***",
            "the TAG form of Requested Procedure Comments must mask too"
        );

        // Person Name keys that were missing from the table entirely.
        for key in [
            "PhysiciansOfRecord",
            "00081048",
            "AdmittingDiagnosesDescription",
            "PatientBirthName",
            "Occupation",
        ] {
            let qs = format!("{key}=Ivanov");
            assert_eq!(
                mask_query_phi(&qs),
                format!("{key}=***"),
                "{key} carries personal data and must mask"
            );
        }

        // Identifiers stay VISIBLE: the auditor must see whose data was
        // searched.
        assert_eq!(
            mask_query_phi("PatientID=P1&AccessionNumber=ACC1"),
            "PatientID=P1&AccessionNumber=ACC1",
            "identifiers are not personal names and stay visible"
        );
    }

    /// A Person Name under a DOTTED sequence-attribute key must mask.
    ///
    /// PS3.18 permits `SequenceTerm.Attribute=value`, and a classified MWL
    /// request can carry a name that way. The comparison used the WHOLE key,
    /// which never matches a flat list entry, so the value was copied into the
    /// journal in clear. Adding the terminal name to the flat list does not
    /// help: the comparison is on the full dotted key.
    #[test]
    fn a_person_name_under_a_dotted_sequence_key_masks() {
        // The exact case from the finding.
        assert_eq!(
            mask_query_phi(
                "ScheduledProcedureStepSequence.ScheduledPerformingPhysicianName=synthetic"
            ),
            "ScheduledProcedureStepSequence.ScheduledPerformingPhysicianName=***",
            "the terminal attribute decides, not the whole path"
        );

        // The tag-number spelling of the same terminal attribute.
        assert_eq!(
            mask_query_phi("00400100.00400006=synthetic"),
            "00400100.00400006=***",
            "a dotted TAG path masks on its terminal element"
        );

        // A dotted path whose terminal is NOT a name is left alone.
        assert_eq!(
            mask_query_phi("00400100.00400001=ACC1"),
            "00400100.00400001=ACC1",
            "ScheduledProcedureStepID is an identifier, not a name"
        );
    }

    /// A malformed escape must not swallow the rest of the key.
    #[test]
    fn a_malformed_escape_does_not_hide_a_name_key() {
        assert_eq!(mask_query_phi("PatientName=Ivanov"), "PatientName=***");
        assert_eq!(percent_decode("patientna%2"), "patientna%2");
        assert_eq!(percent_decode("a+b"), "a b");
    }
}
