// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Disk health probe - dedicated OS thread measures write latency.
//!
//! Every 5 seconds, writes a 4 KB probe file to `storage_dir`,
//! measures wall-clock latency, and stores it in a mutex-protected
//! [`ProbeState`]. This is a separate OS thread (NOT tokio) to avoid being
//! blocked by the async runtime under I/O pressure.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// The latest completed probe latency (ms) plus the start of any probe that is
/// still in flight.
///
/// A hung write (NAS hard mount) never returns, so storing `in_flight_since`
/// BEFORE the write is what lets [`probe_latency_ms`] report a growing number
/// for a write that has not completed - the alerting rule it feeds fires on
/// high values, and a probe that freezes must degrade, not freeze (1611 #12).
struct ProbeState {
    /// Latency of the last COMPLETED probe pass, or `None` before the first.
    last_ms: Option<u64>,
    /// `Some(t)` while a probe write is running and has not yet returned.
    in_flight_since: Option<Instant>,
    /// Whether the last COMPLETED probe failed to write its file.
    ///
    /// Held under the SAME lock as the two fields above. It used to be a
    /// separate atomic read BEFORE the lock was taken, which meant a hung
    /// retry after a failure was invisible: the flag short-circuited
    /// [`probe_latency_ms`] to `None` before anyone could look at
    /// `in_flight_since`, so the one state that most needs a growing number -
    /// a failed disk now hanging - reported "unknown" forever.
    failed: bool,
}

static PROBE_STATE: Mutex<ProbeState> = Mutex::new(ProbeState {
    last_ms: None,
    in_flight_since: None,
    failed: false,
});

/// 1 once at least one probe pass has completed, success or failure.
static PROBE_RAN: AtomicU64 = AtomicU64::new(0);

/// Latest probe latency in milliseconds, or `None` while no probe has
/// completed, the last one failed, or a write is hung past the last reading.
///
/// A failed probe measures how long the FAILURE took, which is not disk
/// latency: a write rejected instantly by a full or read-only filesystem
/// reports about 0 ms, and that is the most alarming state there is. Publishing
/// it as a healthy low latency is worse than publishing nothing, because the
/// alerting rule it feeds fires on high values only - so a dying volume would
/// look like the fastest disk in the fleet. The probe clears the reading on
/// failure and the gauge is then omitted, which is what "unknown" means for a
/// gauge.
///
/// A HUNG write is the opposite case and OUTRANKS the failure flag: it reports
/// the time spent waiting, which grows without bound and therefore crosses the
/// 30 s degraded rule. The failure suppression applies only to a COMPLETED
/// failed pass; an in-flight write is measured first, whatever the previous
/// pass did.
pub fn probe_latency_ms() -> Option<u64> {
    let state = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    probe_latency_from_state(&state, Instant::now())
}

/// Pure latency computation over a state snapshot (separated so a test can
/// inject a fake "now").
///
/// Order matters: an in-flight write is measured BEFORE the failure flag can
/// suppress anything. A completed failure yields `None` (unknown latency);
/// a hung retry after a failure yields its growing elapsed time.
fn probe_latency_from_state(state: &ProbeState, now: Instant) -> Option<u64> {
    let hung = state
        .in_flight_since
        .map(|t| now.duration_since(t).as_millis() as u64);
    if let Some(h) = hung {
        // Still running: the elapsed wait is a real measurement, and the
        // previous pass's outcome says nothing about this one.
        return Some(state.last_ms.map_or(h, |last| last.max(h)));
    }
    if state.failed {
        return None;
    }
    state.last_ms
}

/// Whether any probe pass has completed yet.
pub fn probe_has_run() -> bool {
    PROBE_RAN.load(Ordering::Relaxed) != 0
}

/// Return the latest disk probe latency in milliseconds.
/// 0 means the probe hasn't completed yet.
pub fn disk_latency_ms() -> u64 {
    PROBE_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_ms
        .unwrap_or(0)
}

/// Execute one probe pass against a probe state.
///
/// The `in_flight_since` stamp is taken BEFORE the write so a write that never
/// returns is visible to [`probe_latency_ms`] as a growing latency.
/// `write_probe` performs the actual file write and returns whether it
/// succeeded; `now` is the clock (injected by tests, `Instant::now` in
/// production).
fn probe_once(
    state: &Mutex<ProbeState>,
    write_probe: impl FnOnce() -> bool,
    now: impl Fn() -> Instant,
) {
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.in_flight_since = Some(now());
    }
    let t0 = now();
    let wrote = write_probe();
    let ms = now().duration_since(t0).as_millis() as u64;
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.in_flight_since = None;
        if wrote {
            st.last_ms = Some(ms);
            st.failed = false;
        } else {
            // Keep the last good reading and mark the gauge unknown instead:
            // the elapsed time of a rejection is not latency.
            st.failed = true;
        }
    }
    PROBE_RAN.store(1, Ordering::Relaxed);
}

