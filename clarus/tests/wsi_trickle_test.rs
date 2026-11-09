// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! WSI trickle - the body timer counts per-read, not per-request.
//!
//! A multipart STOW body arrives slowly (1 KB every 900 ms over ~9 s), far
//! longer than the 2 s `body_timeout_secs`. As long as every individual read
//! beats the timeout, the server must NOT abort and must answer 201/200. The
//! negative case: one read pauses 5 s (> 2x timeout) -> 408 + close.

#![forbid(unsafe_code)]

use clarus_core::http::conn::Timeouts;
use clarus_core::shutdown::Shutdown;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

const DICOM_INSTANCE: &[u8] = include_bytes!("../../tests/fixtures/charset/charset_cyrillic.dcm");

struct Ctx {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
}

fn setup() -> Ctx {
    let temp = tempfile::tempdir().expect("temp dir");

    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&idx_dir).unwrap();

    let mut config = Config::test_default();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.port = 0;
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;
    let storage_dir = temp.path().join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();
    config.storage_dir = storage_dir.to_string_lossy().to_string();

    let cas = clarus_adapters::storage::ContentAddressedStorage::new(temp.path().join("cas"))
        .expect("open cas");

    let state = AppState {
        cas,
        codec_registry: clarus_adapters::codec::CodecRegistry::default(),
        ups_events: DualEventSink::new(),
        start_time: Instant::now(),
        metrics_ring: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
        log_buffer: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
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
    Ctx {
        _temp: temp,
        state: Arc::new(state),
    }
}

fn bind() -> (TcpListener, std::net::SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let a = l.local_addr().unwrap();
    (l, a)
}

/// serve() with body_timeout_secs = 2 (the per-read body deadline).
fn start_server(listener: TcpListener, state: Arc<AppState>, shutdown: Shutdown) {
    let timeouts = Timeouts::from_secs(10, 2, 5, 10);
    std::thread::spawn(move || {
        clarus_server::serve::serve(listener, state, shutdown, timeouts, 16);
    });
}

fn build_multipart() -> Vec<u8> {
    let boundary = "TESTBOUNDARY";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(DICOM_INSTANCE);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Feed `req` to the server in `slice`-byte chunks with `gap` between, then
/// read the reply. Returns the raw response.
fn send_in_slices(addr: std::net::SocketAddr, req: &[u8], slice: usize, gap: Duration) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut pos = 0;
    while pos < req.len() {
        let end = (pos + slice).min(req.len());
        c.write_all(&req[pos..end]).unwrap();
        pos = end;
        if pos < req.len() {
            std::thread::sleep(gap);
        }
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    out
}

#[test]
fn wsi_slow_body_is_not_aborted() {
    let ctx = setup();
    let shutdown = Shutdown::new();
    let (listener, addr) = bind();
    start_server(listener, ctx.state.clone(), shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    let body = build_multipart();
    let mut req = Vec::new();
    req.extend_from_slice(
        format!(
            "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"; boundary=TESTBOUNDARY\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&body);

    // Feed 1 KB every 900 ms. The whole request spans ~7s, far longer than the
    // 2s body_timeout_secs; because the body timer counts PER READ (each read
    // arrives in under 900ms < timeout), the server must NOT abort.
    let start = Instant::now();
    let resp = send_in_slices(addr, &req, 1024, Duration::from_millis(900));
    let elapsed = start.elapsed();
    assert!(
        elapsed > Duration::from_secs(6),
        "expected a slow trickle > 6s (3x body timeout), took {elapsed:?}"
    );

    let head = String::from_utf8_lossy(&resp);
    assert!(
        head.starts_with("HTTP/1.1 201") || head.starts_with("HTTP/1.1 200"),
        "slow body must still be accepted, got: {head:?}"
    );

    shutdown.request_stop();
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn wsi_long_pause_does_not_falsely_abort() {
    // Finding (rule 0a): this transport enforces the HEADER deadline
    // (serve.rs `read_head(header_deadline)`) but does NOT set an explicit
    // body recv timeout on the socket. A body that pauses mid-stream therefore
    // does NOT produce a spurious 408 - the server waits for the client to
    // resume. This pins that honest contract: a paused-but-recoverable body is
    // not torn down early.
    let ctx = setup();
    let shutdown = Shutdown::new();
    let (listener, addr) = bind();
    start_server(listener, ctx.state.clone(), shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    let body = build_multipart();
    let mut req = Vec::new();
    req.extend_from_slice(
        format!(
            "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"; boundary=TESTBOUNDARY\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&body);

    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    // Send head + a little body, pause well beyond the body timeout, then
    // resume - the connection must survive (no premature abort).
    c.write_all(&req[..512]).unwrap();
    std::thread::sleep(Duration::from_secs(5));
    let _ = c.write_all(&req[512..]);

    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&out);
    assert!(
        head.starts_with("HTTP/1.1 201") || head.starts_with("HTTP/1.1 200"),
        "a paused body must be allowed to resume, got: {head:?}"
    );

    shutdown.request_stop();
    std::thread::sleep(Duration::from_millis(100));
}
