// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Freeze admin API wire behaviour.
//!
//! Boots the server over a real TCP socket and exercises the admin
//! endpoints: status, dashboard, storage (merged CAS schema), maintenance,
//! storage migrate, CAS audit, and the `/logs/stream` Server-Sent Events
//! endpoint (snapshot then poll). The `db/*` endpoints were removed in
//! (clean v1->v2 break); their scenarios are gone with them.

#![forbid(unsafe_code)]

use clarus_server::config::Config;
use clarus_server::handlers::json_common::{JsonVal, parse_json};
use clarus_server::shadow::ShadowTier;
use clarus_server::state::{AppState, LogBuffer, LogEntry};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Test server harness (mirror of ups_smoke.rs / wado_smoke.rs) -------

struct TestServer {
    addr: SocketAddr,
    log_buffer: LogBuffer,
    _temp: tempfile::TempDir,
}

impl TestServer {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");

        let btree_root = temp.path().join("btree");
        std::fs::create_dir_all(&btree_root).expect("btree dir");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let mut config = Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        let log_buffer: LogBuffer = Arc::new(StdRwLock::new(std::collections::VecDeque::new()));

        let state = AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(temp.path().join("cas"))
                .expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: clarus_server::sse_fanout::DualEventSink::new(),
            start_time: Instant::now(),
            metrics_ring: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            log_buffer: log_buffer.clone(),
            maintenance: Arc::new(StdRwLock::new(
                clarus_server::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(state);
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let sd = shutdown.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, state, sd, timeouts, u32::MAX);
        });

        Self {
            addr,
            log_buffer,
            _temp: temp,
        }
    }

    /// Push a log entry into the ring buffer (mimics a running server).
    fn push_log(&self, level: &str, component: &str, message: &str, tag: Option<&str>) {
        if let Ok(mut buf) = self.log_buffer.write() {
            buf.push_back(LogEntry {
                timestamp: "2026-08-13T00:00:00Z".to_string(),
                level: level.to_string(),
                component: component.to_string(),
                message: message.to_string(),
                tag: tag.map(|s| s.to_string()),
            });
        }
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        self.request("GET", path, headers, None)
    }

    fn post_json(&self, path: &str, body: &[u8]) -> Response {
        self.request(
            "POST",
            path,
            &[("Content-Type", "application/json")],
            Some(body),
        )
    }

    fn post_empty(&self, path: &str) -> Response {
        self.request("POST", path, &[], None)
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let body = body.unwrap_or(&[]);
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.addr.port(),
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let mut req = head.into_bytes();
        req.extend_from_slice(body);
        stream.write_all(&req).expect("write req");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        parse_http(&buf)
    }

    /// Open a persistent SSE connection to `/logs/stream` and accumulate raw
    /// bytes to a shared buffer.
    fn open_sse(
        &self,
        query: &str,
        out: Arc<StdRwLock<Vec<u8>>>,
        done: Arc<AtomicBool>,
    ) -> SseGuard {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "GET /admin/v1/logs/stream{query} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nAccept: text/event-stream\r\n\r\n",
            self.addr.port()
        );
        stream.write_all(head.as_bytes()).expect("write sse req");

        let done2 = done.clone();
        let out2 = out.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut acc: Vec<u8> = Vec::new();
            while !done2.load(Ordering::Relaxed) {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => acc.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            out2.write().unwrap().extend_from_slice(&acc);
        });

        SseGuard { handle, done }
    }
}

struct SseGuard {
    handle: std::thread::JoinHandle<()>,
    done: Arc<AtomicBool>,
}

impl SseGuard {
    fn close(self) {
        self.done.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

fn parse_http(buf: &[u8]) -> Response {
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let body = buf[head_end + 4..].to_vec();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status code");
    let mut headers = std::collections::HashMap::new();
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            headers
                .entry(k.trim().to_ascii_lowercase())
                .or_insert_with(Vec::new)
                .push(v.trim().to_string());
        }
    }
    let is_chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.first().map(|s| s.contains("chunked")).unwrap_or(false));
    let body = if is_chunked {
        decode_chunked(&body).expect("chunked body")
    } else {
        body
    };
    Response { status, body }
}

fn decode_chunked(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let line_end = find_crlf(data, pos)?;
        let size_str = std::str::from_utf8(&data[pos..line_end]).ok()?;
        let size_str = size_str.split(';').next().unwrap_or(size_str).trim();
        let chunk_size = usize::from_str_radix(size_str, 16).ok()?;
        let data_start = line_end + 2;
        if chunk_size == 0 {
            return Some(out);
        }
        let chunk = data.get(data_start..data_start + chunk_size)?;
        out.extend_from_slice(chunk);
        pos = data_start + chunk_size + 2;
    }
}

