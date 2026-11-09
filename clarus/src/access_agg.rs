// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! WADO series access aggregation.
//!
//! One slice (a WADO-RS instance GET) is a transport unit, not a readable
//! event; a viewer loads a whole series across many slices. Emitting an
//! ACCESS line per slice would drown the journal for a large study. This
//! module accumulates the slices of one series into a bucket keyed strictly
//! by `(user, client_ip, study_uid, series_uid)` and emits ONE line when the
//! series completes (`fetched >= expected`) or times out (60s of inactivity
//! -> `partial`).
//!
//! The module is pure: it formats its own nginx kv lines and touches neither
//! the log channel, `clog`, nor the connector. Callers route the returned
//! lines onto the journal themselves.
//!
//! The key deliberately has NO loopback / x-clarus / client-kind dimension:
//! any slice from any network segment lands in the same bucket as the owner
//! intends. `user` arrives already-labelled (`(miniviewer)` / `(loopback)` /
//! auth name); this module never inspects it.

#![forbid(unsafe_code)]

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A dropped series lingers at most this long after its last slice before it
/// is emitted as `partial` and forgotten. 60s is comfortably longer than any
/// inter-slice gap a real viewer produces (typically a fraction of a second)
/// while still surfacing an interrupted transfer promptly.
const TTL: Duration = Duration::from_secs(60);
/// Upper bound of concurrently tracked series. Beyond it the least recently
/// read series is evicted (emitted `partial`) so a burst of one-off series
/// never grows memory without bound.
const CAP: usize = 1024;

/// Accumulated state of one in-progress series.
struct Bucket {
    /// Length-prefixed `(user, ip, study, series)` identity.
    key: Vec<u8>,
    /// Journal label for the client (already labelled upstream).
    user: Vec<u8>,
    /// Effective client address.
    ip: Vec<u8>,
    /// Study instance UID.
    study: Vec<u8>,
    /// Series instance UID.
    series: Vec<u8>,
    /// Number of slices delivered (including failures).
    fetched: u64,
    /// Known slice count from the study manifest; set once on the first
    /// successful resolve and never lowered (a failed slice keeps it).
    expected: Option<u32>,
    /// Number of slices that returned an error status.
    failed: u64,
    /// Sum of delivered bytes.
    bytes: u64,
    /// Sum of handler microseconds.
    us: u64,
    /// Time of the last recorded slice.
    last: Instant,
}

/// The shared bucket table. The `Mutex` is held only for the short lookup /
/// update; a slice never blocks more than one mutex acquisition.
static TABLE: OnceLock<Mutex<Vec<Bucket>>> = OnceLock::new();

fn table() -> &'static Mutex<Vec<Bucket>> {
    TABLE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Build an unambiguous byte key for `(user, ip, study, series)`.
///
/// Length-prefixed so no separator byte can collide across fields (a name or
/// UID may legally contain any ASCII aside from digits/dots). The four-tuple
/// is the ONLY identity: no loopback or marker dimension.
fn key(user: &[u8], ip: &[u8], study: &[u8], series: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(user.len() + ip.len() + study.len() + series.len() + 4 * 4);
    for part in [user, ip, study, series] {
        k.extend_from_slice(&(part.len() as u32).to_le_bytes());
        k.extend_from_slice(part);
    }
    k
}

/// Record one delivered slice (public wrapper using the real clock). Returns
/// the completed aggregator lines, normally zero or one. A simultaneous
/// eviction and completion can yield up to two, hence `Vec`.
///
/// Many fields by design (same shape as `access_line`); suppressed.
#[allow(clippy::too_many_arguments)]
pub fn record(
    user: &[u8],
    ip: &[u8],
    study: &[u8],
    series: &[u8],
    status: u16,
    bytes: u64,
    us: u64,
    expected: Option<u32>,
) -> Vec<String> {
    record_at(
        user,
        ip,
        study,
        series,
        status,
        bytes,
        us,
        expected,
        Instant::now(),
    )
}

