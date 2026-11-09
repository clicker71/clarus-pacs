// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Application state shared across all synchronous connection workers.
//!
//! Plain `Arc` handles: the storage tier, the index writer (shadow), the
//! study scanner, codec registry, transcode pool and the metrics rings.
//! There is no SQLite, no WAL, no r2d2 pool here - the SQLite tier was
//! excised in and this struct holds none of those pieces. Concurrency
//! is `std::sync` (mutexes/rwlocks around the bounded rings); the hot paths
//! (QIDO/WADO/STOW) take no lock from this struct.

#![forbid(unsafe_code)]

use crate::config::Config;
use crate::shadow::ShadowTier;
use crate::sse_fanout::DualEventSink;
use clarus_adapters::codec::CodecRegistry;
use clarus_adapters::storage::ContentAddressedStorage;
#[cfg(feature = "s3")]
use clarus_adapters::storage::s3_client::S3Client;
use clarus_core::query::StudyScanner;
use std::collections::VecDeque;

use std::sync::Arc;
// Keep std RwLock for ring buffers that are accessed from sync contexts (logging, metrics collection).
use std::sync::RwLock as StdRwLock;

// -- Metric history ring buffer (P24b) ------------------------------------

/// A single metric data point for the history endpoint.
#[derive(Debug, Clone)]
pub struct MetricPoint {
    pub timestamp: String,
    pub memory_mb: f64,
    pub disk_used_mb: f64,
}

/// Ring buffer of historical metric points.
/// Wrapped in Arc<StdRwLock<...>> for concurrent access from the collection task
/// and the HTTP handler.
pub type MetricsRing = Arc<StdRwLock<VecDeque<MetricPoint>>>;

// -- Log ring buffer for SSE streaming (P24b) -----------------------------

/// A single log entry for the SSE log stream.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub component: String,
    pub message: String,
    pub tag: Option<String>,
}

/// Ring buffer of recent log entries.
pub type LogBuffer = Arc<StdRwLock<VecDeque<LogEntry>>>;

/// Maximum number of log entries in the ring buffer.
pub const LOG_BUFFER_CAPACITY: usize = 1_000;

/// Maximum number of metric points in the ring buffer (24h at 60s intervals).
pub const METRICS_BUFFER_CAPACITY: usize = 1440;

// -- Maintenance state (P24b) ---------------------------------------------

/// Runtime maintenance mode state.
#[derive(Debug, Clone, Default)]
pub struct MaintenanceState {
    pub enabled: bool,
    pub message: String,
    pub eta_minutes: Option<u64>,
    pub enabled_at: Option<String>,
    pub enabled_by: String,
}

// -- Bounded transcode/render pool (2608) -----------------------------

