// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! STOW ingest aggregation.
//!
//! A STOW upload stores one study across many POST slices (a 432-slice study
//! would otherwise spam three journal lines per slice - started + accepted +
//! ACCESS). This module accumulates the slices of one study into a bucket
//! keyed via `(study_uid, user, ip)` and emits ONE `started` line on the
//! first slice and ONE `completed` line when the session goes quiet (TTL) or
//! `partial` on LRU eviction. Per-request ACCESS lines stay, exactly like the
//! WADO aggregator leaves them.
//!
//! The module is pure: it formats its own nginx kv lines and touches neither
//! the log channel, `clog`, nor the connector. Callers route the returned
//! lines onto the journal themselves.

#![forbid(unsafe_code)]

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A stopped session lingers at most this long after its last slice before it
/// is emitted as `completed` and forgotten. 30s is comfortably longer than any
/// inter-slice gap in one upload while still surfacing an interrupted ingest
/// promptly.
const TTL: Duration = Duration::from_secs(30);
/// Upper bound of concurrently tracked studies. Beyond it the least recently
/// written bucket is evicted (emitted `partial`) so a burst of one-off studies
/// never grows memory without bound.
const CAP: usize = 256;
/// Progress cadence: a `STOW_STUDY progress` line is emitted every this many
/// slices of one session. The tomograph never announces a total, so periodic
/// progress (SQL-era behavior: every 50 slices plus the final line) is the
/// only live feedback during a long upload; the TTL-completed line is the
/// final marker.
const PROGRESS_EVERY: u64 = 50;

/// Accumulated state of one in-progress STOW session.
struct Bucket {
    /// Length-prefixed `(study, user, ip)` identity.
    key: Vec<u8>,
    /// Study instance UID.
    study: Vec<u8>,
    /// Journal label for the client (already labelled upstream).
    user: Vec<u8>,
    /// Effective client address.
    ip: Vec<u8>,
    /// Number of POST requests (slices) aggregated.
    slices: u64,
    /// Freshly-stored instances (`accepted - idempotent`).
    stored: u64,
    /// Idempotent (exact-duplicate) re-STOWs.
    idempotent: u64,
    /// Instances that failed processing.
    failed: u64,
    /// SOPInstanceUID conflicts.
    conflicts: u64,
    /// Sum of request body bytes (only when the source supplies them).
    bytes: u64,
    /// Sum of handler microseconds (only when the source supplies them).
    us: u64,
    /// Whether any source supplied size/timing fields (else omit both).
    has_size: bool,
    /// Time of the last recorded slice.
    last: Instant,
}

/// The shared bucket table. The `Mutex` is held only for the short lookup /
/// update; a slice never blocks more than one mutex acquisition.
static TABLE: OnceLock<Mutex<Vec<Bucket>>> = OnceLock::new();

fn table() -> &'static Mutex<Vec<Bucket>> {
    TABLE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Build an unambiguous byte key for `(study, user, ip)`.
///
/// Length-prefixed so no separator byte can collide across fields (a name or
/// UID may legally contain any ASCII aside from digits/dots).
fn key(study: &[u8], user: &[u8], ip: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(study.len() + user.len() + ip.len() + 3 * 4);
    for part in [study, user, ip] {
        k.extend_from_slice(&(part.len() as u32).to_le_bytes());
        k.extend_from_slice(part);
    }
    k
}

/// Record one STOW slice (public wrapper using the real clock). Returns the
/// aggregator lines, normally zero or one; an eviction plus a new session can
/// yield up to two, hence `Vec`. `bytes`/`us` are `None` when the caller has
/// no source for them (then the emitted lines drop the `bytes=`/`us=` fields).
#[allow(clippy::too_many_arguments)]
pub fn record(
    study: &[u8],
    user: &[u8],
    ip: &[u8],
    stored: u64,
    idempotent: u64,
    failed: u64,
    conflicts: u64,
    bytes: Option<u64>,
    us: Option<u64>,
) -> Vec<String> {
    record_at(
        study,
        user,
        ip,
        stored,
        idempotent,
        failed,
        conflicts,
        bytes,
        us,
        Instant::now(),
    )
}