/// Testable core: same as [`record`] but with a caller-supplied clock, so
/// unit tests drive TTL expiry without sleeping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_at(
    user: &[u8],
    ip: &[u8],
    study: &[u8],
    series: &[u8],
    status: u16,
    bytes: u64,
    us: u64,
    expected: Option<u32>,
    now: Instant,
) -> Vec<String> {
    let mut buckets = table().lock().unwrap_or_else(|e| e.into_inner());
    record_slices(
        &mut buckets,
        user,
        ip,
        study,
        series,
        status,
        bytes,
        us,
        expected,
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
    user: &[u8],
    ip: &[u8],
    study: &[u8],
    series: &[u8],
    status: u16,
    bytes: u64,
    us: u64,
    expected: Option<u32>,
    now: Instant,
) -> Vec<String> {
    let mut out = Vec::new();

    // Lazy sweep: one pass per record evicts every expired bucket, emitted
    // as `partial` so no series is silently forgotten. The periodic 5s sweep
    // (see [`sweep`]) shares this same pass so an idle server still surfaces
    // interrupted series between records.
    out.extend(sweep_buckets(buckets, now));

    let k = key(user, ip, study, series);
    if let Some(b) = buckets.iter_mut().find(|b| b.key == k) {
        record_into(b, status, bytes, us, expected, now);
        if let Some(n) = b.expected
            && b.fetched >= n as u64
        {
            out.push(format_loaded(b));
            buckets.retain(|x| !same_key(x, &k));
            return out;
        }
        return out;
    }

    // New series: admit (evicting the least recently used entry if at cap).
    if buckets.len() >= CAP
        && let Some((idx, _)) = buckets.iter().enumerate().min_by_key(|(_, b)| b.last)
    {
        out.push(format_partial(&buckets[idx]));
        buckets.swap_remove(idx);
    }
    let mut b = Bucket {
        key: k.clone(),
        user: user.to_vec(),
        ip: ip.to_vec(),
        study: study.to_vec(),
        series: series.to_vec(),
        fetched: 0,
        expected: None,
        failed: 0,
        bytes: 0,
        us: 0,
        last: now,
    };
    record_into(&mut b, status, bytes, us, expected, now);
    // A brand-new bucket is a new fetch session: surface a start marker FIRST,
    // before any loaded/partial line the same slice may also produce.
    let started = format_started(&b);
    if let Some(n) = b.expected
        && b.fetched >= n as u64
    {
        out.push(format_loaded(&b));
        out.insert(0, started);
        // A fresh bucket already at expected is served once - do not retain
        // it for a redundant repeat of the same slice.
        return out;
    }
    out.insert(0, started);
    buckets.push(b);
    out
}

/// One shared TTL pass over a bucket vector: every bucket idle longer than
/// [`TTL`] is emitted as `partial` (an interrupted series) and removed. Used
/// by both the lazy sweep inside [`record_slices`] and the standalone periodic
/// [`sweep`], so an idle server surfaces interrupted transfers instead of
/// waiting for the next slice. Eviction is NOT part of this pass; the
/// CAP-aware eviction stays in [`record_slices`].
fn sweep_buckets(buckets: &mut Vec<Bucket>, now: Instant) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buckets.len() {
        if now.duration_since(buckets[i].last) > TTL {
            out.push(format_partial(&buckets[i]));
            buckets.swap_remove(i);
        } else {
            i += 1;
        }
    }
    out
}

/// Periodic TTL sweep over the shared table (public wrapper using the real
/// clock). Emits a `partial` line for every series idle longer than [`TTL`]
/// and removes it; an empty table yields no lines. The journal thread calls
/// this on a fixed 5s tick so interrupted transfers surface promptly even with
/// no further WADO traffic. Eviction stays in [`record_at`].
pub fn sweep() -> Vec<String> {
    sweep_at(Instant::now())
}

/// Testable periodic sweep: same as [`sweep`] but with a caller-supplied
/// clock, so unit tests drive TTL expiry without sleeping.
pub(crate) fn sweep_at(now: Instant) -> Vec<String> {
    let mut buckets = table().lock().unwrap_or_else(|e| e.into_inner());
    sweep_buckets(&mut buckets, now)
}

