// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! S31: keep-alive must not outlive a request whose body was never read.
//!
//! A handler that refuses a request before touching its payload (oversized
//! body, early policy rejection) leaves those bytes on the socket. The next
//! `read_head` would then parse body bytes as a request line - the classic
//! request-smuggling desync of PS3.18 8.1. The connection must close instead
//! of being reused.

#![forbid(unsafe_code)]

use clarus_core::http::conn::Timeouts;
use clarus_core::shutdown::Shutdown;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::{Duration, Instant};

struct TestCtx {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
}

fn setup(max_json_body: u64) -> TestCtx {
    let temp = tempfile::tempdir().expect("temp dir");

    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&idx_dir).unwrap();

    let mut config = Config::test_default();
    config.max_json_body = max_json_body;
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

    TestCtx {
        _temp: temp,
        state: Arc::new(state),
    }
}

fn bind() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

fn start_server(
    ctx: &TestCtx,
    listener: TcpListener,
    shutdown: Shutdown,
) -> std::thread::JoinHandle<()> {
    let state = Arc::clone(&ctx.state);
    std::thread::spawn(move || {
        clarus_server::serve::serve(
            listener,
            state,
            shutdown,
            Timeouts::from_secs(10, 30, 5, 60),
            16,
        );
    })
}

/// A request whose body is refused before it is drained must not be answered
/// with a reusable connection: the stream is no longer aligned to a request
/// line.
#[test]
fn an_undrained_body_closes_the_connection_instead_of_reusing_it() {
    // 1 KiB cap: the refusal happens before a single body byte is drained,
    // which is exactly the state that leaves the stream out of sync.
    let ctx = setup(1024);
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(&ctx, listener, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    let body = vec![b'{'; 8192];
    let mut req = Vec::new();
    req.extend_from_slice(
        format!(
            "POST /dicomweb/workitems HTTP/1.1\r\nHost: x\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&body);

    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(&req).expect("write");
    let mut resp = Vec::new();
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = s.read_to_end(&mut resp);
    let head = String::from_utf8_lossy(&resp);

    // The route refuses the missing `?search` marker before reading anything,
    // so the client gets a 4xx and none of the 8 KiB body is drained.
    assert!(
        head.starts_with("HTTP/1.1 4"),
        "expected a refusal, got: {head}"
    );
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "a request with an undrained body must close the connection: {head}"
    );

    drop(s);
    shutdown.request_stop();
    let _ = handle.join();
}

/// The mirror case: a request with no body keeps the connection. Without this
/// the test above would pass on a server that closed everything unconditionally.
#[test]
fn a_request_without_a_body_keeps_the_connection_alive() {
    let ctx = setup(1024);
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(&ctx, listener, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("write");
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).expect("read");
    let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();

    assert!(head.starts_with("http/1.1 200"), "got: {head}");
    assert!(
        !head.contains("connection: close"),
        "/health must stay keep-alive: {head}"
    );

    drop(s);
    shutdown.request_stop();
    let _ = handle.join();
}