/// Testable core: same as [`record`] but with a caller-supplied clock, so
/// unit tests drive TTL expiry without sleeping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_at(
    study: &[u8],
    user: &[u8],
    ip: &[u8],
    stored: u64,
    idempotent: u64,
    failed: u64,
    conflicts: u64,
    bytes: Option<u64>,
    us: Option<u64>,
    now: Instant,
) -> Vec<String> {
    let mut buckets = table().lock().unwrap_or_else(|e| e.into_inner());
    record_slices(
        &mut buckets,
        study,
        user,
        ip,
        stored,
        idempotent,
        failed,
        conflicts,
        bytes,
        us,
        now,
    )
}

/// Core aggregation against an explicit bucket vector.
///
/// Kept separate from the global wrapper so unit tests exercise the exact
/// same logic on a private vector and run safely in parallel (no race on the
/// single shared `TABLE`). The wrapper locks the table and forwards.
#[allow(clippy::too_many_arguments)]
fn record_slices(
    buckets: &mut Vec<Bucket>,
    study: &[u8],
    user: &[u8],
    ip: &[u8],
    stored: u64,
    idempotent: u64,
    failed: u64,
    conflicts: u64,
    bytes: Option<u64>,
    us: Option<u64>,
    now: Instant,
) -> Vec<String> {
    let mut out = Vec::new();

    // Lazy sweep: one pass per record evicts every expired bucket, emitted as
    // `completed` so no session is silently forgotten. The periodic 5s sweep
    // (see [`sweep`]) shares this same pass so an idle server still surfaces
    // sessions that went quiet between records.
    out.extend(sweep_buckets(buckets, now));

    let k = key(study, user, ip);
    if let Some(b) = buckets.iter_mut().find(|b| b.key == k) {
        record_into(b, stored, idempotent, failed, conflicts, bytes, us, now);
        // Periodic progress: every 50 slices the operator sees the session is
        // alive; the totals line still comes from the TTL sweep.
        if b.slices % PROGRESS_EVERY == 0 {
            out.push(format_progress(b));
        }
        return out;
    }

    // New session: admit (evicting the least recently written entry if at cap).
    if buckets.len() >= CAP
        && let Some((idx, _)) = buckets.iter().enumerate().min_by_key(|(_, b)| b.last)
    {
        out.push(format_partial(&buckets[idx]));
        buckets.swap_remove(idx);
    }
    let mut b = Bucket {
        key: k,
        study: study.to_vec(),
        user: user.to_vec(),
        ip: ip.to_vec(),
        slices: 0,
        stored: 0,
        idempotent: 0,
        failed: 0,
        conflicts: 0,
        bytes: 0,
        us: 0,
        has_size: false,
        last: now,
    };
    record_into(
        &mut b, stored, idempotent, failed, conflicts, bytes, us, now,
    );
    // A brand-new session surfaces a `started` marker FIRST.
    let started = format_started(&b);
    out.insert(0, started);
    buckets.push(b);
    out
}

/// One shared TTL pass over a bucket vector: every bucket idle longer than
/// [`TTL`] is emitted as `completed` and removed. Used by both the lazy sweep
/// inside [`record_slices`] and the standalone periodic [`sweep`], so an idle
/// server surfaces sessions that went quiet between records instead of waiting
/// for the next POST. Eviction (`partial`) is NOT part of this pass; the
/// CAP-aware eviction stays in [`record_slices`].
fn sweep_buckets(buckets: &mut Vec<Bucket>, now: Instant) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buckets.len() {
        if now.duration_since(buckets[i].last) > TTL {
            out.push(format_completed(&buckets[i]));
            buckets.swap_remove(i);
        } else {
            i += 1;
        }
    }
    out
}

/// Periodic TTL sweep over the shared table (public wrapper using the real
/// clock). Emits a `completed` line for every session idle longer than
/// [`TTL`] and removes it; an empty table yields no lines. The journal thread
/// calls this on a fixed 5s tick so interrupted ingests surface promptly even
/// with no further POSTs. Eviction/`partial` is deliberately absent here -
/// capacity handling stays in [`record_at`].
pub fn sweep() -> Vec<String> {
    sweep_at(Instant::now())
}

