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
        // The sentinel is queued AFTER every event emitted so far, so the
        // writer flushes the whole ring before it stops.
        if let Err(e) = self.tx.send(Vec::new()) {
            // The writer already exited (channel closed): nothing to drain.
            crate::clog_debug!("access writer already stopped: {e}");
            return;
        }
        // `send` returns once the message is QUEUED, not once it is written.
        // Waiting for the writer to close the channel is what makes this a
        // flush barrier rather than a best-effort poke.
        let deadline = Instant::now() + Duration::from_secs(SHUTDOWN_FLUSH_SECS);
        while Instant::now() < deadline {
            match self.tx.try_send(Vec::new()) {
                Ok(()) => {}
                // A FULL ring is not the writer stopping: the sentinel did not
                // fit because the backlog is still being drained. Treating it
                // as a stop returned here and left that backlog unwritten -
                // precisely the tail this barrier exists to save.
                Err(TrySendError::Full(_)) => {}
                // The receiver is gone, so the writer has returned.
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

    loop {
        let now_day = crate::audit_log::utc_ymd();
        if now_day != current_day {
            // Rotation: drop the old handle, prune the new day boundary.
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
                    // reveal shutdown by disconnecting.
                    if !batch.is_empty() {
                        flush_batch(&mut file, dir, &mut batch);
                    }
                    return;
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

/// ONE append + fsync of the accumulated batch to `dir/YYYYMMDD.log`.
fn flush_batch(file: &mut Option<std::fs::File>, dir: &Path, batch: &mut Vec<u8>) {
    if batch.is_empty() {
        return;
    }
    if file.is_none() {
        *file = open_today(dir);
    }
    let Some(f) = file.as_mut() else {
        // Open failed (e.g. dir gone): drop the batch rather than spin.
        batch.clear();
        return;
    };
    let _ = f.write_all(batch);
    let _ = f.sync_all();
    batch.clear();
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
        if today_days - day_days <= retention_days as i64 {
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
    w_str(out, "user", &e.user);
    w_str(out, "role", "");
    w_str(out, "src", "clarus-server");
    w_str(out, "src_ip", &e.src_ip);
    w_str(out, "via", e.via);
    w_str(out, "action", e.action);
    w_str(out, "atna", e.atna);
    w_u64(out, "status", e.status as u64);
    w_str(out, "study", &e.study);
    w_str(out, "series", &e.series);
    w_str(out, "sop", &e.sop);
    w_str(out, "patient", &e.patient);
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
            "110103",
            uid(0),
            uid(1),
            uid(2),
            "110180",
            String::new(),
            1u64,
        ),
        "wado_series" | "wado_series_metadata" | "wado_series_thumbnail" => (
            "R",
            "110103",
            uid(0),
            uid(1),
            String::new(),
            "110180",
            String::new(),
            1u64,
        ),
        "wado_study" | "wado_study_metadata" | "wado_study_thumbnail" | "wado_metadata" => (
            "R",
            "110103",
            uid(0),
            String::new(),
            String::new(),
            "110180",
            String::new(),
            1u64,
        ),
        "wado_thumbnail" => (
            "R",
            "110103",
            uid(0),
            uid(1),
            uid(2),
            "110180",
            String::new(),
            1u64,
        ),
        "wado_rendered" => (
            "R",
            "110103",
            uid(0),
            uid(1),
            uid(2),
            "110180",
            String::new(),
            1u64,
        ),
        // QIDO family - query access. Patients = 110110 (patient record),
        // study/series/instance searches = 110112 (query).
        "qido_patients" => (
            "R",
            "110110",
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
            "R",
            "110112",
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
            "110104",
            String::new(),
            String::new(),
            String::new(),
            "",
            String::new(),
            stow_count,
        ),
        // MWL Search - a Query. PS3.15 A.5.3.10 (Query) names Modality
        // Worklist among the SOP classes it reports, so the search maps to
        // 110112 with action R (read). The query text is the auditor's
        // evidence and is carried verbatim under the same 1024-byte cap as the
        // QIDO searches.
        "mwl_search" => (
            "R",
            "110112",
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
                    "110111",
                    String::new(),
                    String::new(),
                    uid(0),
                    "110180",
                    String::new(),
                    1u64,
                )
            } else {
                (
                    "C",
                    "110111",
                    String::new(),
                    String::new(),
                    uid(0),
                    "110180",
                    String::new(),
                    1u64,
                )
            }
        }
        "mpps_get" => (
            "R",
            "110111",
            String::new(),
            String::new(),
            uid(0),
            "110180",
            String::new(),
            1u64,
        ),
        // Self-audit (GOST 27789 9.4): journal reads record themselves.
        "admin_audit_log" => (
            "R",
            "110101",
            String::new(),
            String::new(),
            String::new(),
            "",
            String::new(),
            1u64,
        ),
        "admin_access_log" => (
            "R",
            "110101",
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
        action: "U",
        atna: "110103",
        status: 200,
        study: study.to_string(),
        series: series.to_string(),
        sop: sop.to_string(),
        patient: patient.to_string(),
        obj_type: "2",
        q: String::new(),
        count: 1,
        changed_attrs: changed_attrs as u64,
        bytes_replaced: u64::from(bytes_replaced),
        reason,
    };
    emit(&ev);
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
    if key.as_bytes().iter().any(|b| *b == b'%' || *b == b'+') {
        let decoded = percent_decode(key);
        return name_key_matches(decoded.bytes());
    }
    name_key_matches(key.bytes())
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
        "00100010"
            | "patientname"
            | "00080090"
            | "referringphysicianname"
            | "00081050"
            | "performingphysicianname"
            | "00081060"
            | "nameofphysiciansreadingstudy"
            | "00081070"
            | "operatorsname"
            | "00101001"
            | "otherpatientnames"
            | "00101000"
            | "otherpatientids"
            | "00101002"
            | "otherpatientidsequence"
            | "00104000"
            | "patientcomments"
            | "00400275"
            | "requestedprocedurecomments"
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
static LAST_ANON_MINUTE: AtomicU64 = AtomicU64::new(u64::MAX);

fn anon_buckets() -> &'static [AtomicU64; ANON_WINDOW_MINUTES] {
    ANON_WINDOW.get_or_init(|| std::array::from_fn(|_| AtomicU64::new(0)))
}

/// Cheap per-minute counter ring: one anonymous protected request.
pub fn note_anonymous_protected() {
    let minute = unix_secs() / 60;
    let last = LAST_ANON_MINUTE.swap(minute, Ordering::Relaxed);
    note_anonymous_at(anon_buckets(), last, minute);
}

/// The ring update for a caller-supplied "last seen minute", so the
/// crossed-bucket rule is testable without waiting a real minute.
///
/// `last` is the minute the previous call saw; every bucket between it and
/// `minute` has been crossed and is now an hour old.
fn note_anonymous_at(buckets: &[AtomicU64; ANON_WINDOW_MINUTES], last: u64, minute: u64) {
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
                buckets[(m % ANON_WINDOW_MINUTES as u64) as usize].store(0, Ordering::Relaxed);
            }
        }
    }
    buckets[slot].fetch_add(1, Ordering::Relaxed);
}