/// Global bounded permit pool shared by transcode AND render.
///
/// A bare `AtomicUsize` in-flight counter with a compare_exchange loop (CAS) -
/// deliberately NO new dependencies (the repo is on a diet; `std::sync::
/// Semaphore` does not exist on rustc 1.97.0 and a `Condvar` queue is not
/// wanted because there IS no queue - only `try_acquire` -> 503). This bounds
/// both CPU (max concurrent decodes/renders) and memory (max x decoded frame).
#[derive(Debug, Clone)]
pub struct TranscodePool {
    /// Maximum concurrent transcode/render permits (N).
    max: std::sync::Arc<u32>,
    /// Current number of in-flight permits (0..=max).
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl TranscodePool {
    /// Create a pool with the given maximum concurrent permits.
    pub fn new(max: u32) -> Self {
        use std::sync::atomic::AtomicUsize;
        Self {
            max: std::sync::Arc::new(max.max(1)),
            in_flight: std::sync::Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Effective configured maximum (for `-t` / observability).
    pub fn max(&self) -> u32 {
        *self.max
    }

    /// Try to acquire a permit WITHOUT waiting. On success increments the
    /// in-flight count (and the high-water `transcode_peak` metric) and
    /// returns true; on a full pool bumps `transcode_denied_503` and returns
    /// false. The CAS loop is lock-free; the caller never blocks.
    pub fn try_acquire(&self) -> bool {
        use std::sync::atomic::Ordering;
        loop {
            let cur = self.in_flight.load(Ordering::Relaxed);
            if cur >= (*self.max) as usize {
                crate::metrics::TRANSCODE_DENIED.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            match self.in_flight.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // High-water mark: at least one permit was held at `cur+1`.
                    crate::metrics::bump_peak_if((cur + 1) as u64);
                    return true;
                }
                Err(actual) => {
                    // Retry with the fresh value; the loop never spins hot
                    // because a contended acquire falls through to a release.
                    let _ = actual;
                }
            }
        }
    }

    /// Release a previously-acquired permit and count one completed transcode
    /// (the `transcode_total` metric). Caller must hold a permit.
    pub fn release(&self) {
        use std::sync::atomic::Ordering;
        crate::metrics::TRANSCODE_TOTAL.fetch_add(1, Ordering::Relaxed);
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

// -- AppState -------------------------------------------------------------

/// Shared application state, cloned per connection worker.
///
/// One `Arc<AppState>` is built at startup and handed to every
/// thread-per-connection worker (serve.rs); there is no framework handler
/// injection layer.
#[derive(Clone)]
pub struct AppState {
    /// Content-addressed file storage for DICOM instances.
    pub cas: ContentAddressedStorage,
    /// DICOM codec registry for transcoding (G-05).
    pub codec_registry: CodecRegistry,
    /// UPS event publisher - broadcasts on the event bus and fans out to the
    /// sync dispatcher via a dual sink. PS3.18 Section 11.8.
    pub ups_events: DualEventSink,
    /// Server start instant for uptime calculation.
    pub start_time: std::time::Instant,
    /// Ring buffer for metric history (P24b).
    pub metrics_ring: MetricsRing,
    /// Ring buffer for log streaming via SSE (P24b).
    pub log_buffer: LogBuffer,
    /// Maintenance mode state (P24b).
    pub maintenance: Arc<StdRwLock<MaintenanceState>>,
    /// S3-compatible object storage backend (cold tier fallback).
    /// Only available when compiled with `--features s3`.
    /// `OnceLock` is set by a background thread - server starts instantly,
    /// S3 connects in background. Read with `.get()` (non-blocking).
    #[cfg(feature = "s3")]
    pub s3: Arc<std::sync::OnceLock<S3Client>>,
    /// File-system cold storage (FR-017b).
    /// Second CAS instance rooted at `cold_path`. None if cold_path not configured.
    pub cold_cas: Option<ContentAddressedStorage>,
    /// B-tree tier: .idx appenders + CBOR journal.
    /// SQLite stays primary; this mirrors accepted STOWs for the proptest
    /// shadow-read comparison.
    pub shadow: ShadowTier,
    /// B-tree query scanner: answers study-level QIDO from the CBOR
    /// manifests in `btree.manifest_dir` via the `.idx` accelerators. This is
    /// the read side of the cut-over - SQLite becomes the shadow tier.
    pub study_scanner: StudyScanner,
    /// Server configuration (read-only after startup).
    pub config: Config,
    /// Root directory for the UPS-RS CBOR workitem store (C-014).
    /// Each workitem lives at `workitems/{uid}.cbor`.
    pub workitems_dir: std::path::PathBuf,
    /// Root directory for the Storage Commitment CBOR result store
    /// (PS3.18 Section 13). Each result lives at
    /// `commitments/{TransactionUID}.cbor`.
    pub commitments_dir: std::path::PathBuf,
    /// Bounded transcode/render permit pool (2608). Shared by `maybe_transcode`
    /// and `render_full_from_bytes`; `try_acquire` -> 503 when saturated.
    pub transcode_pool: TranscodePool,
    /// Per-study Input Readiness debounce (see `handlers/readiness.rs`). A CAD
    /// workitem is offered INCOMPLETE and promoted to READY once its study has
    /// been quiet for the debounce period, so a performer cannot claim a study
    /// that is still being stored.
    pub readiness: crate::handlers::readiness::Readiness,
}

impl AppState {
    /// Push a log entry into the ring buffer (for SSE streaming).
    pub fn push_log(&self, entry: LogEntry) {
        if let Ok(mut buffer) = self.log_buffer.write() {
            if buffer.len() >= LOG_BUFFER_CAPACITY {
                buffer.pop_front();
            }
            buffer.push_back(entry);
        }
    }

    /// Push a metric point into the ring buffer.
    pub fn push_metric(&self, point: MetricPoint) {
        if let Ok(mut buffer) = self.metrics_ring.write() {
            if buffer.len() >= METRICS_BUFFER_CAPACITY {
                buffer.pop_front();
            }
            buffer.push_back(point);
        }
    }

    /// Check if maintenance mode is active.
    pub fn is_maintenance(&self) -> bool {
        self.maintenance.read().map(|m| m.enabled).unwrap_or(false)
    }

    /// Get a clone of the current maintenance state.
    pub fn maintenance_state(&self) -> MaintenanceState {
        self.maintenance
            .read()
            .map(|m| m.clone())
            .unwrap_or_default()
    }
}

// -- Bounded pool unit tests (2608) ---------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    // The counters below are PROCESS-WIDE statics shared with every other test
    // that acquires a permit, and the harness runs test functions on parallel
    // threads. Each test therefore measures its own DELTA and asserts on that;
    // asserting the absolute value made the assertion depend on what else was
    // running at the time, which is precisely the flakiness a parallel runner
    // produces on a green tree.
    #[test]
    fn pool_one_permit_two_calls_first_ok_second_503() {
        let denied_before = crate::metrics::TRANSCODE_DENIED.load(Ordering::Relaxed);
        let pool = TranscodePool::new(1);
        // First acquire succeeds, second is refused (503-path, no wait).
        assert!(pool.try_acquire(), "first permit must be granted");
        assert!(
            !pool.try_acquire(),
            "second acquire with a full pool must be refused (no queue)"
        );
        let denied_after = crate::metrics::TRANSCODE_DENIED.load(Ordering::Relaxed);
        assert_eq!(
            denied_after - denied_before,
            1,
            "a refused acquire bumps the denied counter exactly once"
        );
        // Release -> permit freed -> a new acquire succeeds.
        pool.release();
        assert!(pool.try_acquire(), "permit must be reusable after release");
        pool.release();
    }

    #[test]
    fn pool_permit_released_after_use_no_leak() {
        let pool = TranscodePool::new(2);
        // 4 sequential acquire/release rounds must never leak a permit.
        for round in 0..4 {
            assert!(pool.try_acquire(), "round {round}: acquire");
            pool.release();
        }
        // Both permits free -> two concurrent acquires both succeed.
        assert!(pool.try_acquire());
        assert!(pool.try_acquire());
        assert!(!pool.try_acquire(), "pool of 2 must cap at 2");
        pool.release();
        pool.release();
    }

    #[test]
    fn pool_concurrent_acquire_release_is_lock_free_and_bounded() {
        let pool = std::sync::Arc::new(TranscodePool::new(4));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..500 {
                    if pool.try_acquire() {
                        // Hold briefly, then release.
                        std::hint::black_box(());
                        pool.release();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // After all threads finish, in-flight must be back to 0 (no leak).
        assert_eq!(
            pool.in_flight.load(Ordering::Relaxed),
            0,
            "in-flight must return to 0 after all permits released"
        );
    }
}
