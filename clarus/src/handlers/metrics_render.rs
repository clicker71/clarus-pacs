// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Prometheus text rendering for the `/metrics` endpoint.
//!
//! The pure, sync renderer split out of `crate::metrics`. It has
//! **zero axum imports** - it reads the metric statics via `crate::metrics` and
//! emits the Prometheus text format. The axum `metrics_handler` (in
//! `crate::metrics`) is the only caller.
//!
//! ## Invariants (inherited from `crate::metrics`)
//! - The scrape path is the ONLY place this allocates a `String` - counters
//!   stay lock-free `AtomicU64` under DICOMweb load.
//! - No patient data is ever serialized (INVARIANT_2).

#![forbid(unsafe_code)]

use crate::route_classify::NUM_ROUTES;
use std::fmt::Write;

/// Route metadata for Prometheus labels.
struct RouteInfo {
    method: &'static str,
    handler: &'static str,
}

const ROUTE_INFO: [RouteInfo; NUM_ROUTES] = [
    RouteInfo {
        method: "GET",
        handler: "health",
    },
    RouteInfo {
        method: "GET",
        handler: "metrics",
    },
    RouteInfo {
        method: "GET",
        handler: "qido_patients",
    },
    RouteInfo {
        method: "GET",
        handler: "qido_studies",
    },
    RouteInfo {
        method: "GET",
        handler: "wado_study",
    },
    RouteInfo {
        method: "POST",
        handler: "stow_studies",
    },
    RouteInfo {
        method: "UNKNOWN",
        handler: "admin",
    },
    RouteInfo {
        method: "UNKNOWN",
        handler: "commitment",
    },
    RouteInfo {
        method: "UNKNOWN",
        handler: "ups",
    },
    RouteInfo {
        method: "UNKNOWN",
        handler: "other",
    },
];

