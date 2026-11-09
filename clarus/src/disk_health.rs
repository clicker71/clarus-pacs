// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Disk health probe - dedicated OS thread measures write latency.
//!
//! Every 5 seconds, writes a 4 KB probe file to `storage_dir`,
//! measures wall-clock latency, and stores it in an [`AtomicU64`].
//! This is a separate OS thread (NOT tokio) to avoid being blocked
//! by the async runtime under I/O pressure.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Latest disk probe latency in milliseconds.
/// 0 = probe not yet run; >30000 = disk severely degraded.
static DISK_PROBE_LATENCY_MS: AtomicU64 = AtomicU64::new(0);

/// 1 when the last probe failed to write its file. Starts at 0: the probe has
/// not run yet, and reporting a not-yet-measured disk as failed would make
/// `/health` answer 503 in the first moments of every start, turning a restart
/// window into a disk alert.
static PROBE_FAILED: AtomicU64 = AtomicU64::new(0);

/// 1 once at least one probe pass has completed, success or failure.
static PROBE_RAN: AtomicU64 = AtomicU64::new(0);

/// Latest probe latency in milliseconds, or `None` while no probe has
/// completed or the last one failed.
///
/// A failed probe measures how long the FAILURE took, which is not disk
/// latency: a write rejected instantly by a full or read-only filesystem
/// reports about 0 ms, and that is the most alarming state there is. Publishing
/// it as a healthy low latency is worse than publishing nothing, because the
/// alerting rule it feeds fires on high values only - so a dying volume would
/// look like the fastest disk in the fleet. The probe clears the reading on
/// failure and the gauge is then omitted, which is what "unknown" means for a
/// gauge.
pub fn probe_latency_ms() -> Option<u64> {
    if PROBE_FAILED.load(Ordering::Relaxed) != 0 {
        return None;
    }
    Some(DISK_PROBE_LATENCY_MS.load(Ordering::Relaxed))
}

/// Whether any probe pass has completed yet.
pub fn probe_has_run() -> bool {
    PROBE_RAN.load(Ordering::Relaxed) != 0
}

/// Return the latest disk probe latency in milliseconds.
/// 0 means the probe hasn't completed yet.
pub fn disk_latency_ms() -> u64 {
    DISK_PROBE_LATENCY_MS.load(Ordering::Relaxed)
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
            loop {
                let tmp = storage_dir.join(".healthcheck_probe");
                let data = [0u8; 4096];
                let t0 = Instant::now();
                // A write that FAILS measures the failure, not the disk: the
                // elapsed time of a rejection is not latency. Keep the last
                // good reading and mark the gauge unknown instead.
                let wrote = std::fs::write(&tmp, data).is_ok();
                let removed = if wrote {
                    std::fs::remove_file(&tmp).is_ok()
                } else {
                    false
                };
                if wrote && removed {
                    let ms = t0.elapsed().as_millis() as u64;
                    DISK_PROBE_LATENCY_MS.store(ms, Ordering::Relaxed);
                    PROBE_FAILED.store(0, Ordering::Relaxed);
                } else {
                    PROBE_FAILED.store(1, Ordering::Relaxed);
                }
                PROBE_RAN.store(1, Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        })
        .expect("Failed to spawn disk probe thread");
}