/// Testable periodic sweep: same as [`sweep`] but with a caller-supplied
/// clock, so unit tests drive TTL expiry without sleeping.
pub(crate) fn sweep_at(now: Instant) -> Vec<String> {
    let mut buckets = table().lock().unwrap_or_else(|e| e.into_inner());
    sweep_buckets(&mut buckets, now)
}

/// Apply one slice to a bucket: counters, `last`, optional size/timing sum.
#[allow(clippy::too_many_arguments)]
fn record_into(
    b: &mut Bucket,
    stored: u64,
    idempotent: u64,
    failed: u64,
    conflicts: u64,
    bytes: Option<u64>,
    us: Option<u64>,
    now: Instant,
) {
    b.slices += 1;
    b.stored += stored;
    b.idempotent += idempotent;
    b.failed += failed;
    b.conflicts += conflicts;
    if let Some(x) = bytes {
        b.bytes += x;
    }
    if let Some(x) = us {
        b.us += x;
    }
    if bytes.is_some() || us.is_some() {
        b.has_size = true;
    }
    b.last = now;
}

/// Line marking the first slice of a brand-new STOW session for a study.
fn format_started(b: &Bucket) -> String {
    format!(
        "started study={} user={} ip={}",
        String::from_utf8_lossy(&b.study),
        field(&b.user),
        field(&b.ip),
    )
}

/// Line for a session that went quiet (TTL sweep). Includes `of` totals plus
/// `bytes=`/`us=` only when the source supplied them.
fn format_completed(b: &Bucket) -> String {
    let size = format_size(b);
    format!(
        "completed study={} slices={} stored={} idempotent={} failed={} conflicts={}{} user={} ip={}",
        String::from_utf8_lossy(&b.study),
        b.slices,
        b.stored,
        b.idempotent,
        b.failed,
        b.conflicts,
        size,
        field(&b.user),
        field(&b.ip),
    )
}

/// Periodic progress marker during a long session (every [`PROGRESS_EVERY`]
/// slices) - SQL-era behavior: live feedback before the final TTL line.
fn format_progress(b: &Bucket) -> String {
    let size = format_size(b);
    format!(
        "progress study={} slices={} stored={} idempotent={} failed={} conflicts={}{} user={} ip={}",
        String::from_utf8_lossy(&b.study),
        b.slices,
        b.stored,
        b.idempotent,
        b.failed,
        b.conflicts,
        size,
        field(&b.user),
        field(&b.ip),
    )
}

/// Line for a session forced out by LRU eviction (still in progress totals).
fn format_partial(b: &Bucket) -> String {
    let size = format_size(b);
    format!(
        "partial study={} slices={} stored={} idempotent={} failed={} conflicts={}{} user={} ip={}",
        String::from_utf8_lossy(&b.study),
        b.slices,
        b.stored,
        b.idempotent,
        b.failed,
        b.conflicts,
        size,
        field(&b.user),
        field(&b.ip),
    )
}

/// Render one log FIELD value: printable ASCII only, no whitespace.
///
/// `user` comes from the authenticated identity header and `ip` from the peer
/// address, so both are attacker-influenced. Interpolated raw, a newline in
/// either ends the line early and an operator parsing the aggregate reads
/// fields that were never sent - a forged `ip=` inside a `user=` value is
/// indistinguishable from a real one. Every byte outside printable ASCII
/// becomes `?`, which keeps the field one token on one line.
fn field(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in String::from_utf8_lossy(raw).chars() {
        out.push(if c.is_ascii_graphic() { c } else { '?' });
    }
    out
}