/// Render all Prometheus metrics into a `String`.
///
/// This function allocates - it is only called during GET `/metrics` scrape
/// (typically every 15-60 seconds by Prometheus / Zabbix).
pub fn render_metrics() -> String {
    use crate::metrics::{
        CAD_WORKITEM_FAILURES_TOTAL, CAS_BLOBS_CORRUPTED, CAS_BLOBS_UNREADABLE, CAS_WRITE_BYTES,
        CLARUS_CHUNKS_TOTAL, CLARUS_CHUNKS_UNIQUE, CLARUS_IDX_DETECTED_TOTAL,
        CLARUS_IDX_HEALED_TOTAL, CLARUS_IDX_STALE_OBJECTS, CLARUS_LOG_DROPPED, COMMITMENT_GET,
        COMMITMENT_POST, DICOM_STOW_PARSE_ERRORS, DISK_FREE_GB, DISK_FREE_PCT, ERASE_PENDING,
        HTTP_DURATION_COUNT, HTTP_DURATION_SUM_US, HTTP_REQUESTS_ACTIVE, HTTP_REQUESTS_TOTAL,
        INSTANCES_STORED_TOTAL, IOCM_ANYCLASS_IGNORED_TOTAL, MANIFEST_CORRUPT, MANIFEST_DEGRADED,
        MANIFEST_MISSING, NUM_STATUS_CLASSES, ORPHANS_PENDING, PROCESS_RSS_BYTES,
        REJECTED_BY_REASON, REJECTED_INSTANCES_TOTAL, REQUEST_DURATION, RESTORED_BY_REASON,
        ROUTE_COUNTERS, ROUTE_DURATION_COUNT, ROUTE_DURATION_SUM_US, STATUS_2XX, STATUS_4XX,
        STATUS_5XX, STORAGE_BYTES_TOTAL, STORAGE_INSTANCES_TOTAL, STORAGE_OBJECTS_TOTAL,
        STORAGE_READ_BYTES, STORAGE_UNREFERENCED_TOTAL, STORAGE_WRITE_BYTES, STOW_DUPLICATES_TOTAL,
        UPS_PUSH_DROPPED, UPS_PUSH_RETRIES, UPS_PUSHED,
    };
    use std::sync::atomic::Ordering;

    // Pre-allocate ~2 KiB for the output
    let mut out = String::with_capacity(2048);

    // --- clarus_http_requests_total ---
    out.push_str("# HELP clarus_http_requests_total Total number of HTTP requests.\n");
    out.push_str("# TYPE clarus_http_requests_total counter\n");
    let total = HTTP_REQUESTS_TOTAL.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_http_requests_total", None, total);

    // --- clarus_http_requests_active ---
    out.push_str("# HELP clarus_http_requests_active Currently in-flight HTTP requests.\n");
    out.push_str("# TYPE clarus_http_requests_active gauge\n");
    let active = HTTP_REQUESTS_ACTIVE.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_http_requests_active", None, active);

    // --- clarus_log_dropped (Task B 22.08) ---
    out.push_str("# HELP clarus_log_dropped Log lines dropped because the journal channel was full (never-block policy).\n");
    out.push_str("# TYPE clarus_log_dropped gauge\n");
    let log_dropped = CLARUS_LOG_DROPPED.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_log_dropped", None, log_dropped);

    // --- clarus_http_request_duration_microseconds_total ---
    out.push_str("# HELP clarus_http_request_duration_microseconds_total Total request duration in microseconds.\n");
    out.push_str("# TYPE clarus_http_request_duration_microseconds_total counter\n");
    let dur_sum = HTTP_DURATION_SUM_US.load(Ordering::Relaxed);
    push_metric(
        &mut out,
        "clarus_http_request_duration_microseconds_total",
        None,
        dur_sum,
    );

    // --- clarus_http_request_duration_count ---
    out.push_str("# HELP clarus_http_request_duration_count Number of timed requests.\n");
    out.push_str("# TYPE clarus_http_request_duration_count counter\n");
    let dur_count = HTTP_DURATION_COUNT.load(Ordering::Relaxed);
    push_metric(
        &mut out,
        "clarus_http_request_duration_count",
        None,
        dur_count,
    );

    // --- clarus_http_requests_labeled ---
    out.push_str("# HELP clarus_http_requests_labeled HTTP requests by method, handler, status.\n");
    out.push_str("# TYPE clarus_http_requests_labeled counter\n");
    for (i, info) in ROUTE_INFO.iter().enumerate() {
        for sc_idx in 0..NUM_STATUS_CLASSES {
            let val = ROUTE_COUNTERS[i * NUM_STATUS_CLASSES + sc_idx].load(Ordering::Relaxed);
            // Metrics: skip zero-valued labels - Prometheus fills gaps automatically
            if val == 0 {
                continue;
            }
            let status_label = match sc_idx {
                STATUS_2XX => "2xx",
                STATUS_4XX => "4xx",
                STATUS_5XX => "5xx",
                _ => "unknown",
            };
            let labels = format!(
                "method=\"{}\",handler=\"{}\",status=\"{}\"",
                info.method, info.handler, status_label
            );
            push_metric(&mut out, "clarus_http_requests_labeled", Some(&labels), val);
        }
    }

    // --- clarus_http_route_duration_seconds (0210) ---
    // Per-route mean latency source: Prometheus does
    // `rate(_sum)/rate(_count)`; the global HTTP_DURATION cannot say WHICH
    // route is slow, which is the only question this answers.
    out.push_str("# HELP clarus_http_route_duration_seconds_sum Total request duration per route, in seconds.\n");
    out.push_str("# TYPE clarus_http_route_duration_seconds_sum counter\n");
    out.push_str(
        "# HELP clarus_http_route_duration_seconds_count Number of timed requests per route.\n",
    );
    out.push_str("# TYPE clarus_http_route_duration_seconds_count counter\n");
    for (i, info) in ROUTE_INFO.iter().enumerate() {
        let count = ROUTE_DURATION_COUNT[i].load(Ordering::Relaxed);
        if count == 0 {
            continue; // same zero-skip policy as the labeled counters
        }
        let sum_us = ROUTE_DURATION_SUM_US[i].load(Ordering::Relaxed);
        let handler_label = format!("handler=\"{}\"", info.handler);
        let _ = writeln!(
            out,
            "clarus_http_route_duration_seconds_sum{{{}}} {:.6}",
            handler_label,
            sum_us as f64 / 1_000_000.0
        );
        push_metric(
            &mut out,
            "clarus_http_route_duration_seconds_count",
            Some(&handler_label),
            count,
        );
    }

    // --- clarus_storage_read_bytes_total / _write_bytes_total (0210) ---
    // Monotonic counters; Prometheus `rate()` turns them into MB/s.
    out.push_str(
        "# HELP clarus_storage_read_bytes_total Bytes read from storage to serve a response.\n",
    );
    out.push_str("# TYPE clarus_storage_read_bytes_total counter\n");
    push_metric(
        &mut out,
        "clarus_storage_read_bytes_total",
        None,
        STORAGE_READ_BYTES.load(Ordering::Relaxed),
    );

    out.push_str(
        "# HELP clarus_storage_write_bytes_total Bytes written to the cold tier by migration.\n",
    );
    out.push_str("# TYPE clarus_storage_write_bytes_total counter\n");
    push_metric(
        &mut out,
        "clarus_storage_write_bytes_total",
        None,
        STORAGE_WRITE_BYTES.load(Ordering::Relaxed),
    );

    // --- clarus_cas_write_bytes_total (0210) ---
    out.push_str("# HELP clarus_cas_write_bytes_total Bytes persisted into the hot CAS by STOW.\n");
    out.push_str("# TYPE clarus_cas_write_bytes_total counter\n");
    push_metric(
        &mut out,
        "clarus_cas_write_bytes_total",
        None,
        CAS_WRITE_BYTES.load(Ordering::Relaxed),
    );

    // --- clarus_cas_blobs_corrupted / _unreadable (0810) ---
    // Gauges set by the CAS audit walker when a run completes (reset to 0 at
    // the start of each run). 0 = the LAST COMPLETED audit found none; alert on
    // a step from 0 to >0 after a run, not on 0 alone.
    out.push_str(
        "# HELP clarus_cas_blobs_corrupted CAS blobs whose content hash did not match the filename (last completed audit).\n",
    );
    out.push_str("# TYPE clarus_cas_blobs_corrupted gauge\n");
    push_metric(
        &mut out,
        "clarus_cas_blobs_corrupted",
        None,
        CAS_BLOBS_CORRUPTED.load(Ordering::Relaxed),
    );
    out.push_str(
        "# HELP clarus_cas_blobs_unreadable CAS blobs that could not be read during the audit (last completed audit).\n",
    );
    out.push_str("# TYPE clarus_cas_blobs_unreadable gauge\n");
    push_metric(
        &mut out,
        "clarus_cas_blobs_unreadable",
        None,
        CAS_BLOBS_UNREADABLE.load(Ordering::Relaxed),
    );

    // --- clarus_commitment_requests_total (0210) ---
    // The verdict distribution of Storage Commitment over DICOMweb: POST is
    // the request, GET is the verdict poll (PS3.18 13.4/13.5).
    out.push_str("# HELP clarus_commitment_requests_total Storage Commitment requests by method and status.\n");
    out.push_str("# TYPE clarus_commitment_requests_total counter\n");
    for (method, counters) in [("POST", &COMMITMENT_POST), ("GET", &COMMITMENT_GET)] {
        for (sc_idx, counter) in counters.iter().enumerate() {
            let val = counter.load(Ordering::Relaxed);
            if val == 0 {
                continue;
            }
            let status_label = match sc_idx {
                STATUS_2XX => "2xx",
                STATUS_4XX => "4xx",
                STATUS_5XX => "5xx",
                _ => "unknown",
            };
            let labels = format!("method=\"{method}\",status=\"{status_label}\"");
            push_metric(
                &mut out,
                "clarus_commitment_requests_total",
                Some(&labels),
                val,
            );
        }
    }

    // --- Metrics: clarus_http_request_duration_seconds (Summary) ---
    out.push_str("# HELP clarus_http_request_duration_seconds Request duration in seconds.\n");
    out.push_str("# TYPE clarus_http_request_duration_seconds summary\n");
    let dur_count = REQUEST_DURATION.count.load(Ordering::Relaxed);
    if dur_count > 0 {
        if let Ok(sum) = REQUEST_DURATION.sum.lock() {
            push_metric(
                &mut out,
                "clarus_http_request_duration_seconds_count",
                None,
                dur_count,
            );
            let _ = writeln!(out, "clarus_http_request_duration_seconds_sum {:.6}", sum);
        }
        // Metrics: quantiles 0.5, 0.9, 0.99
        for &q in &[0.5, 0.9, 0.99] {
            if let Some(val) = REQUEST_DURATION.quantile(q) {
                let _ = writeln!(
                    out,
                    "clarus_http_request_duration_seconds{{quantile=\"{}\"}} {:.6}",
                    q, val
                );
            }
        }
    }

    // --- Metrics: clarus_storage_objects_total ---
    out.push_str("# HELP clarus_storage_objects_total Total number of DICOM objects in storage.\n");
    out.push_str("# TYPE clarus_storage_objects_total gauge\n");
    let objects = STORAGE_OBJECTS_TOTAL.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_storage_objects_total", None, objects);

    // --- Metrics: clarus_storage_instances_total (3008) ---
    out.push_str(
        "# HELP clarus_storage_instances_total Logical instances across live study manifests.\n",
    );
    out.push_str("# TYPE clarus_storage_instances_total gauge\n");
    let instances = STORAGE_INSTANCES_TOTAL.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_storage_instances_total", None, instances);

    // --- Metrics: clarus_storage_unreferenced_total (3008) ---
    out.push_str(
        "# HELP clarus_storage_unreferenced_total CAS files referenced by no live manifest (orphans, hidden-PHI alibis, leaks).\n",
    );
    out.push_str("# TYPE clarus_storage_unreferenced_total gauge\n");
    let unreferenced = STORAGE_UNREFERENCED_TOTAL.load(Ordering::Relaxed);
    push_metric(
        &mut out,
        "clarus_storage_unreferenced_total",
        None,
        unreferenced,
    );

    // --- Metrics: clarus_orphans_pending (3008) ---
    out.push_str(
        "# HELP clarus_orphans_pending Orphan-sidecar lines not yet reclaimed by the orphan GC.\n",
    );
    out.push_str("# TYPE clarus_orphans_pending gauge\n");
    let orphan = ORPHANS_PENDING.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_orphans_pending", None, orphan);

    // --- Metrics: clarus_storage_bytes_total ---
    out.push_str("# HELP clarus_storage_bytes_total Total size of DICOM storage in bytes.\n");
    out.push_str("# TYPE clarus_storage_bytes_total gauge\n");
    let storage_bytes = STORAGE_BYTES_TOTAL.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_storage_bytes_total", None, storage_bytes);

    // --- Metrics: clarus_dicom_stow_parse_errors_total (MET-005) ---
    out.push_str("# HELP clarus_dicom_stow_parse_errors_total Total number of failed DICOM parse attempts during STOW.\n");
    out.push_str("# TYPE clarus_dicom_stow_parse_errors_total counter\n");
    let parse_errors = DICOM_STOW_PARSE_ERRORS.load(Ordering::Relaxed);
    push_metric(
        &mut out,
        "clarus_dicom_stow_parse_errors_total",
        None,
        parse_errors,
    );

    // --- Metrics: clarus_stow_duplicates_total (STOW-statuses 16.08) ---
    out.push_str("# HELP clarus_stow_duplicates_total Number of accepted STOW instances that were exact duplicates (idempotent re-STOW).\n");
    out.push_str("# TYPE clarus_stow_duplicates_total counter\n");
    let duplicates = STOW_DUPLICATES_TOTAL.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_stow_duplicates_total", None, duplicates);

    // --- Metrics: clarus_chunks_total (MET-006) ---
    out.push_str("# HELP clarus_chunks_total Total number of pixel-block chunk hashes computed.\n");
    out.push_str("# TYPE clarus_chunks_total counter\n");
    let chunks_total = CLARUS_CHUNKS_TOTAL.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_chunks_total", None, chunks_total);

    // --- Metrics: clarus_chunks_unique (MET-006) ---
    out.push_str("# HELP clarus_chunks_unique Total number of unique pixel-block chunk hashes.\n");
    out.push_str("# TYPE clarus_chunks_unique counter\n");
    let chunks_unique = CLARUS_CHUNKS_UNIQUE.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_chunks_unique", None, chunks_unique);

    // --- Metrics: clarus_rejected_instances_total (IOCM rejection rate) ---
    // Paired with clarus_instances_stored_total; the rate is derived in
    // Prometheus. Help text says "rejection note", not "AI rejection": the
    // counter covers every reason, and the `reason` label is what separates a
    // Quality hide (reviewable through the QA surface) from an IOCM one
    // (permanent, no standard un-reject).
    out.push_str("# HELP clarus_rejected_instances_total Number of instances newly hidden by a rejection note (state changes only; re-rejections are not counted).\n");
    out.push_str("# TYPE clarus_rejected_instances_total counter\n");
    let rejected_instances = REJECTED_INSTANCES_TOTAL.load(Ordering::Relaxed);
    push_metric(
        &mut out,
        "clarus_rejected_instances_total",
        None,
        rejected_instances,
    );
    // The labelled series is the breakdown of the line above, so a reader can
    // attribute a rise without a second rate; both move together.
    for (_, label, counter) in REJECTED_BY_REASON.iter() {
        push_metric(
            &mut out,
            "clarus_rejected_by_reason_total",
            Some(&format!("reason=\"{label}\"")),
            counter.load(Ordering::Relaxed),
        );
    }

    // --- Metrics: clarus_restored_total (IOCM restore, admin extension) ---
    // Both counters stay monotonic; the currently-hidden population is the
    // PromQL offset `clarus_rejected_by_reason_total - clarus_restored_total`
    // per reason, which is why this is a separate series and not a decrement.
    out.push_str("# HELP clarus_restored_total Number of instances whose rejection was cleared by an admin restore (state changes only; restores of already-visible instances are not counted).\n");
    out.push_str("# TYPE clarus_restored_total counter\n");
    for (_, label, counter) in RESTORED_BY_REASON.iter() {
        push_metric(
            &mut out,
            "clarus_restored_total",
            Some(&format!("reason=\"{label}\"")),
            counter.load(Ordering::Relaxed),
        );
    }

    // --- Metrics: clarus_iocm_anyclass_ignored_total ---
    // Nothing was hidden, so this is deliberately NOT part of the rejection
    // counter. It reports notes the deployment declined to act on because the
    // IOCM AnyClass feature is switched off.
    out.push_str("# HELP clarus_iocm_anyclass_ignored_total Rejection references left visible because [server] anyclass_enable is false and the reason is an IOCM AnyClass reason.\n");
    out.push_str("# TYPE clarus_iocm_anyclass_ignored_total counter\n");
    push_metric(
        &mut out,
        "clarus_iocm_anyclass_ignored_total",
        Some("cause=\"disabled\""),
        IOCM_ANYCLASS_IGNORED_TOTAL.load(Ordering::Relaxed),
    );

    // --- Metrics: clarus_instances_stored_total (IOCM rejection rate) ---
    out.push_str("# HELP clarus_instances_stored_total Number of instances accepted by STOW, excluding exact duplicates.\n");
    out.push_str("# TYPE clarus_instances_stored_total counter\n");
    let instances_stored = INSTANCES_STORED_TOTAL.load(Ordering::Relaxed);
    push_metric(
        &mut out,
        "clarus_instances_stored_total",
        None,
        instances_stored,
    );

    // -- I/O resilience metrics --------------------------------------------

    // --- clarus_disk_probe_latency_ms ---
    out.push_str("# HELP clarus_disk_probe_latency_ms Disk write probe latency in milliseconds.\n");
    out.push_str("# TYPE clarus_disk_probe_latency_ms gauge\n");
    // Omitted while the last probe failed: a rejected write is not a
    // measurement of latency, and the alerting rule fires on high values - so
    // publishing the ~0 ms of an instant failure would report a dying volume
    // as a healthy one.
    if let Some(disk_ms) = crate::disk_health::probe_latency_ms() {
        push_metric(&mut out, "clarus_disk_probe_latency_ms", None, disk_ms);
    }

    // --- clarus_disk_free_bytes ---
    out.push_str(
        "# HELP clarus_disk_free_bytes Free space on the hot-storage partition, in bytes.\n",
    );
    out.push_str("# TYPE clarus_disk_free_bytes gauge\n");
    // Whole GiB only: the source gauge is a floored integer (`DISK_FREE_GB`,
    // metrics.rs) so the byte value inherits 1 GiB granularity - it is not
    // byte-precise. An exact byte gauge would need a third atomic fed by the
    // `disk-free-probe` thread; alerting rules trigger on 1 GiB steps anyway.
    let free_bytes = DISK_FREE_GB.load(Ordering::Relaxed) * 1024 * 1024 * 1024;
    push_metric(&mut out, "clarus_disk_free_bytes", None, free_bytes);

    // --- clarus_disk_free_percent ---
    // Published alongside the probe latency so an operator can ALERT on a
    // filling volume: latency alone reports a healthy disk until the first 507.
    // Read from the atomics the `disk-free-probe` thread refreshes (main.rs) -
    // a statvfs here would block the scrape path it exists to keep cheap.
    out.push_str("# HELP clarus_disk_free_percent Free space on the hot-storage partition, in percent (0-100).\n");
    out.push_str("# TYPE clarus_disk_free_percent gauge\n");
    let free_pct = DISK_FREE_PCT.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_disk_free_percent", None, free_pct);

    // -- B-tree index staleness ----------------------------------------------
    // 0 = the B-tree `.idx` is fully consistent with the store.

    // --- clarus_idx_stale_objects ---
    out.push_str(
        "# HELP clarus_idx_stale_objects CAS objects not yet in the B-tree idx (0 = ready).\n",
    );
    out.push_str("# TYPE clarus_idx_stale_objects gauge\n");
    let stale = CLARUS_IDX_STALE_OBJECTS.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_idx_stale_objects", None, stale);

    // --- clarus_idx_runtime_stale ---
    // Live staleness the runtime fixer is working through (detected - healed),
    // converging to 0. Distinct from clarus_idx_stale_objects (startup pass).
    out.push_str(
        "# HELP clarus_idx_runtime_stale Runtime modality-idx staleness (detected - healed); 0 = clean.\n",
    );
    out.push_str("# TYPE clarus_idx_runtime_stale gauge\n");
    push_metric(
        &mut out,
        "clarus_idx_runtime_stale",
        None,
        crate::metrics::idx_runtime_stale(),
    );

    // --- clarus_idx_detected_total / clarus_idx_healed_total ---
    out.push_str(
        "# HELP clarus_idx_detected_total Runtime staleness detections (exact lookup, study in manifest not in modality-idx).\n",
    );
    out.push_str("# TYPE clarus_idx_detected_total counter\n");
    let detected = CLARUS_IDX_DETECTED_TOTAL.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_idx_detected_total", None, detected);
    out.push_str(
        "# HELP clarus_idx_healed_total Runtime modality-idx rows appended by the fixer thread.\n",
    );
    out.push_str("# TYPE clarus_idx_healed_total counter\n");
    let healed = CLARUS_IDX_HEALED_TOTAL.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_idx_healed_total", None, healed);

    // --- clarus_manifest_missing ---
    out.push_str(
        "# HELP clarus_manifest_missing 1 = manifests missing/empty while CAS is non-empty (manifest-degraded).\n",
    );
    out.push_str("# TYPE clarus_manifest_missing gauge\n");
    let mmissing = MANIFEST_MISSING.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_manifest_missing", None, mmissing);

    // --- clarus_manifest_corrupt ---
    out.push_str("# HELP clarus_manifest_corrupt Count of manifest files that failed to decode.\n");
    out.push_str("# TYPE clarus_manifest_corrupt gauge\n");
    let mcorrupt = MANIFEST_CORRUPT.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_manifest_corrupt", None, mcorrupt);

    // --- clarus_manifest_degraded ---
    out.push_str(
        "# HELP clarus_manifest_degraded 1 when /health should report manifest-degraded.\n",
    );
    out.push_str("# TYPE clarus_manifest_degraded gauge\n");
    let mdegraded = MANIFEST_DEGRADED.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_manifest_degraded", None, mdegraded);

    // --- clarus_erase_pending ---
    out.push_str(
        "# HELP clarus_erase_pending Deleted studies (tombstones) awaiting physical erasure.\n",
    );
    out.push_str("# TYPE clarus_erase_pending gauge\n");
    let pending = ERASE_PENDING.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_erase_pending", None, pending);

    // --- clarus_process_rss_bytes ---
    out.push_str("# HELP clarus_process_rss_bytes Process RSS in bytes.\n");
    out.push_str("# TYPE clarus_process_rss_bytes gauge\n");
    let rss = PROCESS_RSS_BYTES.load(Ordering::Relaxed);
    push_metric(&mut out, "clarus_process_rss_bytes", None, rss);

    // -- Bloom filter metrics are not exposed ------------------------------------

    // -- S3 cold storage metrics (FR-017) --------------------------
    #[cfg(feature = "s3")]
    {
        let s3_reachable_raw = crate::metrics::S3_REACHABLE.load(Ordering::Relaxed);
        let s3_reachable: i64 = if s3_reachable_raw == u64::MAX {
            -1
        } else {
            s3_reachable_raw as i64
        };
        out.push_str("# HELP clarus_s3_reachable 1=reachable, 0=down, -1=not configured.\n");
        out.push_str("# TYPE clarus_s3_reachable gauge\n");
        push_metric_i64(&mut out, "clarus_s3_reachable", None, s3_reachable);

        let s3_latency_raw = crate::metrics::S3_LATENCY_MS.load(Ordering::Relaxed);
        let s3_latency: i64 = if s3_latency_raw == u64::MAX {
            -1
        } else {
            s3_latency_raw as i64
        };
        out.push_str("# HELP clarus_s3_latency_ms Last S3 HEAD probe latency in ms.\n");
        out.push_str("# TYPE clarus_s3_latency_ms gauge\n");
        push_metric_i64(&mut out, "clarus_s3_latency_ms", None, s3_latency);

        let s3_objects_raw = crate::metrics::S3_OBJECTS_TOTAL.load(Ordering::Relaxed);
        let s3_objects: i64 = if s3_objects_raw == u64::MAX {
            -1
        } else {
            s3_objects_raw as i64
        };
        out.push_str(
            "# HELP clarus_s3_objects Number of objects in S3 bucket. -1=not configured.\n",
        );
        out.push_str("# TYPE clarus_s3_objects gauge\n");
        push_metric_i64(&mut out, "clarus_s3_objects", None, s3_objects);

        let s3_bytes_raw = crate::metrics::S3_BYTES_TOTAL.load(Ordering::Relaxed);
        let s3_bytes: i64 = if s3_bytes_raw == u64::MAX {
            -1
        } else {
            s3_bytes_raw as i64
        };
        out.push_str("# HELP clarus_s3_bytes Total bytes in S3 bucket. -1=not configured.\n");
        out.push_str("# TYPE clarus_s3_bytes gauge\n");
        push_metric_i64(&mut out, "clarus_s3_bytes", None, s3_bytes);

        let s3_migrated = crate::metrics::S3_MIGRATIONS_TOTAL.load(Ordering::Relaxed);
        out.push_str("# HELP clarus_s3_migrations_total Successful S3 migrations.\n");
        out.push_str("# TYPE clarus_s3_migrations_total counter\n");
        push_metric(&mut out, "clarus_s3_migrations_total", None, s3_migrated);

        let s3_errors = crate::metrics::S3_MIGRATION_ERRORS.load(Ordering::Relaxed);
        out.push_str("# HELP clarus_s3_migration_errors_total Failed S3 migration attempts.\n");
        out.push_str("# TYPE clarus_s3_migration_errors_total counter\n");
        push_metric(
            &mut out,
            "clarus_s3_migration_errors_total",
            None,
            s3_errors,
        );
    }

    // Access-audit journal (2808): counters, not gauges (per-request events;
    // the anonymous/identified ratio is derived in Prometheus).
    let access_dropped = crate::metrics::ACCESS_AUDIT_DROPPED.load(Ordering::Relaxed);
    out.push_str(
        "# HELP clarus_access_audit_dropped_total Access journal lines dropped (ring full).\n",
    );
    out.push_str("# TYPE clarus_access_audit_dropped_total counter\n");
    push_metric(
        &mut out,
        "clarus_access_audit_dropped_total",
        None,
        access_dropped,
    );
    let access_anonymous = crate::metrics::ACCESS_ANONYMOUS_TOTAL.load(Ordering::Relaxed);
    out.push_str("# HELP clarus_access_anonymous_total Access events without user attribution.\n");
    out.push_str("# TYPE clarus_access_anonymous_total counter\n");
    push_metric(
        &mut out,
        "clarus_access_anonymous_total",
        None,
        access_anonymous,
    );
    let access_identified = crate::metrics::ACCESS_IDENTIFIED_TOTAL.load(Ordering::Relaxed);
    out.push_str("# HELP clarus_access_identified_total Access events with user attribution.\n");
    out.push_str("# TYPE clarus_access_identified_total counter\n");
    push_metric(
        &mut out,
        "clarus_access_identified_total",
        None,
        access_identified,
    );

    // --- clarus_cad_workitem_failures_total ---
    // CAD workitem creation failures after a durable STOW (origin-server
    // capability, PS3.18 11.1.1.1). A duplicate workitem is not a failure.
    let cad_failures = CAD_WORKITEM_FAILURES_TOTAL.load(Ordering::Relaxed);
    out.push_str(
        "# HELP clarus_cad_workitem_failures_total CAD workitems that failed to be created after a durable STOW.\n",
    );
    out.push_str("# TYPE clarus_cad_workitem_failures_total counter\n");
    push_metric(
        &mut out,
        "clarus_cad_workitem_failures_total",
        None,
        cad_failures,
    );

    // --- worklist subscription push (PS3.18 11.10) ---
    // `pushed` counts deliveries the subscriber acknowledged with 2xx,
    // `retries` counts non-2xx/transport failures that were retried, and
    // `dropped` counts jobs abandoned after the attempt budget ran out or
    // rejected because the queue was full.
    let pushed = UPS_PUSHED.load(Ordering::Relaxed);
    let retries = UPS_PUSH_RETRIES.load(Ordering::Relaxed);
    let dropped = UPS_PUSH_DROPPED.load(Ordering::Relaxed);

    out.push_str(
        "# HELP clarus_ups_push_delivered_total Workitem pushes acknowledged by a subscriber.\n",
    );
    out.push_str("# TYPE clarus_ups_push_delivered_total counter\n");
    push_metric(&mut out, "clarus_ups_push_delivered_total", None, pushed);

    out.push_str(
        "# HELP clarus_ups_push_retries_total Workitem pushes that failed and were retried.\n",
    );
    out.push_str("# TYPE clarus_ups_push_retries_total counter\n");
    push_metric(&mut out, "clarus_ups_push_retries_total", None, retries);

    out.push_str(
        "# HELP clarus_ups_push_dropped_total Workitem pushes abandoned after the attempt budget.\n",
    );
    out.push_str("# TYPE clarus_ups_push_dropped_total counter\n");
    push_metric(&mut out, "clarus_ups_push_dropped_total", None, dropped);

    out
}

