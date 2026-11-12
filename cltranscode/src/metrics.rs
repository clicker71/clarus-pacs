// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Prometheus-compatible in-memory metrics and health endpoint.
//!
//! ## Design
//! - All counters use [`std::sync::atomic::AtomicU64`] - lock-free, zero heap
//!   allocation in the hot path.
//! - Prometheus text rendering allocates a `String` **only** during scrape
//!   (GET `/metrics`), not under DICOM load.
//! - Health/HTTP server: raw `TcpListener`, no framework. nginx-way.

#![forbid(unsafe_code)]

use std::fmt::Write;
use std::io::{Read, Write as IoWrite};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

// -- Metric storage - lock-free Atomics --------------------------------

/// Counter: total DICOM instances received via DIMSE.
pub static INPUT_DIMSE_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Counter: total DICOM instances received via DICOMweb.
pub static INPUT_DICOMWEB_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Counter: successful DIMSE deliveries.
pub static OUTPUT_DIMSE_OK: AtomicU64 = AtomicU64::new(0);

/// Counter: failed DIMSE deliveries.
pub static OUTPUT_DIMSE_FAIL: AtomicU64 = AtomicU64::new(0);

/// Counter: successful DICOMweb deliveries.
pub static OUTPUT_DICOMWEB_OK: AtomicU64 = AtomicU64::new(0);

/// Counter: failed DICOMweb deliveries.
pub static OUTPUT_DICOMWEB_FAIL: AtomicU64 = AtomicU64::new(0);

/// Counter: total transcoded instances.
pub static TRANSCODE_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Counter: fallback as-is (transcoding failed, sent original).
pub static TRANSCODE_FALLBACK_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Counter: total anonymized instances.
pub static ANONYMIZE_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Gauge: files in outbox raw/ (pipeline backlog).
pub static SPOOL_RAW: AtomicU64 = AtomicU64::new(0);

/// Gauge: files in outbox cooked/ (awaiting delivery).
pub static SPOOL_COOKED: AtomicU64 = AtomicU64::new(0);

/// Gauge: files in outbox done/ (delivered, awaiting retention cleanup).
pub static SPOOL_DONE: AtomicU64 = AtomicU64::new(0);

/// Gauge: files in outbox failed/ (gave up after max retries).
pub static SPOOL_FAILED: AtomicU64 = AtomicU64::new(0);

/// Counter: cumulative files moved to outbox failed/.
pub static OUTBOX_FAILED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Gauge: downstream health - 0 = Green (reachable), 1 = Black (unreachable).
pub static HEALTH_STATUS: AtomicU64 = AtomicU64::new(0);

/// Gauge: process uptime start instant.
static UPTIME_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

// -- Recording functions - called from main loop (hot path) ------------

/// Initialize uptime clock. Called once at startup.
pub fn init_uptime() {
    let _ = UPTIME_START.set(Instant::now());
}