/// Render ` bytes=.. us=..` when the source supplied size/timing, else empty.
fn format_size(b: &Bucket) -> String {
    if b.has_size {
        format!(" bytes={} us={}", b.bytes, b.us)
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The aggregate line is parsed by operators and by scripts that key on
    /// `key=value` pairs, so a value must never be able to add a pair of its
    /// own. `user` arrives in an identity header and `ip` from the peer, both
    /// attacker-influenced.
    fn new_bucket() -> Bucket {
        Bucket {
            key: Vec::new(),
            study: Vec::new(),
            user: Vec::new(),
            ip: Vec::new(),
            slices: 0,
            stored: 0,
            idempotent: 0,
            failed: 0,
            conflicts: 0,
            bytes: 0,
            us: 0,
            has_size: false,
            last: Instant::now(),
        }
    }

    #[test]
    fn a_newline_in_user_or_ip_cannot_forge_another_field() {
        let mut b = new_bucket();
        b.study = b"1.2.3".to_vec();
        b.user = b"alice ip=10.0.0.7\ninjected=1".to_vec();
        b.ip = b"10.0.0.1\r\nX".to_vec();
        for line in [
            format_started(&b),
            format_completed(&b),
            format_progress(&b),
            format_partial(&b),
        ] {
            assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
            // One line, one `ip=` token: the value could not add a second.
            assert_eq!(line.matches(" ip=").count(), 1, "{line}");
        }
        // Printable ASCII survives unchanged, so ordinary names stay readable.
        assert_eq!(field(b"alice"), "alice");
        assert_eq!(field(b"10.0.0.1"), "10.0.0.1");
    }

    /// Drive a private bucket vector (not the shared global) so these tests
    /// run in parallel without racing on the one `TABLE`.
    struct Harness {
        buckets: Vec<Bucket>,
    }

    impl Harness {
        fn new() -> Self {
            Harness {
                buckets: Vec::new(),
            }
        }

        /// Record one slice, forwarding to the same core the production path
        /// uses.
        #[allow(clippy::too_many_arguments)]
        fn slice(
            &mut self,
            study: &str,
            user: &str,
            ip: &str,
            stored: u64,
            idempotent: u64,
            failed: u64,
            conflicts: u64,
            now: Instant,
        ) -> Vec<String> {
            record_slices(
                &mut self.buckets,
                study.as_bytes(),
                user.as_bytes(),
                ip.as_bytes(),
                stored,
                idempotent,
                failed,
                conflicts,
                None,
                None,
                now,
            )
        }

        /// Record one slice that DOES carry the size/timing fields, so the
        /// emitted lines include `bytes=` and `us=`.
        // A test helper mirroring `record_slices`; a struct here would only
        // move the same fields behind another name.
        #[allow(clippy::too_many_arguments)]
        fn slice_sized(
            &mut self,
            study: &str,
            user: &str,
            ip: &str,
            stored: u64,
            bytes: u64,
            us: u64,
            now: Instant,
        ) -> Vec<String> {
            record_slices(
                &mut self.buckets,
                study.as_bytes(),
                user.as_bytes(),
                ip.as_bytes(),
                stored,
                0,
                0,
                0,
                Some(bytes),
                Some(us),
                now,
            )
        }

        /// Run the periodic sweep over THIS vector, as the background tick does.
        fn sweep(&mut self, now: Instant) -> Vec<String> {
            sweep_buckets(&mut self.buckets, now)
        }
    }

    #[test]
    fn new_bucket_emits_started_only_on_first_slice() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // First slice of a new key: the start marker is the FIRST line.
        let out = h.slice("1.2.3", "dr.smith", "10.0.0.1", 2, 0, 0, 0, t0);
        assert!(
            out.iter()
                .any(|l| l.starts_with("started study=1.2.3 user=dr.smith ip=10.0.0.1"))
        );
        // A second slice of the same session is silent (totals defer to TTL).
        let out = h.slice("1.2.3", "dr.smith", "10.0.0.1", 1, 1, 0, 0, t0);
        assert!(out.is_empty(), "existing session must stay silent");
    }

    #[test]
    fn key_is_per_study_user_ip() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // Same study but different user -> separate session.
        let a = h.slice("s1", "u1", "ip", 1, 0, 0, 0, t0);
        let b = h.slice("s1", "u2", "ip", 1, 0, 0, 0, t0);
        assert!(a.iter().any(|l| l.starts_with("started study=")));
        assert!(b.iter().any(|l| l.starts_with("started study=")));
        // Same study+user but different ip -> separate session.
        let c = h.slice("s1", "u1", "9.9.9.9", 1, 0, 0, 0, t0);
        assert!(c.iter().any(|l| l.starts_with("started study=")));
    }

    #[test]
    fn ttl_emits_completed_with_totals() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // One session aggregated across three slices (totals accumulate).
        let _ = h.slice("1.2.3", "dr.smith", "10.0.0.1", 3, 1, 1, 0, t0);
        let _ = h.slice("1.2.3", "dr.smith", "10.0.0.1", 2, 0, 1, 1, t0);
        // A foreign slice after TTL sweeps the expired session as `completed`
        // with the summed totals; bytes/us are absent (source None).
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.slice("9.9.9", "u", "ip", 1, 0, 0, 0, t1);
        assert!(out.iter().any(|l| {
            l.contains("completed study=1.2.3 slices=2 stored=5 idempotent=1 failed=2 conflicts=1")
                && !l.contains("bytes=")
                && !l.contains("us=")
        }));
    }

    #[test]
    fn lru_eviction_emits_partial() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // Build CAP lingering buckets (never quiet within the window), then a
        // further distinct bucket evicts the LRU and emits it `partial`.
        for i in 0..CAP {
            let study = format!("s{i}");
            let _ = h.slice(
                &study,
                "u",
                "ip",
                1,
                0,
                0,
                0,
                t0 + Duration::from_nanos(i as u64),
            );
        }
        let out = h.slice(
            "brand-new",
            "u",
            "ip",
            1,
            0,
            0,
            0,
            t0 + Duration::from_secs(1),
        );
        assert!(
            out.iter().any(|l| l.contains("partial study=s0")),
            "LRU eviction must emit partial"
        );
    }

    #[test]
    fn completed_includes_size_when_supplied() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice_sized("1.2.3", "u", "ip", 1, 1000, 50, t0);
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.slice_sized("9.9.9", "u", "ip", 1, 2000, 20, t1);
        assert!(
            out.iter().any(|l| {
                l.contains("completed study=1.2.3 slices=1 stored=1")
                    && l.contains("bytes=1000 us=50")
            }),
            "completed must carry the supplied size fields: {out:?}"
        );
    }

    #[test]
    fn progress_emits_every_50_slices() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // Slice 1 starts the session; slices 2..49 are silent.
        for i in 1..=49 {
            let out = h.slice("1.2.3", "u", "ip", 1, 0, 0, 0, t0);
            if i == 1 {
                assert!(out.iter().any(|l| l.starts_with("started study=")));
            } else {
                assert!(out.is_empty(), "slice {i} must be silent");
            }
        }
        // The 50th slice emits the progress marker.
        let out = h.slice("1.2.3", "u", "ip", 1, 0, 0, 0, t0);
        assert!(
            out.iter()
                .any(|l| l.contains("progress study=1.2.3 slices=50")),
            "50th slice must emit progress: {out:?}"
        );
        // Slices 51..99 are silent again; the 100th emits the next marker.
        for _ in 51..=99 {
            let out = h.slice("1.2.3", "u", "ip", 1, 0, 0, 0, t0);
            assert!(out.is_empty());
        }
        let out = h.slice("1.2.3", "u", "ip", 1, 0, 0, 0, t0);
        assert!(
            out.iter()
                .any(|l| l.contains("progress study=1.2.3 slices=100")),
            "100th slice must emit the next marker: {out:?}"
        );
    }

    #[test]
    fn progress_lines_every_50_and_completed_after_ttl() {
        let t0 = Instant::now();
        // 130 slices of one study via record_at with synthetic timestamps.
        // Order: started@1, silence 2-49, progress@50, progress@100, then a
        // TTL-away foreign slice surfaces completed ... slices=130.
        let mut h = Harness::new();
        let mut saw_started = false;
        let mut saw_50 = false;
        let mut saw_100 = false;
        for i in 1..=130u64 {
            let out = h.slice("130.9", "dr.smith", "10.0.0.1", 1, 0, 0, 0, t0);
            match i {
                1 => {
                    assert!(
                        out.iter().any(|l| l.starts_with("started study=130.9")),
                        "slice 1 must start: {out:?}"
                    );
                    saw_started = true;
                }
                50 => {
                    assert!(
                        out.iter()
                            .any(|l| l.contains("progress study=130.9 slices=50")),
                        "slice 50 must progress: {out:?}"
                    );
                    saw_50 = true;
                }
                100 => {
                    assert!(
                        out.iter()
                            .any(|l| l.contains("progress study=130.9 slices=100")),
                        "slice 100 must progress: {out:?}"
                    );
                    saw_100 = true;
                }
                _ => {
                    assert!(out.is_empty(), "slice {i} must be silent");
                }
            }
        }
        assert!(saw_started && saw_50 && saw_100);
        // A foreign slice after TTL sweeps the session as `completed` with the
        // full 130-slice totals.
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.slice("9.9.9", "u", "ip", 1, 0, 0, 0, t1);
        assert!(
            out.iter()
                .any(|l| l.contains("completed study=130.9 slices=130")),
            "TTL must complete with slices=130: {out:?}"
        );
    }

    #[test]
    fn periodic_sweep_emits_completed_by_ttl_and_removes() {
        // Drive the shared sweep_buckets pass over a private vector (parallel
        // safe, no race on the global TABLE).
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("1.2.3", "dr.smith", "10.0.0.1", 4, 1, 0, 0, t0);
        // A second, still-young session is also present; the sweep after TTL
        // must complete the expired one but leave the young one untouched.
        let _ = h.slice(
            "2.2.2",
            "dr.smith",
            "10.0.0.1",
            1,
            0,
            0,
            0,
            t0 + Duration::from_secs(1),
        );
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = sweep_buckets(&mut h.buckets, t1);
        assert_eq!(out.len(), 1, "one expired bucket -> one completed: {out:?}");
        assert!(
            out[0].contains("completed study=1.2.3 slices=1 stored=4 idempotent=1"),
            "sweep must emit completed with totals: {out:?}"
        );
        // The expired bucket is gone; the young one (2.2.2) survived.
        assert_eq!(h.buckets.len(), 1);
        assert_eq!(String::from_utf8_lossy(&h.buckets[0].study), "2.2.2");
    }

    #[test]
    fn periodic_sweep_empty_and_repeated_emit_no_duplicates() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // Empty vector: no lines.
        assert!(sweep_buckets(&mut h.buckets, t0).is_empty());
        // Seed one bucket, expire it; sweep ONCE removes it.
        let _ = h.slice("1.2.3", "u", "ip", 1, 0, 0, 0, t0);
        let t1 = t0 + TTL + Duration::from_secs(1);
        let once = sweep_buckets(&mut h.buckets, t1);
        assert_eq!(once.len(), 1);
        // A second sweep has nothing left: no duplicate line.
        assert!(h.buckets.is_empty());
        assert!(sweep_buckets(&mut h.buckets, t1).is_empty());
    }

    #[test]
    fn periodic_sweep_leaves_young_bucket_untouched() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("1.2.3", "u", "ip", 1, 0, 0, 0, t0);
        // Sweep just under TTL: bucket stays, no line.
        let soon = t0 + TTL - Duration::from_secs(1);
        assert!(sweep_buckets(&mut h.buckets, soon).is_empty());
        assert_eq!(h.buckets.len(), 1);
    }

    #[test]
    fn periodic_sweep_global_uses_real_clock_path() {
        // Drive the same `sweep_buckets` pass over a private vector. The
        // previous version used the global TABLE with a unique study key,
        // which made it pass only as long as no other test touched that table
        // concurrently - a test that depends on nobody else running is not a
        // test of the sweep, it is a test of the scheduler.
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("sweep.9.1", "u", "ip", 2, 0, 0, 0, t0);
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.sweep(t1);
        assert!(
            out.iter().any(|l| l.contains("completed study=sweep.9.1")),
            "the sweep must surface the expired study: {out:?}"
        );
        assert!(
            h.buckets.is_empty(),
            "the sweep must remove what it completed"
        );
    }
}
