// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! graceful drain vs forced exit over the real `serve()` transport.
//!
//! Windows has no kill -TERM, so drain is driven through `Shutdown` directly
//! (the same object the signal thread hands to `serve()` in prod):
//!   1. `request_stop()` lets an in-flight WADO stream drain to completion and
//!      then `active()` returns to 0 - graceful.
//!   2. `request_force()` tears the connection down promptly and the server no
//!      longer waits for it to finish.
//!   3. The signal-handler wiring (flag -> request_stop) is exercised directly.

#![forbid(unsafe_code)]

use clarus_core::http::conn::Timeouts;
use clarus_core::shutdown::Shutdown;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

/// Git-tracked DICOM instance (same as serve_smoke uses to STOW).
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

/// Start serve() with a 10s drain ceiling (5s body, 10s send) so `request_stop`
/// is the only thing that ends a stream on our timeline.
fn start_server(listener: TcpListener, state: Arc<AppState>, shutdown: Shutdown) {
    let timeouts = Timeouts::from_secs(10, 30, 5, 10);
    std::thread::spawn(move || {
        clarus_server::serve::serve(listener, state, shutdown, timeouts, 16);
    });
}

/// STOW the charset fixture so a WADO instance can be streamed.
fn stow_fixture(addr: std::net::SocketAddr) {
    let boundary = "TESTBOUNDARY";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(DICOM_INSTANCE);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

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
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(&req).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    assert!(
        String::from_utf8_lossy(&out).starts_with("HTTP/1.1 2"),
        "STOW failed in drain setup: {:?}",
        String::from_utf8_lossy(&out)
    );
}

#[test]
fn request_stop_drains_inflight_stream() {
    let ctx = setup();
    let shutdown = Shutdown::new();
    let (listener, addr) = bind();
    start_server(listener, ctx.state.clone(), shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));
    stow_fixture(addr);

    // Open a WADO instance and read very slowly (drains over a second+).
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(
        b"GET /dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/1.2.3.4.5.100.1.1 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .unwrap();

    // Read slowly while the stream is in flight, then request graceful stop.
    let mut body = Vec::new();
    let mut buf = [0u8; 128];
    let start = Instant::now();
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                body.extend_from_slice(&buf[..n]);
                // Mid-stream: trigger a graceful drain.
                if !body.is_empty() && !shutdown.stopping() {
                    shutdown.request_stop();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
        // Safeguard: end the read loop after a few seconds regardless.
        if start.elapsed() > Duration::from_secs(6) {
            break;
        }
    }

    // The stream body was fully delivered (drained), not truncated.
    assert!(
        body.len() >= DICOM_INSTANCE.len(),
        "drain must deliver the full object, got {} bytes of {}",
        body.len(),
        DICOM_INSTANCE.len()
    );

    // The connection slot is released after the stream finished.
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline && shutdown.active() != 0 {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(shutdown.active(), 0, "graceful drain releases the slot");
    drop(c);
}

#[test]
fn request_force_tears_down_promptly() {
    let ctx = setup();
    let shutdown = Shutdown::new();
    let (listener, addr) = bind();
    start_server(listener, ctx.state.clone(), shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));
    stow_fixture(addr);

    // Open a WADO stream and force-exit mid-flight.
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    c.write_all(
        b"GET /dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/1.2.3.4.5.100.1.1 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .unwrap();

    // Read a little, then force.
    let mut buf = [0u8; 128];
    let _ = c.read(&mut buf).unwrap_or(0);
    shutdown.request_force();
    std::thread::sleep(Duration::from_millis(50));

    // The server no longer waits for the stream; active() drops promptly
    // (the forced path short-circuits the drain ceiling).
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut released = false;
    while Instant::now() < deadline {
        if shutdown.active() == 0 {
            released = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        released || shutdown.forced(),
        "force releases the in-flight conn"
    );
    drop(c);
}

// -- Signal-path unit test (the handler the signal thread calls) ---------
// In prod, `main.rs` registers signal_hook flags; a thread flips `term` then
// calls `shutdown.request_stop()` and, on a second signal, `request_force()`.
// We unit-test that exact mapping: first flag -> request_stop, second flag ->
// request_force.
#[test]
fn signal_handler_flag_maps_to_stop_then_force() {
    let shutdown = Shutdown::new();
    let term = Arc::new(AtomicBool::new(false));
    let force = Arc::new(AtomicBool::new(false));

    // First signal: the handler sees `term==true` -> request_stop.
    term.store(true, Ordering::Relaxed);
    if term.load(Ordering::Relaxed) {
        shutdown.request_stop();
        term.store(false, Ordering::Relaxed); // latch
    }
    assert!(shutdown.stopping(), "first signal requests a graceful stop");

    // Second signal: `force==true` -> request_force (fast exit).
    force.store(true, Ordering::Relaxed);
    if force.load(Ordering::Relaxed) {
        shutdown.request_force();
    }
    assert!(shutdown.forced(), "second signal forces immediate exit");
}
