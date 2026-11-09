// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Prometheus-compatible in-memory metrics.
//!
//! ## Design
//! - All counters use [`std::sync::atomic::AtomicU64`] - lock-free, zero heap
//!   allocation in the hot path.
//! - Per-route labelled counters are stored in a fixed-size array indexed by
//!   [`RouteId`][crate::auth::RouteId] x status class (2xx / 4xx / 5xx).
//! - Prometheus text rendering allocates a `String` **only** during scrape
//!   (GET `/metrics`), not under DICOMweb load.
//!
//! ## Invariants
//! - **INVARIANT_2 (PII Protection):** No patient data ever appears in
//!   metrics or logs. Only UIDs, HTTP statuses, and system errors.
//! - **INVARIANT_3 (Telemetry):** `/metrics` protected by the same Basic Auth
//!   middleware. All counters are in-memory, no dynamic allocations under load.

#![forbid(unsafe_code)]

use crate::route_classify::{NUM_ROUTES, RouteId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Status classification
// ---------------------------------------------------------------------------

// Class index constants live on the caller side of the u16 table; the
// classification itself is the canonical `clarus_core::http_status::status_class`
// (the `http` crate is not used; status codes come from the core u16 table).
pub(crate) const STATUS_2XX: usize = clarus_core::http_status::STATUS_2XX;
pub(crate) const STATUS_4XX: usize = clarus_core::http_status::STATUS_4XX;
pub(crate) const STATUS_5XX: usize = clarus_core::http_status::STATUS_5XX;
pub(crate) const NUM_STATUS_CLASSES: usize = clarus_core::http_status::NUM_STATUS_CLASSES;

#[inline]
fn route_counter_index(route: RouteId, sc: usize) -> usize {
    (route as usize) * NUM_STATUS_CLASSES + sc
}

// ---------------------------------------------------------------------------
// Atomic metric storage - zero allocation in hot path
// ---------------------------------------------------------------------------

// Four HOT-path counters, each 64B-aligned so the N worker threads updating
// them concurrently do not false-share one line (they would otherwise pack
// into a single 64B line, invalidating it on every RMW from every worker).
// `AlignedU64` derefs to `AtomicU64`, so call sites keep `.load`/`.fetch_add`
// unchanged. Cold gauges (storage/db-pool, updated every 30-300s) stay
// unaligned - no contention there.
#[repr(C, align(64))]
pub(crate) struct AlignedU64(AtomicU64);

impl core::ops::Deref for AlignedU64 {
    type Target = AtomicU64;
    fn deref(&self) -> &AtomicU64 {
        &self.0
    }
}

pub(crate) static HTTP_REQUESTS_TOTAL: AlignedU64 = AlignedU64(AtomicU64::new(0));
pub(crate) static HTTP_REQUESTS_ACTIVE: AlignedU64 = AlignedU64(AtomicU64::new(0));
pub(crate) static HTTP_DURATION_SUM_US: AlignedU64 = AlignedU64(AtomicU64::new(0));
pub(crate) static HTTP_DURATION_COUNT: AlignedU64 = AlignedU64(AtomicU64::new(0));

// -- 2608: bounded transcode/render pool metrics ---------------------
// Counters only (NOT instantaneous gauges): a pool event's duration is < 1 s,
// so any refresh-interval gauge sample is useless. `transcode_total` counts
// completed transcodes/renders since start; `transcode_denied_503` counts how
// many times the pool answered 503 (the saturation signal); `transcode_peak`
// is the high-water of simultaneously-held permits ("was the pool ever full?").
// In a default (non-transcode) build these stay 0 and honestly report 0.
pub(crate) static TRANSCODE_TOTAL: AlignedU64 = AlignedU64(AtomicU64::new(0));
pub(crate) static TRANSCODE_DENIED: AlignedU64 = AlignedU64(AtomicU64::new(0));
pub(crate) static TRANSCODE_PEAK: AlignedU64 = AlignedU64(AtomicU64::new(0));

// -- 2808: access-audit journal metrics --------------------------------
// Counters, not gauges (same policy as the pool metrics: a per-request
// event is < 1 s, an instantaneous gauge sample is useless). The
// anonymous/identified ratio is derived in Prometheus.
pub(crate) static ACCESS_AUDIT_DROPPED: AlignedU64 = AlignedU64(AtomicU64::new(0));
pub(crate) static ACCESS_ANONYMOUS_TOTAL: AlignedU64 = AlignedU64(AtomicU64::new(0));
pub(crate) static ACCESS_IDENTIFIED_TOTAL: AlignedU64 = AlignedU64(AtomicU64::new(0));
/// Shadow journal events the ring had to evict to make room.
///
/// The eviction itself is the intended fail-open policy (a full journal must
/// never block a STOW), but a silent eviction makes a journal with a hole look
/// complete. Counter, not gauge: it is a running total of losses.
pub(crate) static JOURNAL_EVICTED: AlignedU64 = AlignedU64(AtomicU64::new(0));

/// Monotonic high-water: raise `TRANSCODE_PEAK` to max(current, `candidate`).
/// Lock-free CAS; fails harmlessly under contention (the next acquire retries
/// with a fresh candidate, so the true peak is never missed across retries).
pub(crate) fn bump_peak_if(candidate: u64) {
    loop {
        let cur = TRANSCODE_PEAK.load(Ordering::Relaxed);
        if candidate <= cur {
            return;
        }
        if TRANSCODE_PEAK
            .compare_exchange_weak(cur, candidate, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}
// Metrics: histogram of HTTP request duration in seconds (Prometheus Summary)
//
// Stores up to 1000 recent measurements. Quantiles computed only during
// scrape (GET /metrics), not in hot path.
pub struct Summary {
    samples: std::sync::Mutex<Vec<f64>>,
    /// Total duration (scrape path) - `pub(crate)` for `metrics_render`.
    pub(crate) sum: std::sync::Mutex<f64>,
    /// Sample count (scrape path) - `pub(crate)` for `metrics_render`.
    pub(crate) count: std::sync::atomic::AtomicU64,
    max_samples: usize,
}

impl Summary {
    /// Metrics: create new histogram with sample limit.
    pub const fn new(max_samples: usize) -> Self {
        Self {
            samples: std::sync::Mutex::new(Vec::new()),
            sum: std::sync::Mutex::new(0.0),
            count: std::sync::atomic::AtomicU64::new(0),
            max_samples,
        }
    }

    /// Metrics: record request duration in seconds.
    ///
    /// Called after each request completes. Blocks Mutex briefly -
    /// acceptable for alpha load (< 100 req/s).
    /// P26 (deferred - v0.4): Replace Mutex<Vec<f64>> with lock-free ring
    /// buffer or hdr-histogram crate for production loads >100 req/s.
    pub fn observe(&self, duration_secs: f64) {
        // Metrics: update sum and count
        if let Ok(mut sum) = self.sum.lock() {
            *sum += duration_secs;
        }
        self.count.fetch_add(1, Ordering::Relaxed);

        // Metrics: store sample in circular buffer (max 1000)
        if let Ok(mut samples) = self.samples.lock() {
            if samples.len() >= self.max_samples {
                // Metrics: buffer full - remove oldest (FIFO)
                samples.remove(0);
            }
            samples.push(duration_secs);
        }
    }

    /// Metrics: compute quantile from stored samples.
    ///
    /// Returns None if no samples yet. O(n log n) - only at scrape time.
    /// `pub(crate)` for `metrics_render` (scrape path).
    pub(crate) fn quantile(&self, q: f64) -> Option<f64> {
        let samples = self.samples.lock().ok()?;
        if samples.is_empty() {
            return None;
        }
        let mut sorted = samples.clone();
        sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
        Some(sorted[idx])
    }
}

// Metrics: global request duration histogram
pub(crate) static REQUEST_DURATION: Summary = Summary::new(1000);

// Metrics: storage directory gauges - updated every 60s by background task
pub(crate) static STORAGE_OBJECTS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub(crate) static STORAGE_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
// 3008 viewer split of the OBJ gauge: `instances` = logical instance records
// across live manifests (the honest "how many instances" number);
// `unreferenced` = CAS files referenced by NO live manifest = replaced-
// version orphans + hidden-PHI alibi copies + crash-window leaks - extra
// physical copies beyond the logical set.
pub(crate) static STORAGE_INSTANCES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub(crate) static STORAGE_UNREFERENCED_TOTAL: AtomicU64 = AtomicU64::new(0);
// 3008 orphan GC: orphan-sidecar lines not yet reclaimed. Fed by one Relaxed
// fetch_add per orphan-sidecar append on the STOW update path; corrected by
// the orphan-GC pass (0 after a clean pass).
pub static ORPHANS_PENDING: AtomicU64 = AtomicU64::new(0);

// -- 0210: storage and CAS throughput counters -------------------------
// Monotonic byte counters, NOT gauges: Prometheus `rate()` derives MB/s at
// scrape time, so no in-process rate math and no window bookkeeping exists
// here (Rob Pike: no speed hack until the bottleneck is measured).
//
// STORAGE_READ_BYTES counts bytes handed to a WADO body writer;
// STORAGE_WRITE_BYTES counts bytes the cold-migration copy pushed to the cold
// CAS. CAS_WRITE_BYTES counts bytes persisted into the hot CAS by STOW.
pub static STORAGE_READ_BYTES: AtomicU64 = AtomicU64::new(0);
pub static STORAGE_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static CAS_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);

// -- 0810: CAS integrity audit outcome (gauges) ------------------------
// Set by the CAS audit walker (`cas_audit_worker`) when a run completes, and
// reset to 0 at the start of each run. 0 means "no corruption found by the
// last completed audit" - NOT "no corruption exists"; before the FIRST audit
// the gauges are 0 and the viewer must render UNKNOWN, never red. The
// distinction lives in the audit STATE (`status`), which the viewer reads via
// `/health/status`; these gauges are for Prometheus alerting on a bit-rot
// event (a step from 0 to >0 after a completed run).
pub static CAS_BLOBS_CORRUPTED: AtomicU64 = AtomicU64::new(0);
pub static CAS_BLOBS_UNREADABLE: AtomicU64 = AtomicU64::new(0);

// -- 0210: commitment-request verdicts ---------------------------------
// [2xx, 4xx, 5xx] for POST (the request) and GET (the verdict poll). Indexed
// with `status_class`, so a new status class does not need a new counter.
pub(crate) static COMMITMENT_POST: [AtomicU64; NUM_STATUS_CLASSES] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
pub(crate) static COMMITMENT_GET: [AtomicU64; NUM_STATUS_CLASSES] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// Record one commitment-request answer (`is_post` selects the POST/GET pair).
#[inline]
pub fn record_commitment(is_post: bool, status: u16) {
    let sc = clarus_core::http_status::status_class(status);
    let slot = if is_post {
        &COMMITMENT_POST
    } else {
        &COMMITMENT_GET
    };
    if let Some(counter) = slot.get(sc) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

// -- 0210: per-route latency -------------------------------------------
// ROUTE_DURATION_SUM_US/COUNT are indexed by RouteId alone (not by status
// class: latency of a 4xx is still latency). They give Prometheus a
// `rate(sum)/rate(count)` = mean seconds per route, which the global
// HTTP_DURATION cannot express.
pub(crate) static ROUTE_DURATION_SUM_US: [AlignedU64; NUM_ROUTES] =
    [const { AlignedU64(AtomicU64::new(0)) }; NUM_ROUTES];
pub(crate) static ROUTE_DURATION_COUNT: [AlignedU64; NUM_ROUTES] =
    [const { AlignedU64(AtomicU64::new(0)) }; NUM_ROUTES];

/// Record one request's duration against its route.
#[inline]
fn record_route_duration(route: RouteId, elapsed_us: u64) {
    ROUTE_DURATION_SUM_US[route as usize].fetch_add(elapsed_us, Ordering::Relaxed);
    ROUTE_DURATION_COUNT[route as usize].fetch_add(1, Ordering::Relaxed);
}

// Metrics: DICOM STOW parse error counter (MET-005)
pub(crate) static DICOM_STOW_PARSE_ERRORS: AtomicU64 = AtomicU64::new(0);

/// Metrics: record a failed DICOM parse attempt during STOW.
#[inline]
pub fn record_dicom_parse_error() {
    DICOM_STOW_PARSE_ERRORS.fetch_add(1, Ordering::Relaxed);
}

// Metrics: STOW exact-duplicate (idempotent re-STOW) counter.
// STOW statuses: observability
// of how many of the accepted instances were exact duplicates.
pub(crate) static STOW_DUPLICATES_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Metrics: record one exact-duplicate (idempotent) STOW instance.
#[inline]
pub fn record_stow_duplicate() {
    STOW_DUPLICATES_TOTAL.fetch_add(1, Ordering::Relaxed);
}

// -- IOCM rejection rate ----------------------------------------------------
// Two counters, because a rate is a ratio and the ratio belongs in PromQL
// (`rate(rejected_instances_total) / rate(instances_stored_total)`). Computing
// it here would bake in a window the operator cannot change and would lose
// the raw signal the two halves carry independently.
//
// Scope is generic IOCM, not AIRA: the denominator is EVERY accepted instance,
// not the AI-result subset. Restricting the denominator to AI-result classes
// would over-count human reports (Comprehensive SR 88.33 is also a report)
// and so report a rejection rate against a population that does not exist.
pub(crate) static REJECTED_INSTANCES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub(crate) static INSTANCES_STORED_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Per-reason split of [`REJECTED_INSTANCES_TOTAL`].
///
/// A `reason` label on the rejection counter, not a second rate: an operator
/// reading a rise has to know whether it came from Quality rejections (which
/// the QA surface can review) or from an IOCM hide (which nothing can undo).
/// The labels are the four standard codes; an unlisted code still lands in
/// the unlabelled total, so the two never disagree downward.
pub(crate) static REJECTED_BY_REASON: [(u32, &str, AtomicU64); 4] = [
    (113_001, "113001", AtomicU64::new(0)),
    (113_037, "113037", AtomicU64::new(0)),
    (113_038, "113038", AtomicU64::new(0)),
    (113_039, "113039", AtomicU64::new(0)),
];

/// Metrics: IOCM AnyClass rejection notes whose hiding power was ignored
/// because the deployment could not vouch for the note's origin.
///
/// Not folded into the rejection counter: nothing was rejected, so adding it
/// there would report hides that did not happen. This counter is the signal
/// that a deployment is RECEIVING notes its trust configuration declines to
/// act on - either a misconfigured `require_proxy_identity` or an unauthorised
/// client attempting to hide images.
pub(crate) static IOCM_ANYCLASS_IGNORED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Metrics: record instances NEWLY hidden by an accepted rejection note.
///
/// `count` is the number `mark_rejected` actually flipped, so a re-rejected or
/// already-hidden instance contributes nothing - the counter tracks state
/// changes, not references read out of a note. Those differ: one note may name
/// an instance twice, or name one that was hidden last week.
#[inline]
pub fn record_rejected_instances(reason: u32, count: u64) {
    if count == 0 {
        return;
    }
    REJECTED_INSTANCES_TOTAL.fetch_add(count, Ordering::Relaxed);
    if let Some((_, _, counter)) = REJECTED_BY_REASON
        .iter()
        .find(|(code, _, _)| *code == reason)
    {
        counter.fetch_add(count, Ordering::Relaxed);
    }
}

/// Per-reason count of instances whose sequester was reversed by the admin
/// restore endpoint.
///
/// A SEPARATE counter from the rejection one, and both stay monotonic. The
/// population that is hidden right now is the difference, expressed in PromQL
/// as `clarus_rejected_by_reason_total - clarus_restored_total`; decrementing
/// the rejection counter instead would make it an in-memory gauge, lose the
/// fact that the instance WAS once hidden, and make the two series
/// impossible to reconcile after a restart.
pub(crate) static RESTORED_BY_REASON: [(u32, &str, AtomicU64); 4] = [
    (113_001, "113001", AtomicU64::new(0)),
    (113_037, "113037", AtomicU64::new(0)),
    (113_038, "113038", AtomicU64::new(0)),
    (113_039, "113039", AtomicU64::new(0)),
];

/// Metrics: record instances whose rejection was cleared by an admin restore.
///
/// `count` is the number this operation actually flipped, so restoring an
/// instance that is already visible adds nothing - the same "state changes,
/// not references in the request" rule the rejection counter follows.
#[inline]
pub fn record_restored_instances(reason: u32, count: u64) {
    if count == 0 {
        return;
    }
    if let Some((_, _, counter)) = RESTORED_BY_REASON
        .iter()
        .find(|(code, _, _)| *code == reason)
    {
        counter.fetch_add(count, Ordering::Relaxed);
    }
}

/// Metrics: record AnyClass rejection references refused because the
/// deployment could not vouch for the note's origin.
#[inline]
pub fn record_iocm_anyclass_ignored(count: u64) {
    if count > 0 {
        IOCM_ANYCLASS_IGNORED_TOTAL.fetch_add(count, Ordering::Relaxed);
    }
}

/// Metrics: record instances accepted by STOW, EXCLUDING exact duplicates.
///
/// Exact duplicates are excluded because their arrival does not add to the
/// stored population, and including them would let a client lower the measured
/// rejection rate by re-sending the same instances.
#[inline]
pub fn record_instances_stored(count: u64) {
    if count > 0 {
        INSTANCES_STORED_TOTAL.fetch_add(count, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod iocm_rate_tests {
    use super::{
        INSTANCES_STORED_TOTAL, IOCM_ANYCLASS_IGNORED_TOTAL, REJECTED_BY_REASON,
        REJECTED_INSTANCES_TOTAL, record_instances_stored, record_iocm_anyclass_ignored,
        record_rejected_instances,
    };
    use std::sync::atomic::Ordering;

    // These measure the increment the helper performs, not a value delta across
    // an HTTP request: the counters are process-global and the STOW tests run
    // concurrently, so only the helper's own contract is observable here.

    #[test]
    fn the_rejection_counter_adds_the_flip_count() {
        let before = REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed);
        record_rejected_instances(113_001, 3);
        assert_eq!(REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed) - before, 3);
    }

    #[test]
    fn a_zero_flip_count_is_not_recorded() {
        // A repeated rejection flips nothing; counting it would inflate the
        // rate with references that changed no state.
        let before = REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed);
        record_rejected_instances(113_001, 0);
        assert_eq!(REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed), before);
    }

    #[test]
    fn the_rejection_counter_splits_by_reason() {
        // Both halves move together: the labelled series is the breakdown of
        // the total, so they must never disagree.
        let total_before = REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed);
        let b038 = REJECTED_BY_REASON
            .iter()
            .find(|(code, _, _)| *code == 113_038)
            .expect("113038 is a standard reason");
        let before_038 = b038.2.load(Ordering::Relaxed);

        record_rejected_instances(113_038, 2);

        assert_eq!(
            REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed) - total_before,
            2
        );
        assert_eq!(b038.2.load(Ordering::Relaxed) - before_038, 2);
    }

    #[test]
    fn an_unlisted_reason_still_moves_the_total() {
        // The total is the primary series; an unknown code must not vanish
        // from it just because it has no label yet.
        let before = REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed);
        record_rejected_instances(123_456, 1);
        assert_eq!(REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed) - before, 1);
    }

    #[test]
    fn the_ignored_counter_adds_only_positive_counts() {
        let before = IOCM_ANYCLASS_IGNORED_TOTAL.load(Ordering::Relaxed);
        record_iocm_anyclass_ignored(0);
        assert_eq!(IOCM_ANYCLASS_IGNORED_TOTAL.load(Ordering::Relaxed), before);
        record_iocm_anyclass_ignored(4);
        assert_eq!(
            IOCM_ANYCLASS_IGNORED_TOTAL.load(Ordering::Relaxed) - before,
            4
        );
    }

    #[test]
    fn the_stored_counter_adds_the_new_instance_count() {
        let before = INSTANCES_STORED_TOTAL.load(Ordering::Relaxed);
        record_instances_stored(5);
        assert_eq!(INSTANCES_STORED_TOTAL.load(Ordering::Relaxed) - before, 5);
    }

    #[test]
    fn a_zero_stored_count_is_not_recorded() {
        // An all-duplicate STOW contributes 0 and must leave the denominator
        // alone.
        let before = INSTANCES_STORED_TOTAL.load(Ordering::Relaxed);
        record_instances_stored(0);
        assert_eq!(INSTANCES_STORED_TOTAL.load(Ordering::Relaxed), before);
    }
}