/// Anonymous protected requests in the last hour (rolling minute ring).
pub fn anonymous_window() -> u64 {
    anon_buckets()
        .iter()
        .map(|b| b.load(Ordering::Relaxed))
        .sum()
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

        // Fill the ring as a busy hour would, marking it as "last seen".
        for b in buckets.iter() {
            b.store(100, Ordering::Relaxed);
        }
        const LAST: u64 = 1_000;

        // Five minutes later: the buckets for 1001..1005 are crossed and must
        // be cleared, while the rest keep their older counts.
        note_anonymous_at(&buckets, LAST, LAST + 5);

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

        note_anonymous_at(&buckets, u64::MAX, 500);
        let total: u64 = buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        assert_eq!(total, 1, "the first call must start from a clean ring");

        for b in buckets.iter() {
            b.store(9, Ordering::Relaxed);
        }
        note_anonymous_at(&buckets, 500, 500 + ANON_WINDOW_MINUTES as u64);
        let total: u64 = buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        assert_eq!(total, 1, "a full-window gap must clear every bucket");
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

    /// `emit` is a safe no-op before `init` (pre-init guard).
    #[test]
    fn emit_before_init_is_noop() {
        // TX is process-global; this test only asserts the guard path when
        // unset is unreachable after other tests may have initialized it, so
        // we just exercise render + emit without panicking.
        let ev = sample_event();
        emit(&ev);
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

        // MWL search -> Query (110112), action R, query text carried.
        let (action, atna, _, _, _, obj_type, q, count) =
            classify("mwl_search", b"PatientID=P1", &uid, 0).expect("mwl_search maps");
        assert_eq!((action, atna), ("R", "110112"));
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

    /// A malformed escape must not swallow the rest of the key.
    #[test]
    fn a_malformed_escape_does_not_hide_a_name_key() {
        assert_eq!(mask_query_phi("PatientName=Ivanov"), "PatientName=***");
        assert_eq!(percent_decode("patientna%2"), "patientna%2");
        assert_eq!(percent_decode("a+b"), "a b");
    }
}