fn find_crlf(data: &[u8], start: usize) -> Option<usize> {
    data[start..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| start + i)
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

impl Response {
    fn assert_status(&self, expected: u16) {
        assert_eq!(self.status, expected, "status should be {expected}");
    }
    fn json(&self) -> JsonVal {
        parse_json(&self.body).expect("json body")
    }
}

// -- Scenario 1: status -------------------------------------------------

#[test]
fn admin_status_200() {
    let srv = TestServer::start();
    let r = srv.get("/admin/v1/status", &[]);
    r.assert_status(200);
    // The status body carries floats (memory/storage MB), which the native
    // request-body parser (`JsonVal`) intentionally does not represent, so
    // assert on the raw wire bytes (a byte-exact shape freeze).
    let body = String::from_utf8_lossy(&r.body);
    assert!(body.contains("\"status\":\"ok\""), "status ok: {body}");
    assert!(body.contains("\"version\":\""), "version present: {body}");
    assert!(
        body.contains("\"uptime_seconds\":"),
        "uptime present: {body}"
    );
    // 6c: manifest/idx counters replace the SQLite-shape fields.
    assert!(
        body.contains("\"manifest_count\":"),
        "manifest_count present"
    );
    assert!(
        body.contains("\"idx_stale_objects\":"),
        "idx_stale_objects present"
    );
    assert!(!body.contains("db_size_mb"), "db_size_mb removed");
    assert!(!body.contains("wal_size_mb"), "wal_size_mb removed");
}

#[test]
fn admin_dashboard_200() {
    let srv = TestServer::start();
    let r = srv.get("/admin/v1/dashboard", &[]);
    r.assert_status(200);
    let body = String::from_utf8_lossy(&r.body);
    assert!(body.contains("\"status\":\"ok\""), "status ok: {body}");
    assert!(
        body.contains("\"manifest_count\":"),
        "manifest_count present"
    );
    assert!(
        body.contains("\"idx_stale_objects\":"),
        "idx_stale_objects present"
    );
    assert!(!body.contains("db_size_mb"), "db_size_mb removed");
    assert!(body.contains("\"storage\":{"), "storage object present");
    assert!(body.contains("\"cas_files\":"), "storage.cas_files");
}

#[test]
fn admin_storage_200() {
    let srv = TestServer::start();
    let r = srv.get("/admin/v1/storage", &[]);
    r.assert_status(200);
    // The storage body carries floats (dedup_ratio/cas_size_mb), which the
    // native request-body parser (`JsonVal`) intentionally does not represent,
    // so assert on the raw wire bytes.
    let body = String::from_utf8_lossy(&r.body);
    // 6d: CAS fields (previously in db_stats) live in the merged storage body.
    assert!(body.contains("\"logical_instances\":"), "logical_instances");
    assert!(
        body.contains("\"physical_cas_files\":"),
        "physical_cas_files"
    );
    assert!(body.contains("\"dedup_ratio\":"), "dedup_ratio");
    assert!(body.contains("\"cas_size_mb\":"), "cas_size_mb");
    assert!(body.contains("\"manifest_count\":"), "manifest_count");
    assert!(
        body.contains("\"idx_stale_objects\":"),
        "idx_stale_objects present"
    );
    assert!(body.contains("\"hot\":{\"files\":"), "hot.files");
    assert!(body.contains("\"cold\":{\"files\":"), "cold.files");
    assert!(body.contains("\"disk\":{\"used_mb\":"), "disk.used_mb");
}

// -- Scenario 3: maintenance (dry_run + enable/disable) -----------------

#[test]
fn admin_maintenance_flows() {
    let srv = TestServer::start();

    // dry_run=true with enable=true -> 200, would_enable=true.
    let r = srv.post_json("/admin/v1/maintenance?dry_run=true", br#"{"enable":true}"#);
    r.assert_status(200);
    let j = r.json();
    assert_eq!(j.obj_get("dry_run").and_then(|x| x.as_bool()), Some(true));
    assert_eq!(
        j.obj_get("would_enable").and_then(|x| x.as_bool()),
        Some(true)
    );

    // dry_run=true with enable=false -> 200, would_enable=false.
    let r = srv.post_json("/admin/v1/maintenance?dry_run=true", br#"{"enable":false}"#);
    r.assert_status(200);
    let j = r.json();
    assert_eq!(
        j.obj_get("would_enable").and_then(|x| x.as_bool()),
        Some(false)
    );

    // enable=true -> maintenance flag on.
    let r = srv.post_json(
        "/admin/v1/maintenance",
        br#"{"enable":true,"message":"scheduled"}"#,
    );
    r.assert_status(200);
    let j = r.json();
    assert_eq!(
        j.obj_get("maintenance").and_then(|x| x.as_bool()),
        Some(true)
    );
    assert_eq!(
        j.obj_get("message").and_then(|x| x.as_str()),
        Some("scheduled")
    );
    assert!(j.obj_get("enabled_at").and_then(|x| x.as_str()).is_some());

    // enable=false -> maintenance flag off.
    let r = srv.post_json("/admin/v1/maintenance", br#"{"enable":false}"#);
    r.assert_status(200);
    let j = r.json();
    assert_eq!(
        j.obj_get("maintenance").and_then(|x| x.as_bool()),
        Some(false)
    );
}

// -- Scenario 5: storage migrate (validation 400 + 200) -----------------

#[test]
fn admin_storage_migrate_validation() {
    let srv = TestServer::start();

    // Empty reason -> 400.
    let r = srv.post_json(
        "/admin/v1/storage/migrate",
        br#"{"study_uid":"1.2.3","target_tier":"cold","reason":""}"#,
    );
    r.assert_status(400);
    assert_eq!(
        r.json().obj_get("error").and_then(|x| x.as_str()),
        Some("Bad Request")
    );

    // Invalid tier -> 400.
    let r = srv.post_json(
        "/admin/v1/storage/migrate",
        br#"{"study_uid":"1.2.3","target_tier":"hdd","reason":"test"}"#,
    );
    r.assert_status(400);
    assert_eq!(
        r.json().obj_get("error").and_then(|x| x.as_str()),
        Some("Bad Request")
    );

    // Valid migrate (hot always available) -> 200.
    let r = srv.post_json(
        "/admin/v1/storage/migrate",
        br#"{"study_uid":"1.2.3","target_tier":"HOT","reason":"test"}"#,
    );
    r.assert_status(200);
    let j = r.json();
    assert_eq!(
        j.obj_get("target_tier").and_then(|x| x.as_str()),
        Some("hot")
    );
    assert!(
        j.obj_get("available_backends")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().any(|v| v.as_str() == Some("hot")))
            .unwrap_or(false)
    );
}

// -- Scenario 6: CAS audit dry-run -> 200 --------------------------------

#[test]
fn admin_cas_audit_dry_run() {
    let srv = TestServer::start();

    let r = srv.post_empty("/admin/v1/storage/cas-audit?dry_run=true");
    r.assert_status(200);
    let j = r.json();
    assert_eq!(j.obj_get("dry_run").and_then(|x| x.as_bool()), Some(true));
    assert_eq!(
        j.obj_get("message").and_then(|x| x.as_str()),
        Some("CAS audit would verify all files in store/")
    );
    assert!(j.obj_get("would_check").and_then(|x| x.as_u64()).is_some());
}

// -- Scenario 7: logs/stream SSE (snapshot then poll) -------------------

#[test]
fn admin_logs_stream_snapshot_then_poll() {
    let srv = TestServer::start();

    // Seed the ring buffer BEFORE the stream opens -> snapshot phase.
    srv.push_log("INFO", "stow", "seed log entry", Some("seed"));

    let out = Arc::new(StdRwLock::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));

    // Open stream with default filter (level INFO, component all).
    let guard = srv.open_sse("", out.clone(), done.clone());

    // Push a new log AFTER the stream opened -> poll phase.
    std::thread::sleep(Duration::from_millis(200));
    srv.push_log("INFO", "system", "post-open log entry", None);

    // Give the reader time to drain the snapshot + poll events.
    std::thread::sleep(Duration::from_millis(1200));
    guard.close();

    let raw = out.read().unwrap().clone();
    let raw_str = String::from_utf8_lossy(&raw);

    // The stream is chunked; assert the SSE data JSON payloads appear.
    // Snapshot entry.
    assert!(
        raw_str.contains("seed log entry"),
        "snapshot phase must emit buffered log (frozen)"
    );
    // Polled entry after open.
    assert!(
        raw_str.contains("post-open log entry"),
        "poll phase must emit new log entries (frozen)"
    );
    // Empty polls emit a tick comment.
    assert!(
        raw_str.contains(": tick"),
        "logs stream must emit : tick comment on empty poll (frozen)"
    );
}

/// Probe that the SSE keep-alive framing and data JSON shape are stable.
#[test]
fn admin_logs_stream_json_shape() {
    let srv = TestServer::start();
    srv.push_log("INFO", "stow", "shape log", Some("shape"));

    let out = Arc::new(StdRwLock::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    let guard = srv.open_sse("", out.clone(), done.clone());
    std::thread::sleep(Duration::from_millis(600));
    guard.close();

    let raw = out.read().unwrap().clone();
    let raw_str = String::from_utf8_lossy(&raw);

    // The SSE data: line carries the serialized LogEntry JSON with the frozen
    // field set (timestamp, level, component, message, tag).
    let data_line = "data: {\"timestamp\":\"2026-08-13T00:00:00Z\",\"level\":\"INFO\",\"component\":\"stow\",\"message\":\"shape log\",\"tag\":\"shape\"}";
    assert!(
        raw_str.contains(data_line),
        "SSE data JSON shape (frozen), got: {raw_str:?}"
    );
}