/// Apply one slice to a bucket: counters, `last`, one-time `expected`.
fn record_into(
    b: &mut Bucket,
    status: u16,
    bytes: u64,
    us: u64,
    expected: Option<u32>,
    now: Instant,
) {
    b.fetched += 1;
    if status >= 400 {
        b.failed += 1;
    }
    b.bytes += bytes;
    b.us += us;
    if b.expected.is_none() && expected.is_some() {
        b.expected = expected;
    }
    b.last = now;
}

/// Line marking the first slice of a brand-new fetch session for a series.
/// Emitted FIRST for a new bucket, then the normal loaded/partial logic runs.
///
/// `user` and `ip` come from the request path, so they carry attacker-chosen
/// bytes: they are sanitized here, through the same helper the per-request
/// `access_line` uses. Sanitizing only one of the two journal routes would
/// leave the other open to the same control-byte injection.
fn format_started(b: &Bucket) -> String {
    format!(
        "started study={} series={} user={} ip={}",
        String::from_utf8_lossy(&b.study),
        String::from_utf8_lossy(&b.series),
        crate::access_log::sanitize_ctl(&String::from_utf8_lossy(&b.user)),
        crate::access_log::sanitize_ctl(&String::from_utf8_lossy(&b.ip)),
    )
}

/// Line for a fully loaded series (always `of=N`).
fn format_loaded(b: &Bucket) -> String {
    format!(
        "loaded study={} series={} slices={} of={} bytes={} us={} failed={} user={} ip={}",
        String::from_utf8_lossy(&b.study),
        String::from_utf8_lossy(&b.series),
        b.fetched,
        b.expected.unwrap_or(0),
        b.bytes,
        b.us,
        b.failed,
        crate::access_log::sanitize_ctl(&String::from_utf8_lossy(&b.user)),
        crate::access_log::sanitize_ctl(&String::from_utf8_lossy(&b.ip)),
    )
}

/// Line for an interrupted series. `of=N` only when the manifest count is
/// known; otherwise `got=G` alone.
fn format_partial(b: &Bucket) -> String {
    let of = match b.expected {
        Some(n) => format!(" of={n}"),
        None => String::new(),
    };
    format!(
        "partial study={} series={} got={}{} bytes={} us={} failed={} user={} ip={}",
        String::from_utf8_lossy(&b.study),
        String::from_utf8_lossy(&b.series),
        b.fetched,
        of,
        b.bytes,
        b.us,
        b.failed,
        crate::access_log::sanitize_ctl(&String::from_utf8_lossy(&b.user)),
        crate::access_log::sanitize_ctl(&String::from_utf8_lossy(&b.ip)),
    )
}

fn same_key(b: &Bucket, k: &[u8]) -> bool {
    b.key == k
}