// Metrics (Task B 22.08): monotonic counter of log lines dropped because the
// journal channel was full (never-block policy). Incremented by `clog`'s
// `record_drop()`; `Disconnected` (process shutdown) is intentionally NOT
// counted. Monotonic, no reset.
pub(crate) static CLARUS_LOG_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Metrics: record one dropped log line (journal channel was full).
#[inline]
pub fn record_log_drop() {
    CLARUS_LOG_DROPPED.fetch_add(1, Ordering::Relaxed);
}

// Metrics: Block-Level CAS chunk counters (MET-006)
pub(crate) static CLARUS_CHUNKS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub(crate) static CLARUS_CHUNKS_UNIQUE: AtomicU64 = AtomicU64::new(0);

// -- I/O resilience metrics ----------------------------------------------

/// Disk probe latency in milliseconds (updated by disk_health OS thread).
/// Read-only in metrics - the source of truth is disk_health::disk_latency_ms().
pub static DISK_PROBE_LATENCY_MS: AtomicU64 = AtomicU64::new(0);

/// Process RSS in bytes - updated by background metrics collection loop.
pub static PROCESS_RSS_BYTES: AtomicU64 = AtomicU64::new(0);

/// Free space on the hot storage partition, in percent (0-100). Updated by the
/// `disk-free-probe` background thread (every ~20s). The `/health/status`
/// endpoint color-codes the DISK indicator from this + [`DISK_FREE_GB`] against
/// the `[storage_tiers] emergency_free_gb` threshold (same threshold the
/// emergency migration loop uses). 0 = probe not yet run or stat failed.
pub static DISK_FREE_PCT: AtomicU64 = AtomicU64::new(0);