/// Record input received via DIMSE.
#[inline]
pub fn record_input_dimse() {
    INPUT_DIMSE_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Record input received via DICOMweb.
#[inline]
pub fn record_input_dicomweb() {
    INPUT_DICOMWEB_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Record successful DIMSE output.
#[inline]
pub fn record_output_dimse_ok() {
    OUTPUT_DIMSE_OK.fetch_add(1, Ordering::Relaxed);
}

/// Record failed DIMSE output.
#[inline]
pub fn record_output_dimse_fail() {
    OUTPUT_DIMSE_FAIL.fetch_add(1, Ordering::Relaxed);
}

/// Record successful DICOMweb output.
#[inline]
pub fn record_output_dicomweb_ok() {
    OUTPUT_DICOMWEB_OK.fetch_add(1, Ordering::Relaxed);
}

/// Record failed DICOMweb output.
#[inline]
pub fn record_output_dicomweb_fail() {
    OUTPUT_DICOMWEB_FAIL.fetch_add(1, Ordering::Relaxed);
}

/// Record a successful transcode.
#[inline]
pub fn record_transcode() {
    TRANSCODE_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Record a transcode fallback (as-is).
#[inline]
pub fn record_transcode_fallback() {
    TRANSCODE_FALLBACK_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Record an anonymization.
#[inline]
pub fn record_anonymize() {
    ANONYMIZE_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Update spool gauge: raw/ (pipeline backlog).
#[inline]
pub fn set_spool_raw(count: u64) {
    SPOOL_RAW.store(count, Ordering::Relaxed);
}

/// Update spool gauge: cooked/ (awaiting delivery).
#[inline]
pub fn set_spool_cooked(count: u64) {
    SPOOL_COOKED.store(count, Ordering::Relaxed);
}

/// Update spool gauge: done/ (delivered, awaiting cleanup).
#[inline]
pub fn set_spool_done(count: u64) {
    SPOOL_DONE.store(count, Ordering::Relaxed);
}

/// Update spool gauge: failed/ (gave up).
#[inline]
pub fn set_spool_failed(count: u64) {
    SPOOL_FAILED.store(count, Ordering::Relaxed);
}

/// Update all spool gauges at once from a Spool reference.
pub fn update_spool_gauges(spool: &crate::spool::Spool) {
    set_spool_raw(spool.count_raw());
    set_spool_cooked(spool.count_cooked());
    set_spool_done(spool.count_done());
    set_spool_failed(spool.count_failed());
}

/// Update downstream health status gauge.
#[inline]
pub fn set_health_status(is_black: bool) {
    HEALTH_STATUS.store(if is_black { 1 } else { 0 }, Ordering::Relaxed);
}

/// Record a file moved to outbox failed/.
#[inline]
pub fn record_outbox_failed() {
    OUTBOX_FAILED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

// -- Health & Metrics HTTP server --------------------------------------

/// Start the health/metrics HTTP server on the given port.
///
/// Spawns a background thread to avoid blocking the main loop.
/// Handles only:
/// - `GET /health` -> liveness
/// - `GET /health?readiness=true` -> readiness (if health gate provided)
/// - `GET /metrics` -> Prometheus text format
/// - Everything else -> 404
pub fn start_metrics_server(port: u16, read_check: Option<fn() -> bool>) {
    thread::spawn(move || {
        let addr = format!("0.0.0.0:{}", port);
        let listener = match TcpListener::bind(&addr) {
            Ok(l) => {
                log::info!("metrics: listening on {} (health + /metrics)", addr);
                l
            }
            Err(e) => {
                log::error!("metrics: failed to bind {}: {}", addr, e);
                eprintln!(
                    "cltranscode: failed to start health/metrics server on {}: {}",
                    addr, e
                );
                std::process::exit(1);
            }
        };
        listener
            .set_nonblocking(true)
            .expect("metrics: nonblocking");

        loop {
            match listener.accept() {
                Ok((stream, addr)) => {
                    log::debug!("metrics: connection from {}", addr);
                    handle_http(stream, &read_check);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    log::error!("metrics: accept error: {}", e);
                    break;
                }
            }
        }
    });
}

/// Handle a single HTTP connection.
fn handle_http(mut stream: TcpStream, read_check: &Option<fn() -> bool>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();

    if parts.len() < 2 {
        send_response(&mut stream, 400, "text/plain", "Bad Request");
        return;
    }

    let method = parts[0];
    let path = parts[1];

    if method != "GET" {
        send_response(&mut stream, 405, "text/plain", "Method Not Allowed");
        return;
    }

    match path {
        "/health" => {
            // Check readiness query param
            let is_readiness = request.contains("readiness=true");
            if is_readiness {
                if let Some(check) = read_check {
                    if check() {
                        send_response(&mut stream, 200, "text/plain", "OK\n");
                    } else {
                        send_response(&mut stream, 503, "text/plain", "Service Unavailable\n");
                    }
                } else {
                    // No readiness check configured -> always ready
                    send_response(&mut stream, 200, "text/plain", "OK\n");
                }
            } else {
                // Liveness: process is alive
                send_response(&mut stream, 200, "text/plain", "OK\n");
            }
        }
        "/metrics" => {
            let body = render_metrics();
            let content_type = "text/plain; version=0.0.4";
            send_response(&mut stream, 200, content_type, &body);
        }
        _ => {
            send_response(&mut stream, 404, "text/plain", "Not Found\n");
        }
    }
}

fn send_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        status_text,
        content_type,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

// -- Prometheus text rendering -----------------------------------------

/// Render all Prometheus metrics into a `String`.
///
/// Called only during GET `/metrics` scrape. Allocation acceptable.
fn render_metrics() -> String {
    let mut out = String::with_capacity(2048);

    // --- cltranscode_input_total ---
    out.push_str("# HELP cltranscode_input_total DICOM instances received.\n");
    out.push_str("# TYPE cltranscode_input_total counter\n");
    push_metric(
        &mut out,
        "cltranscode_input_total",
        Some("protocol=\"dimse\""),
        INPUT_DIMSE_TOTAL.load(Ordering::Relaxed),
    );
    push_metric(
        &mut out,
        "cltranscode_input_total",
        Some("protocol=\"dicomweb\""),
        INPUT_DICOMWEB_TOTAL.load(Ordering::Relaxed),
    );

    // --- cltranscode_output_total ---
    out.push_str("# HELP cltranscode_output_total DICOM instances delivered by status.\n");
    out.push_str("# TYPE cltranscode_output_total counter\n");
    push_metric(
        &mut out,
        "cltranscode_output_total",
        Some("protocol=\"dimse\",status=\"ok\""),
        OUTPUT_DIMSE_OK.load(Ordering::Relaxed),
    );
    push_metric(
        &mut out,
        "cltranscode_output_total",
        Some("protocol=\"dimse\",status=\"fail\""),
        OUTPUT_DIMSE_FAIL.load(Ordering::Relaxed),
    );
    push_metric(
        &mut out,
        "cltranscode_output_total",
        Some("protocol=\"dicomweb\",status=\"ok\""),
        OUTPUT_DICOMWEB_OK.load(Ordering::Relaxed),
    );
    push_metric(
        &mut out,
        "cltranscode_output_total",
        Some("protocol=\"dicomweb\",status=\"fail\""),
        OUTPUT_DICOMWEB_FAIL.load(Ordering::Relaxed),
    );

    // --- cltranscode_transcode_total ---
    out.push_str("# HELP cltranscode_transcode_total Successful transcodes.\n");
    out.push_str("# TYPE cltranscode_transcode_total counter\n");
    push_metric(
        &mut out,
        "cltranscode_transcode_total",
        None,
        TRANSCODE_TOTAL.load(Ordering::Relaxed),
    );

    // --- cltranscode_transcode_fallback_total ---
    out.push_str(
        "# HELP cltranscode_transcode_fallback_total Fallback as-is (transcoding failed).\n",
    );
    out.push_str("# TYPE cltranscode_transcode_fallback_total counter\n");
    push_metric(
        &mut out,
        "cltranscode_transcode_fallback_total",
        None,
        TRANSCODE_FALLBACK_TOTAL.load(Ordering::Relaxed),
    );

    // --- cltranscode_anonymize_total ---
    out.push_str("# HELP cltranscode_anonymize_total DICOM instances anonymized.\n");
    out.push_str("# TYPE cltranscode_anonymize_total counter\n");
    push_metric(
        &mut out,
        "cltranscode_anonymize_total",
        None,
        ANONYMIZE_TOTAL.load(Ordering::Relaxed),
    );

    // --- cltranscode_spool_files ---
    out.push_str("# HELP cltranscode_spool_files Files in each spool directory.\n");
    out.push_str("# TYPE cltranscode_spool_files gauge\n");
    push_metric(
        &mut out,
        "cltranscode_spool_files",
        Some("state=\"raw\""),
        SPOOL_RAW.load(Ordering::Relaxed),
    );
    push_metric(
        &mut out,
        "cltranscode_spool_files",
        Some("state=\"cooked\""),
        SPOOL_COOKED.load(Ordering::Relaxed),
    );
    push_metric(
        &mut out,
        "cltranscode_spool_files",
        Some("state=\"done\""),
        SPOOL_DONE.load(Ordering::Relaxed),
    );
    push_metric(
        &mut out,
        "cltranscode_spool_files",
        Some("state=\"failed\""),
        SPOOL_FAILED.load(Ordering::Relaxed),
    );

    // --- cltranscode_outbox_failed_total ---
    out.push_str(
        "# HELP cltranscode_outbox_failed_total Cumulative files moved to outbox failed/.\n",
    );
    out.push_str("# TYPE cltranscode_outbox_failed_total counter\n");
    push_metric(
        &mut out,
        "cltranscode_outbox_failed_total",
        None,
        OUTBOX_FAILED_TOTAL.load(Ordering::Relaxed),
    );

    // --- cltranscode_health_status ---
    out.push_str("# HELP cltranscode_health_status Downstream DIMSE health (0=Green, 1=Black).\n");
    out.push_str("# TYPE cltranscode_health_status gauge\n");
    push_metric(
        &mut out,
        "cltranscode_health_status",
        None,
        HEALTH_STATUS.load(Ordering::Relaxed),
    );

    // --- cltranscode_uptime_seconds ---
    out.push_str("# HELP cltranscode_uptime_seconds Process uptime in seconds.\n");
    out.push_str("# TYPE cltranscode_uptime_seconds gauge\n");
    let uptime = UPTIME_START
        .get()
        .map(|start| start.elapsed().as_secs())
        .unwrap_or(0);
    push_metric(&mut out, "cltranscode_uptime_seconds", None, uptime);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_metrics_returns_prometheus_format() {
        record_input_dimse();
        record_output_dimse_ok();
        record_transcode();
        record_anonymize();

        let out = render_metrics();
        assert!(out.contains("cltranscode_input_total{protocol=\"dimse\"}"));
        assert!(out.contains("cltranscode_output_total{protocol=\"dimse\",status=\"ok\"}"));
        assert!(out.contains("cltranscode_transcode_total"));
        assert!(out.contains("cltranscode_anonymize_total"));
        assert!(out.contains("cltranscode_uptime_seconds"));
    }

    #[test]
    fn counters_increment() {
        let before = INPUT_DIMSE_TOTAL.load(Ordering::Relaxed);
        record_input_dimse();
        record_input_dimse();
        assert_eq!(INPUT_DIMSE_TOTAL.load(Ordering::Relaxed), before + 2);
    }

    #[test]
    fn outbox_gauge_updates() {
        set_spool_cooked(42);
        assert_eq!(SPOOL_COOKED.load(Ordering::Relaxed), 42);
        set_spool_cooked(0);
        assert_eq!(SPOOL_COOKED.load(Ordering::Relaxed), 0);
    }
}
