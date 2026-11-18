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
}

static PROBE_STATE: Mutex<ProbeState> = Mutex::new(ProbeState {
    last_ms: None,
    in_flight_since: None,
});

/// 1 when the last probe failed to write its file. Starts at 0: the probe has
/// not run yet, and reporting a not-yet-measured disk as failed would make
/// `/health` answer 503 in the first moments of every start, turning a restart
/// window into a disk alert.
static PROBE_FAILED: AtomicU64 = AtomicU64::new(0);

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
/// A HUNG write is the opposite case: it reports the time spent waiting, which
/// grows without bound and therefore crosses the 30 s degraded rule. It is
/// folded in as `max(last_ms, now - in_flight_since)`.
pub fn probe_latency_ms() -> Option<u64> {
    if PROBE_FAILED.load(Ordering::Relaxed) != 0 {
        return None;
    }
    let state = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    probe_latency_from_state(&state, Instant::now())
}

/// Pure latency computation over a state snapshot (separated so a test can
/// inject a fake "now").
fn probe_latency_from_state(state: &ProbeState, now: Instant) -> Option<u64> {
    let hung = state
        .in_flight_since
        .map(|t| now.duration_since(t).as_millis() as u64);
    match (state.last_ms, hung) {
        (None, None) => None,
        (Some(last), None) => Some(last),
        (None, Some(h)) => Some(h),
        (Some(last), Some(h)) => Some(last.max(h)),
    }
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
            PROBE_FAILED.store(0, Ordering::Relaxed);
        } else {
            // Keep the last good reading and mark the gauge unknown instead:
            // the elapsed time of a rejection is not latency.
            PROBE_FAILED.store(1, Ordering::Relaxed);
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
    /// `PROBE_FAILED`/`PROBE_RAN` statics (probe_once always updates them),
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

    /// A failed write still yields `None` from the public gauge (the elapsed
    /// time of a rejection is not latency), even with a previous good reading.
    #[test]
    fn failed_write_keeps_the_gauge_unknown() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut st = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
            st.last_ms = Some(11);
        }
        PROBE_FAILED.store(1, Ordering::Relaxed);
        assert!(
            probe_latency_ms().is_none(),
            "a failed probe must be unknown"
        );

        // Cleanup: restore the global to its not-yet-run state.
        PROBE_FAILED.store(0, Ordering::Relaxed);
        {
            let mut st = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
            st.last_ms = None;
            st.in_flight_since = None;
        }
        // A successful pass publishes a reading again.
        PROBE_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_ms = Some(11);
        PROBE_FAILED.store(0, Ordering::Relaxed);
        assert!(probe_latency_ms().is_some());
        // Cleanup.
        PROBE_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_ms = None;
    }
}