/// Free space on the hot storage partition, in gigabytes (floored). Pairs with
/// [`DISK_FREE_PCT`] so `/health/status` can apply the `emergency_free_gb`
/// threshold (a GB threshold) to color the DISK indicator.
pub static DISK_FREE_GB: AtomicU64 = AtomicU64::new(0);

// -- B-tree index staleness ----------------------------------------------

/// Number of CAS objects not yet reflected in the B-tree `.idx` after lazy
/// recovery rebuilds from `manifests/`. `0` means the B-tree tier is fully
/// consistent with the store - the server is ready to serve QIDO as primary.
/// Updated by the recovery task as it walks CAS and rebuilds `.idx`.
pub static CLARUS_IDX_STALE_OBJECTS: AtomicU64 = AtomicU64::new(0);

// -- Runtime idx heal (lazy, on exact lookup) -----------------------------

/// LIVE staleness the runtime fixer is working through: detections of an exact
/// study-UID lookup finding a study in `manifests/` but missing from the
/// modality-idx, MINUS heals the background fixer has completed. Converges to
/// `0` in runtime as the fixer drains: `0` = clean, `>0` = detected and being
/// repaired. This is DISTINCT from [`CLARUS_IDX_STALE_OBJECTS`] (a one-time
/// startup-pass counter) - do not merge the two populations.
pub static CLARUS_IDX_RUNTIME_STALE: AtomicU64 = AtomicU64::new(0);

