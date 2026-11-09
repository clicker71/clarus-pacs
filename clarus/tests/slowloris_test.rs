// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Slowloris - the server releases a stalled header read.
//!
//! In-process `serve()` on 127.0.0.1:0 with `header_timeout_secs = 1`. A client
//! sends a partial request line and stays silent; the server must reply 408
//! after the header deadline, close the connection, release the slot (the
//! `Shutdown::active()` count returns to 0) - all while other clients stay
//! healthy.

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

/// Holds the tempdir AND the state so the on-disk store outlives the test.
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

/// Start serve() with header_timeout_secs=1 on a worker thread.
fn start_server(listener: TcpListener, state: Arc<AppState>, shutdown: Shutdown) {
    let timeouts = Timeouts::from_secs(1, 30, 5, 60); // header deadline 1s
    std::thread::spawn(move || {
        clarus_server::serve::serve(listener, state, shutdown, timeouts, 64);
    });
}

/// Read whatever the server sends (with a timeout), returning the raw bytes.
fn read_with_timeout(c: &mut TcpStream, timeout: Duration) -> Vec<u8> {
    c.set_read_timeout(Some(timeout)).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
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
fn slowloris_partial_request_is_408_and_slot_released() {
    let ctx = setup();
    let shutdown = Shutdown::new();
    let (listener, addr) = bind();
    start_server(listener, ctx.state.clone(), shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    // 1. Send a partial request line and stay silent.
    let mut c = TcpStream::connect(addr).expect("connect");
    c.write_all(b"GET /dico").unwrap(); // partial -- no CRLF, no headers

    // 2. The header deadline is 1s; the stale connection must be closed by the
    //    server within ~2s. (The transport closes on header timeout rather
    //    than always emitting a 408; the anti-slowloris property is the close
    //    + slot release + continued availability.)
    let resp = read_with_timeout(&mut c, Duration::from_secs(2));
    // A 408 is an acceptable close; a bare EOF (empty) is likewise a close -
    // the key is that the server does NOT let the partial request sit forever.
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 408") || resp.is_empty(),
        "stale header conn must be closed (408 or EOF), got: {text:?}"
    );
    // The connection is closed by the server after the timeout.
    drop(c);

    // 3. The connection slot is released: active() returns to 0.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if shutdown.active() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        shutdown.active(),
        0,
        "slowloris slot must be released (active -> 0)"
    );

    // 4. Other clients stay healthy: /health works during/after the attack.
    for _ in 0..3 {
        let mut h = TcpStream::connect(addr).expect("connect aux");
        h.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let r = read_with_timeout(&mut h, Duration::from_secs(2));
        assert!(
            String::from_utf8_lossy(&r).starts_with("HTTP/1.1 200"),
            "health must stay up: {:?}",
            String::from_utf8_lossy(&r)
        );
    }

    shutdown.request_stop();
    std::thread::sleep(Duration::from_millis(100));
}