#[cfg(test)]
mod tests {
    use super::*;

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
            series: &str,
            user: &str,
            ip: &str,
            status: u16,
            expected: Option<u32>,
            now: Instant,
        ) -> Vec<String> {
            record_slices(
                &mut self.buckets,
                user.as_bytes(),
                ip.as_bytes(),
                study.as_bytes(),
                series.as_bytes(),
                status,
                1000,
                50,
                expected,
                now,
            )
        }
    }

    #[test]
    fn completion_emits_loaded_and_removes_bucket() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("1.2.3", "4.5.6", "dr.smith", "10.0.0.1", 200, Some(2), t0);
        let out = h.slice("1.2.3", "4.5.6", "dr.smith", "10.0.0.1", 200, Some(2), t0);
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("loaded study=1.2.3 series=4.5.6 slices=2 of=2"));
        // Bucket removed: a third slice of the same series no longer completes.
        // It opens a fresh session, so a started marker appears but no loaded.
        let out = h.slice("1.2.3", "4.5.6", "dr.smith", "10.0.0.1", 200, Some(2), t0);
        assert!(
            out.iter().all(|l| !l.starts_with("loaded study=")),
            "completed series must not re-emit loaded"
        );
    }

    #[test]
    fn aggregate_lines_sanitize_control_bytes_in_user_and_ip() {
        // `user` and `ip` reach the aggregator from the request path. A
        // control byte here would land in the journal through the WADO_SERIES
        // route - the second of the two journal paths - so this is the check
        // that the shared helper really is applied on both.
        let mut h = Harness::new();
        let t0 = Instant::now();
        // `started` line: user carries ESC, ip carries NUL.
        let out = h.slice("1.2.3", "4.5.6", "d\u{1b}r", "10.0.\0.1", 200, Some(2), t0);
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("started "), "got: {}", out[0]);
        assert!(
            !out[0].contains('\u{1b}') && !out[0].contains('\0'),
            "started line must be sanitized: {:?}",
            out[0]
        );
        assert!(
            out[0].contains("user=d?r") && out[0].contains("ip=10.0.?.1"),
            "control bytes become '?': {:?}",
            out[0]
        );

        // `loaded` line: TAB in the user, DEL in the ip.
        let out = h.slice("s1", "se1", "u\t1", "ip\u{7f}1", 200, Some(1), t0);
        assert_eq!(out.len(), 2, "started + loaded");
        let loaded = out.iter().find(|l| l.starts_with("loaded ")).unwrap();
        assert!(
            !loaded.contains('\t') && !loaded.contains('\u{7f}'),
            "loaded line must be sanitized: {loaded:?}"
        );
        assert!(
            loaded.contains("user=u?1") && loaded.contains("ip=ip?1"),
            "control bytes become '?': {loaded:?}"
        );
    }

    #[test]
    fn aggregate_partial_line_sanitizes_control_bytes() {
        // The TTL sweep path (`partial`) is the third renderer; it shares the
        // same bucket fields and must be sanitized too.
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("s", "se", "u\u{1b}1", "ip\u{7f}1", 200, None, t0);
        // Advance past the TTL so the sweep evicts and emits a partial line.
        let out = h.slice("s", "se2", "other", "other", 200, Some(1), t0 + TTL * 2);
        let partial = out
            .iter()
            .find(|l| l.starts_with("partial "))
            .unwrap_or_else(|| panic!("expected a partial line in {out:?}"));
        assert!(
            !partial.contains('\u{1b}') && !partial.contains('\u{7f}'),
            "partial line must be sanitized: {partial:?}"
        );
    }

    #[test]
    fn ge_guard_prevents_repeat_loaded() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // Three slices reach N=3 -> loaded emitted on the 3rd.
        let _ = h.slice("s1", "se1", "u1", "ip1", 200, Some(3), t0);
        let _ = h.slice("s1", "se1", "u1", "ip1", 200, Some(3), t0);
        let out = h.slice("s1", "se1", "u1", "ip1", 200, Some(3), t0);
        assert_eq!(out.len(), 1);
        // 4th slice (re-fetch of a SOP with another transfer syntax): no new
        // loaded line, because the completed bucket was already removed. It
        // opens a fresh session (started), but must not re-emit loaded.
        let out = h.slice("s1", "se1", "u1", "ip1", 200, Some(3), t0);
        assert!(
            out.iter().all(|l| !l.starts_with("loaded study=")),
            "completed series must not re-emit loaded"
        );
    }

    #[test]
    fn single_slice_series_completes_immediately() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // A one-slice series completes on its single slice and leaves no
        // lingering bucket (so memory stays bounded for many single-study
        // fetches). The new bucket still opens with a `started` marker, then
        // emits `loaded` on the same slice.
        let out = h.slice("1.2", "3.4", "u", "ip", 200, Some(1), t0);
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with("started study=1.2 series=3.4"));
        assert!(out[1].contains("slices=1 of=1"));
        assert!(h.buckets.is_empty(), "completed bucket must not linger");
    }

    #[test]
    fn ttl_emits_partial_with_of() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("1.2.3", "4.5.6", "u", "ip", 200, Some(4), t0);
        // Activity stops; a later record (different series) sweeps and emits
        // the expired series as `partial` with `of=4`.
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.slice("2.2.2", "9.9.9", "u", "ip", 200, Some(1), t1);
        assert!(
            out.iter()
                .any(|l| { l.contains("partial study=1.2.3 series=4.5.6 got=1 of=4") })
        );
    }

    #[test]
    fn partial_without_n_omits_of() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("1.2.3", "4.5.6", "u", "ip", 200, None, t0);
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.slice("2.2.2", "9.9.9", "u", "ip", 200, Some(1), t1);
        assert!(out.iter().any(|l| {
            l.contains("partial study=1.2.3 series=4.5.6 got=1") && !l.contains("of=")
        }));
    }

    #[test]
    fn key_is_per_series_user_ip() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // Different series, same user/ip: separate buckets, each completes
        // with a started marker first, then loaded.
        let a = h.slice("s1", "seA", "u", "ip", 200, Some(1), t0);
        let b = h.slice("s1", "seB", "u", "ip", 200, Some(1), t0);
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
        assert!(a[0].starts_with("started study=s1 series=seA"));
        assert!(b[0].starts_with("started study=s1 series=seB"));

        // Different user, same series+ip: separate bucket.
        let c = h.slice("s1", "seA", "other", "ip", 200, Some(1), t0);
        assert!(c[0].contains("user=other"));
        // Different ip, same series+user: separate bucket.
        let d = h.slice("s1", "seA", "u", "9.9.9.9", 200, Some(1), t0);
        assert!(d[0].contains("ip=9.9.9.9"));
    }

    #[test]
    fn failed_under_400_increments_failed() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("s", "se", "u", "ip", 200, Some(2), t0);
        let out = h.slice("s", "se", "u", "ip", 404, Some(2), t0);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("failed=1"));
    }

    #[test]
    fn lru_eviction_emits_partial() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // Build CAP lingering buckets (expected none -> never complete), then
        // a further distinct bucket evicts the LRU and emits it `partial`.
        for i in 0..CAP {
            let study = format!("s{i}");
            let _ = h.slice(
                &study,
                "se",
                "u",
                "ip",
                200,
                None,
                t0 + Duration::from_nanos(i as u64),
            );
        }
        // All CAP buckets age in timestamp order; a new one evicts the oldest.
        let out = h.slice(
            "brand-new",
            "se",
            "u",
            "ip",
            200,
            None,
            t0 + Duration::from_secs(1),
        );
        assert!(out.iter().any(|l| l.contains("partial study=s0")));
    }

    #[test]
    fn sweep_removes_expired_on_other_record() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("old", "se", "u", "ip", 200, None, t0);
        // Advance past TTL and record a DIFFERENT series: the lazy sweep on
        // this foreign record emits the expired `old` bucket as `partial`.
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.slice("new", "se", "u", "ip", 200, None, t1);
        assert!(
            out.iter()
                .any(|l| { l.contains("study=old") && l.contains("got=1") && !l.contains("of=") })
        );
        // Re-recording `old` after TTL now starts a FRESH bucket (the old one
        // was swept): a new `old` bucket holds a single slice, not the two of
        // an unswept append.
        let _ = h.slice(
            "old",
            "se",
            "u",
            "ip",
            200,
            None,
            t1 + Duration::from_secs(1),
        );
        let old = h
            .buckets
            .iter()
            .find(|b| b.study == b"old" && b.series == b"se")
            .expect("fresh old bucket present");
        assert_eq!(old.fetched, 1);
    }

    #[test]
    fn expected_is_set_once_not_lowered() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("s", "se", "u", "ip", 200, Some(5), t0);
        // A failed resolve (expected=None) must NOT lower the bucket to None.
        let out = h.slice("s", "se", "u", "ip", 500, None, t0);
        // failed slice bumps count but expected stays 5; no loaded yet.
        assert!(out.is_empty());
        // Now advance to TTL: partial must carry of=5 (not None).
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.slice("x", "x", "u", "ip", 200, Some(1), t1);
        assert!(
            out.iter()
                .any(|l| l.contains("study=s series=se got=2 of=5"))
        );
    }

    #[test]
    fn new_bucket_emits_started_first_only_on_first_slice() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        // First slice of a new key: the start marker is the FIRST line.
        let out = h.slice("1.2.3", "4.5.6", "dr.smith", "10.0.0.1", 200, Some(3), t0);
        assert!(
            out.first()
                .is_some_and(|l| { l.starts_with("started study=1.2.3 series=4.5.6") })
        );
        // Second slice of the same (still-incomplete, needs 3) bucket: no
        // started line (an existing key does not re-open).
        let out = h.slice("1.2.3", "4.5.6", "dr.smith", "10.0.0.1", 200, Some(3), t0);
        assert!(out.is_empty(), "no started for an existing bucket");
        // A third slice completes the series; still no started (existing key),
        // only the loaded line.
        let out = h.slice("1.2.3", "4.5.6", "dr.smith", "10.0.0.1", 200, Some(3), t0);
        assert_eq!(out.len(), 1);
        assert!(
            out.iter()
                .any(|l| l.starts_with("loaded study=") && l.contains("slices=3 of=3"))
        );
    }

    #[test]
    fn after_ttl_new_bucket_starts_marker_again() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("1.2.3", "4.5.6", "u", "ip", 200, None, t0);
        // A foreign record after TTL sweeps the expired bucket and emits it
        // partial; the foreign key is itself a NEW bucket that re-opens with
        // started.
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = h.slice("2.2.2", "9.9.9", "u", "ip", 200, None, t1);
        assert!(
            out.iter()
                .any(|l| l.starts_with("partial study=1.2.3 series=4.5.6 got=1"))
        );
        assert!(
            out.first()
                .is_some_and(|l| { l.starts_with("started study=2.2.2 series=9.9.9") })
        );
        // Re-recording the ORIGINAL key after it was swept starts a fresh
        // bucket: started again for `1.2.3`.
        let t2 = t1 + Duration::from_secs(1);
        let out = h.slice("1.2.3", "4.5.6", "u", "ip", 200, None, t2);
        assert!(
            out.first()
                .is_some_and(|l| { l.starts_with("started study=1.2.3 series=4.5.6") })
        );
    }

    #[test]
    fn periodic_sweep_emits_partial_by_ttl_and_removes() {
        // Drive the shared sweep_buckets pass over a private vector (parallel
        // safe, no race on the global TABLE).
        let mut h = Harness::new();
        let t0 = Instant::now();
        let _ = h.slice("1.2.3", "4.5.6", "dr.smith", "10.0.0.1", 200, Some(4), t0);
        // A second, still-young session is also present (expected=10 with 1
        // slice -> below expected, so it lingers); the sweep after TTL must
        // partial the expired one but leave the young one untouched. A bucket
        // with expected=1 would complete immediately and be removed, which is
        // not what this test exercises.
        let _ = h.slice(
            "2.2.2",
            "9.9.9",
            "dr.smith",
            "10.0.0.1",
            200,
            Some(10),
            t0 + Duration::from_secs(1),
        );
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = sweep_buckets(&mut h.buckets, t1);
        assert_eq!(out.len(), 1, "one expired bucket -> one partial: {out:?}");
        assert!(
            out[0].contains("partial study=1.2.3 series=4.5.6 got=1 of=4"),
            "sweep must emit partial with totals: {out:?}"
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
        let _ = h.slice("1.2.3", "4.5.6", "u", "ip", 200, None, t0);
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
        let _ = h.slice("1.2.3", "4.5.6", "u", "ip", 200, Some(4), t0);
        // Sweep just under TTL: bucket stays, no line.
        let soon = t0 + TTL - Duration::from_secs(1);
        assert!(sweep_buckets(&mut h.buckets, soon).is_empty());
        assert_eq!(h.buckets.len(), 1);
    }

    #[test]
    fn periodic_sweep_global_uses_real_clock_path() {
        // Smoke the public sweep_at (global table) with a unique key, mirroring
        // the record_at-based tests that already share the table.
        let t0 = Instant::now();
        let _ = record_at(
            b"u",
            b"ip",
            b"sweep.study.7",
            b"sweep.series.7",
            200,
            1000,
            50,
            Some(4),
            t0,
        );
        let t1 = t0 + TTL + Duration::from_secs(1);
        let out = sweep_at(t1);
        assert!(
            out.iter()
                .any(|l| l.contains("partial study=sweep.study.7 series=sweep.series.7")),
            "global sweep must surface the expired series: {out:?}"
        );
    }
}