/// Monotonic total of runtime staleness detections (for Prometheus aggregate).
pub static CLARUS_IDX_DETECTED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Monotonic total of runtime heals completed (a missing modality row appended).
pub static CLARUS_IDX_HEALED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Record one runtime staleness detection (exact lookup found study out of
/// sync). Feeds the live gauge and the monotonic total.
#[inline]
pub fn record_idx_stale_detect() {
    CLARUS_IDX_RUNTIME_STALE.fetch_add(1, Ordering::Relaxed);
    CLARUS_IDX_DETECTED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Record one runtime heal completion (a missing modality row appended).
#[inline]
pub fn record_idx_stale_heal() {
    CLARUS_IDX_RUNTIME_STALE.fetch_sub(1, Ordering::Relaxed);
    CLARUS_IDX_HEALED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// The live runtime-stale gauge value (detected minus healed, saturating).
#[inline]
pub fn idx_runtime_stale() -> u64 {
    CLARUS_IDX_RUNTIME_STALE.load(Ordering::Relaxed)
}

// -- manifest degradation statics ----------------------------------

/// Manifest-path is missing (no `*.manifest` files found when CAS is non-empty):
/// the cas->manifests rebuild gap detected at startup. `1` = degraded. Set by
/// the lazy-recovery startup sanity check (main.rs).
pub static MANIFEST_MISSING: AtomicU64 = AtomicU64::new(0);

/// Number of manifest files that could not be decoded. Set from the
/// `ReconcileReport.corrupt` of the lazy-recovery pass (increases a monotonic
/// counter of corrupt manifests seen). `0` = no corruption seen.
pub static MANIFEST_CORRUPT: AtomicU64 = AtomicU64::new(0);

/// Drives the /health degraded signal: `1` when the store is in a
/// manifest-degraded state (CAS non-empty but manifests missing/empty, or
/// corrupt manifests observed). `0` = manifests healthy.
pub static MANIFEST_DEGRADED: AtomicU64 = AtomicU64::new(0);

/// CAD workitem creation failures after a durable STOW (UPS origin-server
/// capability). Monotonic counter; increments once per study whose workitem
/// could not be created. A duplicate (the workitem already exists) is NOT a
/// failure and never increments this. The STOW itself is never affected.
pub static CAD_WORKITEM_FAILURES_TOTAL: AtomicU64 = AtomicU64::new(0);

// -- worklist subscription push (PS3.18 11.10/11.11) ---------------

/// Workitem notifications delivered to a subscriber (2xx from the push POST).
pub static UPS_PUSHED: AtomicU64 = AtomicU64::new(0);

/// Notifications abandoned: attempts exhausted, queue overflow, or a dead
/// worker. The subscriber's catch-up search is expected to find the workitem.
pub static UPS_PUSH_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Retry attempts made after a failed push (one increment per retry, not per
/// first attempt) - the cost the `[ups] push_attempts` dial buys.
pub static UPS_PUSH_RETRIES: AtomicU64 = AtomicU64::new(0);

/// Final-state workitems removed by the retention sweep. Monotonic counter;
/// one increment per deleted workitem. `SCHEDULED` and `IN PROGRESS` are
/// never swept, so they can never be counted here.
pub static UPS_RETENTION_SWEPT: AtomicU64 = AtomicU64::new(0);

// -- WebSocket notification channel (PS3.18 8.10.4) ---------------

/// Events dropped because one channel's bounded queue was full.
///
/// Notifications are best-effort by design (the AIW-I Reliability statement),
/// so a non-zero value is not an incident - it means a subscriber stopped
/// reading while its workitem kept changing state. The durable path is UPS
/// Search, which the subscriber uses to catch up.
pub static WS_CHANNEL_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Notification connections opened since start.
pub static WS_CHANNELS_OPENED: AtomicU64 = AtomicU64::new(0);

/// Event Reports successfully queued onto a channel (RAD-87).
pub static WS_EVENTS_DELIVERED: AtomicU64 = AtomicU64::new(0);

// -- lazy erase pending tombstones -------------------------------

/// Number of `.del` tombstones awaiting physical erasure by the erase-worker
/// (+1 per tombstone with at least one file still waiting on an open handle).
/// `0` means every deleted study's files have been erased and lifted. Grows
/// while a file is held open on Windows (retry backoff) - informational.
pub static ERASE_PENDING: AtomicU64 = AtomicU64::new(0);

// -- S3 cold storage metrics (FR-017, only with --features s3) ----------

/// S3 reachable: -1 = not configured, 0 = down, 1 = reachable.
#[cfg(feature = "s3")]
pub static S3_REACHABLE: AtomicU64 = AtomicU64::new(0);
// Initialized to -1 (i64::MAX as u64) to signal "not configured".
// Set to 0 or 1 by the first health probe.
// Not using AtomicI64 for simplicity - all metrics are u64.
// -1 is encoded as u64::MAX. Metrics rendering converts to i64.

/// S3 latency in milliseconds (last HEAD probe). -1 = not measured (u64::MAX).
#[cfg(feature = "s3")]
pub static S3_LATENCY_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Number of objects in S3 bucket. -1 = not configured (u64::MAX).
#[cfg(feature = "s3")]
pub static S3_OBJECTS_TOTAL: AtomicU64 = AtomicU64::new(u64::MAX);

/// Total bytes in S3 bucket. -1 = not configured (u64::MAX).
#[cfg(feature = "s3")]
pub static S3_BYTES_TOTAL: AtomicU64 = AtomicU64::new(u64::MAX);

/// Successful S3 migrations (hot -> S3).
#[cfg(feature = "s3")]
pub static S3_MIGRATIONS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Failed S3 migration attempts.
#[cfg(feature = "s3")]
pub static S3_MIGRATION_ERRORS: AtomicU64 = AtomicU64::new(0);

/// Initialize S3 "not configured" sentinel values.
/// Called once at startup. Sets gauges to u64::MAX (-1 equivalent).
#[cfg(feature = "s3")]
pub fn init_s3_not_configured() {
    S3_REACHABLE.store(u64::MAX, Ordering::Relaxed);
    S3_OBJECTS_TOTAL.store(u64::MAX, Ordering::Relaxed);
    S3_BYTES_TOTAL.store(u64::MAX, Ordering::Relaxed);
}

/// Record S3 reachability probe result.
#[cfg(feature = "s3")]
pub fn record_s3_health(reachable: bool, latency_ms: u64) {
    S3_REACHABLE.store(if reachable { 1 } else { 0 }, Ordering::Relaxed);
    S3_LATENCY_MS.store(latency_ms, Ordering::Relaxed);
}

/// Record S3 migration success.
#[cfg(feature = "s3")]
pub fn record_s3_migration() {
    S3_MIGRATIONS_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Record S3 migration error.
#[cfg(feature = "s3")]
pub fn record_s3_migration_error() {
    S3_MIGRATION_ERRORS.fetch_add(1, Ordering::Relaxed);
}

/// Initialize S3 gauges and counters from database on startup.
///
/// Called once after DB connection is established, before any migrations run.
/// Prevents false zeros after process restart.
#[cfg(feature = "s3")]
pub fn init_s3_from_db(object_count: u64, total_bytes: u64, migrated_count: u64) {
    S3_OBJECTS_TOTAL.store(object_count, Ordering::Relaxed);
    S3_BYTES_TOTAL.store(total_bytes, Ordering::Relaxed);
    S3_MIGRATIONS_TOTAL.store(migrated_count, Ordering::Relaxed);
}

/// Metrics: record chunk statistics after a STOW operation.
///
/// `total` - total number of chunk hashes computed for this instance.
/// `new_count` - how many of those chunks were previously unseen (inserted, not updated).
#[inline]
pub fn record_chunk_stats(total: u64, new_count: u64) {
    CLARUS_CHUNKS_TOTAL.fetch_add(total, Ordering::Relaxed);
    CLARUS_CHUNKS_UNIQUE.fetch_add(new_count, Ordering::Relaxed);
}

// -- I/O resilience metric recording functions ----------------------------

/// Update the disk probe latency gauge from the disk_health module.
///
/// A failed probe leaves the gauge untouched rather than writing the elapsed
/// time of the failure: that time is not latency, and it is the LOWEST when
/// the disk is worst (an instant rejection on a full or read-only mount).
#[inline]
pub fn update_disk_probe_latency() {
    if let Some(ms) = crate::disk_health::probe_latency_ms() {
        DISK_PROBE_LATENCY_MS.store(ms, Ordering::Relaxed);
    }
}

/// Update process RSS in bytes.
#[inline]
pub fn update_process_rss_bytes(rss: u64) {
    PROCESS_RSS_BYTES.store(rss, Ordering::Relaxed);
}

/// Update the free-space gauges for the hot storage partition (`storage_dir`).
///
/// Computes free percent and free gigabytes via [`crate::disk_utils`] (statvfs)
/// and stores them in [`DISK_FREE_PCT`] / [`DISK_FREE_GB`]. A failed stat leaves
/// both gauges at their last known value; a SUCCESSFUL stat that reports zero
/// available is published as zero - a full disk is the one reading an operator
/// must never miss, so it cannot be swallowed by the same branch that guards a
/// broken probe. Runs on the `disk-free-probe` background thread every ~20s so
/// the `/health/status` handler never blocks on a syscall.
pub fn update_disk_free_gauges(storage_dir: &std::path::Path) {
    let Some((total_mb, avail_mb)) = crate::disk_utils::disk_space_mb(storage_dir) else {
        return; // stat failed - keep the last known good values
    };
    // Clamp: a negative delta is what some filesystems report at root reserve,
    // and an unsigned conversion would wrap it to an enormous gauge value.
    DISK_FREE_GB.store((avail_mb.max(0.0) / 1024.0) as u64, Ordering::Relaxed);
    if total_mb > 0.0 {
        let pct = ((avail_mb.max(0.0) / total_mb) * 100.0).round() as u64;
        DISK_FREE_PCT.store(pct.min(100), Ordering::Relaxed);
    }
}

/// Metrics: record request duration into histogram (in seconds).
#[inline]
pub fn record_request_duration(duration_secs: f64) {
    REQUEST_DURATION.observe(duration_secs);
}

/// Labeled counter array: [route x status_class].
///
/// Indexed by `route_counter_index(route, status_class)`.
///
/// Built by a const fn from `NUM_ROUTES` rather than written out by hand: the
/// previous hand-written literal had to be edited in lockstep with the
/// [`RouteId`] enum, and an omitted entry is a silent misattribution (every
/// later route shifts by one). The array length is now derived, so adding a
/// route cannot leave it short.
pub(crate) static ROUTE_COUNTERS: [AtomicU64; NUM_ROUTES * NUM_STATUS_CLASSES] =
    build_route_counters();

const fn build_route_counters() -> [AtomicU64; NUM_ROUTES * NUM_STATUS_CLASSES] {
    [const { AtomicU64::new(0) }; NUM_ROUTES * NUM_STATUS_CLASSES]
}

// ---------------------------------------------------------------------------
// Recording functions - called from the auth middleware (hot path)
// ---------------------------------------------------------------------------

/// Record the start of an HTTP request. Increments the in-flight gauge.
#[inline]
pub fn record_request_start() {
    HTTP_REQUESTS_ACTIVE.fetch_add(1, Ordering::Relaxed);
}

/// Record the completion of an HTTP request (successful auth).
///
/// Called after the handler returns a response. Updates total, active,
/// duration counters, and the per-route labeled counter.
///
/// # Panics
/// Never panics - all operations are infallible atomic loads/stores.
#[inline]
pub fn record_request_end(route: RouteId, status: u16, started_at: Instant) {
    HTTP_REQUESTS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    HTTP_REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);

    // Duration in microseconds
    let elapsed_us = started_at.elapsed().as_micros() as u64;
    HTTP_DURATION_SUM_US.fetch_add(elapsed_us, Ordering::Relaxed);
    HTTP_DURATION_COUNT.fetch_add(1, Ordering::Relaxed);

    // Per-request sample for the scrape-time Summary quantiles (p50/p90/p99).
    // Locking cost is documented as acceptable for alpha load (<100 req/s);
    // P26 (deferred) replaces the Vec with a lock-free buffer if that changes.
    record_request_duration(elapsed_us as f64 / 1_000_000.0);

    // Per-route latency (0210): the global counters cannot say WHICH route is
    // slow, which is the only question a per-route `rate()` answers.
    record_route_duration(route, elapsed_us);

    // Per-route counter
    let sc = clarus_core::http_status::status_class(status);
    ROUTE_COUNTERS[route_counter_index(route, sc)].fetch_add(1, Ordering::Relaxed);
}

/// Record a rejected request (401 Unauthorized or similar).
///
/// Updates total and the per-route counter. Does NOT touch active/duration
/// gauges because the request never reached a handler.
#[inline]
pub fn record_request(route: RouteId, status: u16) {
    HTTP_REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);

    let sc = clarus_core::http_status::status_class(status);
    ROUTE_COUNTERS[route_counter_index(route, sc)].fetch_add(1, Ordering::Relaxed);
}

/// Status-class (2xx / 4xx / 5xx) from a raw status code - pure variant for
/// the synchronous transport, which must not reference framework
/// types. Canonical implementation lives in `clarus_core::http_status`.
#[inline]
fn status_class_u16(code: u16) -> usize {
    clarus_core::http_status::status_class(code)
}

/// Record the completion of a request from the synchronous conn loop.
///
/// Mirrors [`record_request_end`] but takes a plain `u16` status so the caller
/// stays free of framework types. Computes the status class locally.
#[inline]
pub fn record_request_end_u16(route: RouteId, status: u16, started_at: Instant) {
    HTTP_REQUESTS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    HTTP_REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);

    let elapsed_us = started_at.elapsed().as_micros() as u64;
    HTTP_DURATION_SUM_US.fetch_add(elapsed_us, Ordering::Relaxed);
    HTTP_DURATION_COUNT.fetch_add(1, Ordering::Relaxed);

    // Same Summary feed as record_request_end: the synchronous conn loop
    // needs its own copy because it stays free of framework types.
    record_request_duration(elapsed_us as f64 / 1_000_000.0);

    record_route_duration(route, elapsed_us);

    let sc = status_class_u16(status);
    ROUTE_COUNTERS[route_counter_index(route, sc)].fetch_add(1, Ordering::Relaxed);
}

/// Record a completed-but-rejected request from the synchronous conn loop.
///
/// Mirrors [`record_request`] with a plain `u16` status; does not touch the
/// active/duration gauges (the request never reached a dispatch handler).
#[inline]
pub fn record_request_u16(route: RouteId, status: u16) {
    HTTP_REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);
    let sc = status_class_u16(status);
    ROUTE_COUNTERS[route_counter_index(route, sc)].fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Storage & DB size gauges (background update, MET-002 + MET-003)
// ---------------------------------------------------------------------------

/// Metrics: scan storage directory and update object count and size gauges.
///
/// Counts all .dcm files recursively. Called by background thread every 300s.
/// Errors (missing dir, permission denied) reset counters to 0.
///
/// Synchronous (std::thread metrics worker). Directory
/// traversal is blocking by nature and runs on its own OS thread.
pub fn update_storage_gauges(storage_dir: &std::path::Path) {
    update_storage_gauges_full(storage_dir, None);
}

/// Storage gauges with manifest awareness (production path): besides the
/// physical `.dcm` walk (OBJ gauge), computes the logical instance count and
/// the unreferenced count (CAS files whose hash is NOT referenced by any live
/// manifest - replaced-version orphans, hidden-PHI alibi copies, crash-window
/// leaks). `manifest_dir: None` leaves `instances`/`unreferenced` untouched
/// (test/diagnostic callers).
pub fn update_storage_gauges_full(
    storage_dir: &std::path::Path,
    manifest_dir: Option<&std::path::Path>,
) {
    let mut count: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut extra: u64 = 0;
    let mut instances: u64 = 0;
    let referenced =
        manifest_dir.and_then(
            |dir| match crate::erase_worker::collect_referenced_counted(dir) {
                Ok((set, n)) => {
                    instances = n;
                    Some(set)
                }
                Err(e) => {
                    crate::clog_warn!("Metrics: cannot scan manifests for {}: {e}", dir.display());
                    None
                }
            },
        );
    match walk_dir(
        storage_dir,
        &mut count,
        &mut total_bytes,
        referenced.as_ref(),
        &mut extra,
    ) {
        Ok(()) => {
            STORAGE_OBJECTS_TOTAL.store(count, Ordering::Relaxed);
            STORAGE_BYTES_TOTAL.store(total_bytes, Ordering::Relaxed);
            if referenced.is_some() {
                STORAGE_INSTANCES_TOTAL.store(instances, Ordering::Relaxed);
                STORAGE_UNREFERENCED_TOTAL.store(extra, Ordering::Relaxed);
            }
            crate::clog_debug!(
                "Metrics: storage scan complete — {} objects, {} bytes, {} instances, {} unreferenced",
                count,
                total_bytes,
                instances,
                extra
            );
        }
        Err(e) => {
            STORAGE_OBJECTS_TOTAL.store(0, Ordering::Relaxed);
            STORAGE_BYTES_TOTAL.store(0, Ordering::Relaxed);
            if referenced.is_some() {
                STORAGE_INSTANCES_TOTAL.store(0, Ordering::Relaxed);
                STORAGE_UNREFERENCED_TOTAL.store(0, Ordering::Relaxed);
            }
            crate::clog_warn!(
                "Metrics: storage scan failed for {}: {e}",
                storage_dir.display()
            );
        }
    }
}

/// Metrics: recursively walk directory counting .dcm files and total size.
/// When `referenced` is given (manifest hashes), files whose stem is not in
/// the set are counted into `extra` (orphans / hidden-PHI alibis / leaks).
fn walk_dir(
    dir: &std::path::Path,
    count: &mut u64,
    bytes: &mut u64,
    referenced: Option<&std::collections::HashSet<String>>,
    extra: &mut u64,
) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Metrics: skip _tmp directory (CAS temporary files)
            if entry.file_name() != "_tmp" {
                walk_dir(&path, count, bytes, referenced, extra)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "dcm") {
            // Metrics: count .dcm files
            *count += 1;
            if let Some(set) = referenced {
                let stem = path.file_stem().and_then(|s| s.to_str());
                if stem.is_some_and(|s| !set.contains(s)) {
                    *extra += 1;
                }
            }
            if let Ok(meta) = entry.metadata() {
                *bytes += meta.len();
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Prometheus text rendering (scrape path - allocation acceptable)
// ---------------------------------------------------------------------------

// The Prometheus text renderer (`render_metrics`, `push_metric`,
// `push_metric_i64`, `RouteInfo`, `ROUTE_INFO`) was extracted into
// `crate::handlers::metrics_render`. The metric statics and
// status-class constants below are `pub(crate)` so that module can read them.

// ---------------------------------------------------------------------------
// HTTP metrics recording - sync variants called from the conn loop.
// There is no middleware layer: the sync conn loop calls
// `record_request_start` / `record_request_end_u16` directly.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route_classify::RouteId;

    #[test]
    fn test_status_class() {
        assert_eq!(clarus_core::http_status::status_class(200), STATUS_2XX);
        assert_eq!(clarus_core::http_status::status_class(404), STATUS_4XX);
        assert_eq!(clarus_core::http_status::status_class(401), STATUS_4XX);
        assert_eq!(clarus_core::http_status::status_class(500), STATUS_5XX);
    }

    #[test]
    fn test_route_counter_index() {
        let idx = route_counter_index(RouteId::Health, STATUS_2XX);
        assert_eq!(idx, 0);
        let idx = route_counter_index(RouteId::StowStudies, STATUS_5XX);
        assert_eq!(idx, 17); // StowStudies = 5, 5 * 3 + 2 = 17
    }

    #[test]
    fn test_metrics_rendering_no_panic() {
        // Render should produce valid output with all expected HELP/TYPE lines
        let output = crate::handlers::metrics_render::render_metrics();
        assert!(output.starts_with("# HELP"));
        assert!(output.contains("clarus_http_requests_total"));
    }

    /// 0810: the CAS audit gauges are always present in the scrape (even at 0),
    /// so a Prometheus alert rule can reference them from the first scrape.
    #[test]
    fn test_cas_audit_gauges_render() {
        let output = crate::handlers::metrics_render::render_metrics();
        assert!(
            output.contains("clarus_cas_blobs_corrupted"),
            "corrupted gauge must be exposed: {output}"
        );
        assert!(
            output.contains("clarus_cas_blobs_unreadable"),
            "unreadable gauge must be exposed: {output}"
        );
        // Both are declared as gauges (not counters) - the audit RESETS them
        // each run, so a counter type would be wrong.
        assert!(output.contains("# TYPE clarus_cas_blobs_corrupted gauge"));
        assert!(output.contains("# TYPE clarus_cas_blobs_unreadable gauge"));
    }

    #[test]
    fn test_record_request_smoke() {
        record_request(RouteId::Metrics, 200);
        let output = crate::handlers::metrics_render::render_metrics();
        assert!(output.contains("clarus_http_requests_total"));
    }

    // --- Metrics: Summary histogram tests (MET-001) ---

    #[test]
    fn test_summary_observe_and_quantile() {
        // Metrics: verify basic histogram operation
        let s = Summary::new(100);
        for i in 0..50 {
            s.observe((i as f64) * 0.01); // 0.00, 0.01, ..., 0.49
        }
        let p50 = s.quantile(0.5).unwrap();
        let p90 = s.quantile(0.9).unwrap();
        let p99 = s.quantile(0.99).unwrap();
        // Median ~0.245
        assert!(p50 > 0.20 && p50 < 0.30);
        // 90th percentile ~0.44
        assert!(p90 > 0.40 && p90 < 0.49);
        // 99th percentile ~0.49
        assert!(p99 > 0.46);
        assert_eq!(s.count.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn test_summary_buffer_limit() {
        // Metrics: verify buffer doesn't grow beyond max_samples
        let s = Summary::new(10);
        for i in 0..20 {
            s.observe(i as f64);
        }
        let samples = s.samples.lock().unwrap();
        assert_eq!(samples.len(), 10);
        // Should contain last 10 samples (10..19)
        assert!(samples[0] >= 10.0);
    }

    #[test]
    fn test_summary_empty_quantile() {
        // Metrics: empty histogram returns None
        let s = Summary::new(100);
        assert!(s.quantile(0.5).is_none());
    }

    // --- Metrics: Storage gauge tests (MET-002) ---

    #[test]
    fn test_storage_gauges() {
        // One test, sequential asserts: STORAGE_OBJECTS_TOTAL/BYTES_TOTAL are
        // shared globals, separate tests would race in parallel runs.
        // Metrics: scan empty directory
        let tmp = tempfile::tempdir().unwrap();
        update_storage_gauges(tmp.path());
        assert_eq!(STORAGE_OBJECTS_TOTAL.load(Ordering::Relaxed), 0);
        assert_eq!(STORAGE_BYTES_TOTAL.load(Ordering::Relaxed), 0);

        // Metrics: count .dcm files, skip non-dcm
        for i in 0..3 {
            let path = tmp.path().join(format!("test_{i}.dcm"));
            std::fs::write(&path, vec![0u8; 100]).unwrap();
        }
        // Non-.dcm file - should NOT be counted
        std::fs::write(tmp.path().join("readme.txt"), b"hello").unwrap();
        update_storage_gauges(tmp.path());
        assert_eq!(STORAGE_OBJECTS_TOTAL.load(Ordering::Relaxed), 3);
        assert_eq!(STORAGE_BYTES_TOTAL.load(Ordering::Relaxed), 300);
    }

    // --- Metrics: Active connections gauge test (MET-004) ---

    #[test]
    fn test_active_connections_gauge() {
        // Metrics: verify active connections gauge increments/decrements correctly
        let before = HTTP_REQUESTS_ACTIVE.load(Ordering::Relaxed);

        // Simulate request start
        record_request_start();
        assert_eq!(HTTP_REQUESTS_ACTIVE.load(Ordering::Relaxed), before + 1);

        // Second concurrent request
        record_request_start();
        assert_eq!(HTTP_REQUESTS_ACTIVE.load(Ordering::Relaxed), before + 2);

        // First request completes
        record_request_end(RouteId::Other, 200, Instant::now());
        assert_eq!(HTTP_REQUESTS_ACTIVE.load(Ordering::Relaxed), before + 1);

        // Second request completes
        record_request_end(RouteId::Other, 200, Instant::now());
        assert_eq!(HTTP_REQUESTS_ACTIVE.load(Ordering::Relaxed), before);
    }

    // --- Metrics: hot counters must not false-share a cache line -----------

    /// Raw byte address of a static, masked to its cache line base (64B).
    fn cache_line<T>(v: &T) -> usize {
        (v as *const T as usize) & !63usize
    }

    #[test]
    fn hot_counters_are_on_distinct_cache_lines() {
        // The four per-request counters each carry #[repr(align(64))]; any pair
        // must land on different 64B lines so concurrent worker updates do not
        // invalidate a shared line (false sharing).
        let total = cache_line(&HTTP_REQUESTS_TOTAL);
        let active = cache_line(&HTTP_REQUESTS_ACTIVE);
        let dur_sum = cache_line(&HTTP_DURATION_SUM_US);
        let dur_count = cache_line(&HTTP_DURATION_COUNT);
        assert_ne!(total, active, "TOTAL and ACTIVE share a cache line");
        assert_ne!(
            dur_sum, dur_count,
            "DURATION_SUM and DURATION_COUNT share a cache line"
        );
        // All four mutually distinct.
        let mut lines = vec![total, active, dur_sum, dur_count];
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(lines.len(), 4, "some hot counters share a cache line");
    }
}