/// Append a single Prometheus line to the buffer.
#[inline]
fn push_metric(buf: &mut String, name: &str, labels: Option<&str>, value: u64) {
    match labels {
        Some(l) => {
            let _ = writeln!(buf, "{name}{{{l}}} {value}");
        }
        None => {
            let _ = writeln!(buf, "{name} {value}");
        }
    }
}

/// Append a Prometheus line with an i64 value (for gauges with -1 sentinel).
#[cfg(feature = "s3")]
#[inline]
fn push_metric_i64(buf: &mut String, name: &str, labels: Option<&str>, value: i64) {
    match labels {
        Some(l) => {
            let _ = writeln!(buf, "{name}{{{l}}} {value}");
        }
        None => {
            let _ = writeln!(buf, "{name} {value}");
        }
    }
}

// -- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // The gauges are imported inside `render_metrics` for scrape locality, so
    // the test module reaches them through their full path.
    use crate::metrics::{DISK_FREE_GB, DISK_FREE_PCT};
    use std::sync::atomic::Ordering;

    /// Borrow the process-global free-space gauges and put them back on drop.
    ///
    /// The atomics are shared with the `disk-free-probe` thread and with the
    /// `/health/status` reader, so a test that left them non-zero would leak a
    /// false reading into every other test in this binary.
    struct DiskGaugesGuard {
        gb: u64,
        pct: u64,
    }

    impl DiskGaugesGuard {
        fn set(gb: u64, pct: u64) -> Self {
            let guard = DiskGaugesGuard {
                gb: DISK_FREE_GB.load(Ordering::Relaxed),
                pct: DISK_FREE_PCT.load(Ordering::Relaxed),
            };
            DISK_FREE_GB.store(gb, Ordering::Relaxed);
            DISK_FREE_PCT.store(pct, Ordering::Relaxed);
            guard
        }
    }

    impl Drop for DiskGaugesGuard {
        fn drop(&mut self) {
            DISK_FREE_GB.store(self.gb, Ordering::Relaxed);
            DISK_FREE_PCT.store(self.pct, Ordering::Relaxed);
        }
    }

    /// `/metrics` must publish the free-space gauges that `/health/status`
    /// colours its DISK indicator from: the write-probe latency cannot tell a
    /// nearly-full volume apart from a healthy one.
    #[test]
    fn disk_free_gauges_are_rendered() {
        const GB: u64 = 42;
        let _guard = DiskGaugesGuard::set(GB, 37);
        let out = render_metrics();

        assert!(out.contains("# TYPE clarus_disk_free_bytes gauge\n"));
        assert!(out.contains("# TYPE clarus_disk_free_percent gauge\n"));
        assert!(
            out.contains(&format!(
                "\nclarus_disk_free_bytes {}\n",
                GB * 1024 * 1024 * 1024
            )),
            "missing clarus_disk_free_bytes sample:\n{out}"
        );
        assert!(
            out.contains("\nclarus_disk_free_percent 37\n"),
            "missing clarus_disk_free_percent sample:\n{out}"
        );
    }
}