/// Spawn a dedicated OS thread that periodically writes a probe file
/// to measure disk health. The thread runs until the process exits.
///
/// # Panics
/// Panics if the OS thread cannot be spawned.
pub fn spawn_disk_probe(storage_dir: PathBuf) {
    std::thread::Builder::new()
        .name("disk-probe".into())
        .spawn(move || {
            let tmp = storage_dir.join(".healthcheck_probe");
            let data = [0u8; 4096];
            loop {
                // A write that FAILS measures the failure, not the disk: the
                // elapsed time of a rejection is not latency. Keep the last
                // good reading and mark the gauge unknown instead. A write
                // that HANGS is reported as growing latency by the in-flight
                // stamp taken inside `probe_once`.
                probe_once(
                    &PROBE_STATE,
                    || {
                        let wrote = std::fs::write(&tmp, data).is_ok();
                        if wrote {
                            std::fs::remove_file(&tmp).is_ok()
                        } else {
                            false
                        }
                    },
                    Instant::now,
                );
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        })
        .expect("Failed to spawn disk probe thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// Serializes the two probe tests: both touch the process-global
    /// `PROBE_STATE`/`PROBE_RAN` statics (probe_once always updates them),
    /// and a parallel run would interleave their stores.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// A hung write must degrade the gauge instead of freezing it: with a
    /// previous reading of 7 ms and a write that started 45 s ago, the gauge
    /// must report >= 45 s (before the fix it answered the last value, 7).
    #[test]
    fn hung_write_latency_grows_past_30s() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = Mutex::new(ProbeState {
            last_ms: Some(7),
            in_flight_since: None,
            failed: false,
        });
        let shared = Arc::new(state);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                // The write "never completes" until the test releases it.
                probe_once(
                    &shared,
                    || {
                        started_tx.send(()).expect("signal start");
                        let _ = release_rx.recv();
                        true
                    },
                    Instant::now,
                );
            })
        };

        // The body stamps `in_flight_since` BEFORE the write, so the start
        // signal guarantees the stamp is already visible.
        started_rx.recv().expect("probe started");
        let in_flight_since = {
            let st = shared.lock().unwrap_or_else(|e| e.into_inner());
            st.in_flight_since.expect("probe must be in flight")
        };
        // Advance the fake clock by 45 s past the write start.
        let fake_now = in_flight_since + Duration::from_secs(45);
        let ms = {
            let st = shared.lock().unwrap_or_else(|e| e.into_inner());
            probe_latency_from_state(&st, fake_now)
        };
        assert!(
            ms >= Some(45_000),
            "a hung write must report a growing latency, got {ms:?}"
        );

        release_tx.send(()).expect("release");
        worker.join().expect("probe thread joins");
    }

    /// A hung retry AFTER a failed probe must be reported, not suppressed.
    ///
    /// The failure flag used to be a separate atomic read BEFORE the lock, so
    /// `probe_latency_ms` returned `None` at the first test and never looked
    /// at `in_flight_since`. A disk that rejected a write and then HUNG on the
    /// retry - the exactly-worst state - therefore reported "unknown" forever
    /// instead of the growing number the alerting rule needs.
    #[test]
    fn a_hung_retry_after_a_failure_is_reported() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = Instant::now();
        let state = ProbeState {
            last_ms: Some(11),
            in_flight_since: None,
            // The previous pass failed; this one has not returned.
            failed: true,
        };
        // Completed failure: unknown latency, as before.
        assert_eq!(probe_latency_from_state(&state, base), None);

        // Now a retry is in flight and has been hanging for 40 s.
        let hung = ProbeState {
            in_flight_since: Some(base),
            ..state
        };
        let ms = probe_latency_from_state(&hung, base + Duration::from_secs(40));
        assert_eq!(
            ms,
            Some(40_000),
            "an in-flight write outranks the previous pass's failure flag"
        );
    }

    /// The public gauge must follow the probe's OWN transitions: omit latency
    /// after a failed write, publish it again after a success.
    ///
    /// The previous version set the flags by hand and would have passed even
    /// if `probe_once` stopped publishing either transition. This one injects
    /// the write outcome through `probe_once`, so the transition itself is
    /// under test.
    #[test]
    fn the_gauge_follows_the_probe_through_failure_and_recovery() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut st = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
            *st = ProbeState {
                last_ms: None,
                in_flight_since: None,
                failed: false,
            };
        }

        // A successful pass publishes a reading.
        probe_once(&PROBE_STATE, || true, Instant::now);
        assert!(
            probe_latency_ms().is_some(),
            "a successful probe publishes its latency"
        );

        // A failed pass makes the gauge unknown, even though a good reading
        // exists: the elapsed time of a rejection is not latency.
        probe_once(&PROBE_STATE, || false, Instant::now);
        assert!(
            probe_latency_ms().is_none(),
            "a failed probe must be unknown, not the previous reading"
        );

        // Recovery: the next successful pass publishes again.
        probe_once(&PROBE_STATE, || true, Instant::now);
        assert!(
            probe_latency_ms().is_some(),
            "a recovered disk must publish its latency again"
        );

        // Cleanup: restore the global to its not-yet-run state.
        {
            let mut st = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
            *st = ProbeState {
                last_ms: None,
                in_flight_since: None,
                failed: false,
            };
        }
    }
}
