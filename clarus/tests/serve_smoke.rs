// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Loopback tests for the synchronous `serve()` transport.
//!
//! A real `TcpListener` on `127.0.0.1:0` is served by `serve()` in a worker
//! thread; clients connect and drive keep-alive, routing errors, WADO close,
//! and the max-connections counter.

#![forbid(unsafe_code)]

use clarus_core::http::conn::Timeouts;
use clarus_core::shutdown::Shutdown;
use clarus_server::config::Config;
use clarus_server::handlers::json_common::{JsonVal, parse_json};
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

fn setup() -> TestCtx {
    setup_with(|_| {})
}

fn setup_with(mutate: impl FnOnce(&mut Config)) -> TestCtx {
    let temp = tempfile::tempdir().expect("temp dir");

    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&idx_dir).unwrap();

    let mut config = Config::test_default();
    mutate(&mut config);
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

/// Start serving `listener` on a worker thread; returns the shutdown handle.
fn start_server(
    listener: TcpListener,
    ctx: &TestCtx,
    shutdown: Shutdown,
    max_connections: u32,
) -> std::thread::JoinHandle<()> {
    start_server_with_timeouts(
        listener,
        ctx,
        shutdown,
        max_connections,
        Timeouts::from_secs(10, 30, 5, 60),
    )
}

/// Same, with explicit per-phase timeouts (header / body / keepalive / send).
fn start_server_with_timeouts(
    listener: TcpListener,
    ctx: &TestCtx,
    shutdown: Shutdown,
    max_connections: u32,
    timeouts: Timeouts,
) -> std::thread::JoinHandle<()> {
    let state = Arc::clone(&ctx.state);
    std::thread::spawn(move || {
        clarus_server::serve::serve(listener, state, shutdown, timeouts, max_connections);
    })
}

fn bind() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Send one raw request and read the full response head + body.
fn send_raw(addr: std::net::SocketAddr, req: &[u8]) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(req).unwrap();
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
fn serve_health_is_200() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    let raw = send_raw(
        addr,
        b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    shutdown.request_stop();
    let _ = handle.join();

    let head = String::from_utf8_lossy(&raw);
    assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
}

#[test]
fn serve_unknown_path_is_404() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    let raw = send_raw(
        addr,
        b"GET /does/not/exist HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    shutdown.request_stop();
    let _ = handle.join();

    let head = String::from_utf8_lossy(&raw);
    assert!(head.starts_with("HTTP/1.1 404"), "got: {head}");
}

#[test]
fn serve_bad_method_is_405_allow() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    // PATCH is an unknown method for every route -> 405 + Allow.
    let raw = send_raw(
        addr,
        b"PATCH /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    shutdown.request_stop();
    let _ = handle.join();

    let head = String::from_utf8_lossy(&raw);
    assert!(head.starts_with("HTTP/1.1 405"), "got: {head}");
    assert!(head.to_ascii_lowercase().contains("allow:"));
}

#[test]
fn serve_every_response_carries_cors_allow_origin() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    // 200 on a real route, with a cross-origin request as the browser sends it.
    let raw = send_raw(
        addr,
        b"GET /health HTTP/1.1\r\nHost: x\r\nOrigin: http://192.168.1.116:3000\r\nConnection: close\r\n\r\n",
    );
    let head = String::from_utf8_lossy(&raw);
    assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    assert!(
        head.contains("Access-Control-Allow-Origin: *"),
        "permissive ACAO missing: {head}"
    );

    // 404 also carries it (error path goes through the same head builder).
    let raw = send_raw(
        addr,
        b"GET /does/not/exist HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let head = String::from_utf8_lossy(&raw);
    assert!(head.starts_with("HTTP/1.1 404"), "got: {head}");
    assert!(
        head.contains("Access-Control-Allow-Origin: *"),
        "ACAO missing on 404: {head}"
    );

    // QIDO JSON renderer (manual head assembly) also carries it.
    let raw = send_raw(
        addr,
        b"GET /dicomweb/studies HTTP/1.1\r\nHost: x\r\nAccept: application/dicom+json\r\nConnection: close\r\n\r\n",
    );
    let head = String::from_utf8_lossy(&raw);
    assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    assert!(
        head.contains("Access-Control-Allow-Origin: *"),
        "ACAO missing on QIDO JSON: {head}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn serve_options_preflight_is_204() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    // Browser CORS preflight before a cross-origin STOW upload.
    let raw = send_raw(
        addr,
        b"OPTIONS /dicomweb/studies HTTP/1.1\r\nHost: x\r\nOrigin: http://192.168.1.116:3000\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type\r\nConnection: close\r\n\r\n",
    );
    let head = String::from_utf8_lossy(&raw);
    assert!(head.starts_with("HTTP/1.1 204"), "got: {head}");
    assert!(
        head.contains("Access-Control-Allow-Origin: *"),
        "got: {head}"
    );
    assert!(
        head.contains("Access-Control-Allow-Methods:"),
        "got: {head}"
    );
    assert!(
        head.contains("Access-Control-Allow-Headers:"),
        "got: {head}"
    );
    assert!(head.contains("Access-Control-Max-Age:"), "got: {head}");
    assert!(
        head.to_ascii_lowercase().contains("content-length: 0"),
        "got: {head}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn serve_wado_client_close_still_closes() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    // WADO routes are keep-alive since 29.08 (router.rs: wado_study /
    // wado_series / wado_instance = KeepAlive::On), but a client that
    // announces Connection: close still gets one response + close (A1-era
    // behavior; serve.rs:337-338 keep_alive = route On && !connection_close).
    let raw = send_raw(
        addr,
        b"GET /dicomweb/studies/1.2.3/series/4.5/instances/6.7 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    shutdown.request_stop();
    let _ = handle.join();

    let head = String::from_utf8_lossy(&raw);
    assert!(head.starts_with("HTTP/1.1 "), "got: {head}");
    assert!(
        head.contains("Connection: close"),
        "client-requested close must be honored: {head}"
    );
}

#[test]
fn serve_wado_stays_open_without_close_header() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    // No Connection header: the WADO route now keeps the socket open, so a
    // second request on the same stream is served (handler-level 404 for the
    // missing object still honors the route's keep-alive flag).
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(
        b"GET /dicomweb/studies/1.2.3/series/4.5/instances/6.7 HTTP/1.1\r\nHost: x\r\n\r\n",
    )
    .unwrap();
    let r1 = read_one_response(&mut c);
    let h1 = String::from_utf8_lossy(&r1);
    assert!(h1.starts_with("HTTP/1.1 404"), "r1: {h1}");
    assert!(
        !h1.to_ascii_lowercase().contains("connection: close"),
        "WADO keep-alive must not close: {h1}"
    );

    c.write_all(
        b"GET /dicomweb/studies/1.2.3/series/4.5/instances/6.8 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let r2 = read_one_response(&mut c);
    let h2 = String::from_utf8_lossy(&r2);
    assert!(h2.starts_with("HTTP/1.1 404"), "r2: {h2}");

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn keepalive_idle_connection_closed_by_server() {
    // Two-phase idle geometry (serve.rs:203-228): before the first request the
    // deadline is the HEADER timeout; after a completed response it becomes
    // the keep-alive idle timeout. In production keepalive_timeout_secs = 10 s
    // (main.rs:1133) and drain_timeout_secs = 30 s default > idle - an idle
    // keep-alive socket drains within the drain deadline (drain_test.rs
    // geometry unchanged). Here header = 2 s, keepalive = 1 s for a fast test.
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server_with_timeouts(
        listener,
        &ctx,
        shutdown.clone(),
        512,
        Timeouts::from_secs(2, 30, 1, 60),
    );
    std::thread::sleep(Duration::from_millis(50));

    // (a) A connection that never sends a request is closed at the header
    // deadline (the slowloris geometry) - never held forever.
    let mut silent = TcpStream::connect(addr).expect("connect");
    silent
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let t0 = Instant::now();
    let mut buf = [0u8; 16];
    let n = silent.read(&mut buf).unwrap_or(0);
    let dt = t0.elapsed();
    assert_eq!(n, 0, "silent connection must be closed (EOF)");
    assert!(
        dt >= Duration::from_secs(1) && dt < Duration::from_secs(5),
        "closed at the header deadline (2 s), got {dt:?}"
    );

    // (b) After the first response the idle deadline becomes the keep-alive
    // timeout (1 s): the same connection is closed shortly after.
    let mut ka = TcpStream::connect(addr).expect("connect");
    ka.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    ka.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let r1 = read_one_response(&mut ka);
    assert!(
        String::from_utf8_lossy(&r1).starts_with("HTTP/1.1 200"),
        "r1"
    );
    let t0 = Instant::now();
    let n = ka.read(&mut buf).unwrap_or(0);
    let dt = t0.elapsed();
    assert_eq!(n, 0, "idle keep-alive connection must be closed (EOF)");
    assert!(
        dt >= Duration::from_millis(500) && dt < Duration::from_secs(4),
        "closed at the keep-alive deadline (1 s), got {dt:?}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn an_idle_keepalive_connection_sees_shutdown_promptly() {
    // A worker parked in the keep-alive read cannot see the stop flag until the
    // idle deadline expires (10 s in production, see the geometry test above),
    // and the drain waits on that worker. The keep-alive read is therefore
    // sliced and re-checks the flag, so an idle connection must close within
    // the slice (~250 ms), not the full 10 s keep-alive window.
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server_with_timeouts(
        listener,
        &ctx,
        shutdown.clone(),
        512,
        Timeouts::from_secs(2, 30, 10, 60),
    );
    std::thread::sleep(Duration::from_millis(50));

    let mut ka = TcpStream::connect(addr).expect("connect");
    ka.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    ka.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let r1 = read_one_response(&mut ka);
    assert!(
        String::from_utf8_lossy(&r1).starts_with("HTTP/1.1 200"),
        "r1"
    );

    // The connection now idles with a 10 s keep-alive deadline; stop arrives
    // well before it could expire on its own.
    // Let the worker finish the response and park in the keep-alive read;
    // stopping before it gets there would test the accept path, not this one.
    std::thread::sleep(Duration::from_millis(500));
    let t0 = Instant::now();
    shutdown.request_stop();
    let mut buf = [0u8; 16];
    let n = ka.read(&mut buf).unwrap_or(0);
    let dt = t0.elapsed();
    assert_eq!(n, 0, "idle keep-alive connection must be closed (EOF)");
    assert!(
        dt < Duration::from_secs(3),
        "closed promptly after stop, not at the 10 s idle deadline: {dt:?}"
    );

    let _ = handle.join();
}

/// A stop must not cut off a request that has already STARTED to arrive.
/// `Idle(used)` with `used > 0` means part of the head is buffered, so the
/// connection is mid-request, not idle: it is left to finish instead of being
/// closed without an answer.
#[test]
fn a_partial_head_is_not_cut_off_by_shutdown() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server_with_timeouts(
        listener,
        &ctx,
        shutdown.clone(),
        512,
        Timeouts::from_secs(2, 30, 10, 60),
    );
    std::thread::sleep(Duration::from_millis(50));

    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    // A PREFIX of a request line: the worker parks with these bytes buffered.
    c.write_all(b"GET /he").unwrap();
    std::thread::sleep(Duration::from_millis(500));

    shutdown.request_stop();
    // If the stop closes a partially-received head, the socket is gone by the
    // time the head is completed below and the write/read fails. The worker
    // thread's own lifetime says nothing here: `serve` returns on stop once the
    // accept loop exits, independently of a connection still in flight.
    std::thread::sleep(Duration::from_millis(800));

    // Finish the head: the request the client already began is still served.
    c.write_all(b"alth HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("the connection must still be open after a stop");
    let mut buf = [0u8; 4096];
    let n = c.read(&mut buf).expect("read");
    assert!(
        n > 0,
        "a request whose head was already arriving must still be answered"
    );
    let head = String::from_utf8_lossy(&buf[..n]);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "the in-flight request is served, not dropped: {head}"
    );

    let _ = handle.join();
}

/// Read one full HTTP response (head + body per Content-Length) so the
/// connection stays perfectly in sync for a following request.
fn read_one_response(c: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = c.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        // Head terminator present? Then read exactly the declared body length.
        if let Some(sep) = out.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&out[..sep]).to_ascii_lowercase();
            let cl = head
                .lines()
                .find(|l| l.starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let have_body = out.len().saturating_sub(sep + 4);
            if have_body < cl {
                let mut rest = vec![0u8; cl - have_body];
                let mut got = 0;
                while got < rest.len() {
                    let n = c.read(&mut rest[got..]).expect("read body");
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
                out.extend_from_slice(&rest[..got]);
            }
            break;
        }
    }
    out
}

#[test]
fn serve_keep_alive_two_requests_one_connection() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    // health and metrics are KeepAlive::On -> two requests, same TCP stream.
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let r1 = read_one_response(&mut c);
    assert!(
        String::from_utf8_lossy(&r1).starts_with("HTTP/1.1 200"),
        "r1: {:?}",
        r1
    );

    // Send the second request immediately; the connection is still open.
    c.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let r2 = read_one_response(&mut c);
    assert!(
        String::from_utf8_lossy(&r2).starts_with("HTTP/1.1 200"),
        "r2: {:?}",
        r2
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn serve_max_connections_closes_excess() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    // Only 2 simultaneous connections allowed.
    let handle = start_server(listener, &ctx, shutdown.clone(), 2);
    std::thread::sleep(Duration::from_millis(50));

    // Open 5 connections and send nothing: the 3rd+ must be closed without
    // reading (EOF) because the active counter exceeds max_connections.
    let mut conns = Vec::new();
    for _ in 0..5 {
        conns.push(TcpStream::connect(addr).expect("connect"));
    }
    std::thread::sleep(Duration::from_millis(200));

    // Count how many sockets still accept a write without an error + are open.
    let mut served = 0;
    for c in &mut conns {
        c.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut buf = [0u8; 16];
        // Write a request; a served socket replies (read > 0), a rejected
        // socket is closed without reading -> EOF (read returns 0).
        let _ = c.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n");
        let got = c.read(&mut buf).unwrap_or(0);
        if got > 0 {
            served += 1;
        }
    }
    shutdown.request_stop();
    let _ = handle.join();

    // At most 2 connections were served; the rest were rejected at accept time.
    assert!(served <= 2, "expected at most 2 served, got {served}");
}

/// Drive a burst ("pack") of keep-alive `/health` requests over one
/// connection. Returns how many returned `200 OK`; used to prove that a
/// saturated server answers *every* accepted request (none silently dropped).
fn drain_health_batch(addr: std::net::SocketAddr, per_conn: usize) -> usize {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut ok = 0;
    for _ in 0..per_conn {
        c.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        // Every accepted request must get a response; a flushed request that
        // produced no reply is a lost (silently dropped) request.
        let r = read_one_response(&mut c);
        if String::from_utf8_lossy(&r).starts_with("HTTP/1.1 200") {
            ok += 1;
        }
    }
    ok
}

#[test]
fn serve_saturation_tomograph_pack_others_not_lost() {
    // saturation: one "tomograph" floods a sized pack of
    // keep-alive requests across several connections, while independent
    // "other" clients issue requests concurrently. Under the connection cap
    // the transport must serve *every accepted* request - none lost.
    //
    // The 507 (insufficient-storage) and disk-probe (DISK_DEGRADED) gates are
    // not reachable here: `preflight_disk` is currently uncalled, and the 507
    // ENOSPC / `DISK_DEGRADED` (>30s probe) triggers are non-deterministic in
    // an integration harness. The deterministic, green-able contract is the
    // transport saturation property asserted below.
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    // Generous cap so the tomograph pack + the "other" clients all fit: the
    // property under test is request-kept-under-saturation, not over-cap
    // rejection (that is already covered by serve_max_connections_closes_excess).
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let tomo_conns = 4usize;
    let tomo_per_conn = 20usize;
    let other_conns = 2usize;
    let other_per_conn = 10usize;

    // One tomograph worker floods its whole pack.
    let tomo = std::thread::spawn(move || -> usize {
        let mut served = 0;
        let mut handles = Vec::new();
        for _ in 0..tomo_conns {
            handles.push(std::thread::spawn(move || {
                drain_health_batch(addr, tomo_per_conn)
            }));
        }
        for h in handles {
            served += h.join().expect("tomo conn thread");
        }
        served
    });

    // Concurrent "other" clients must not be lost while the tomograph floods.
    let other_addr = addr;
    let others = std::thread::spawn(move || -> usize {
        let mut served = 0;
        let mut handles = Vec::new();
        for _ in 0..other_conns {
            handles.push(std::thread::spawn(move || {
                drain_health_batch(other_addr, other_per_conn)
            }));
        }
        for h in handles {
            served += h.join().expect("other conn thread");
        }
        served
    });

    let tomo_served = tomo.join().expect("tomo thread");
    let other_served = others.join().expect("other thread");

    shutdown.request_stop();
    let _ = handle.join();

    let expected_tomo = tomo_conns * tomo_per_conn;
    let expected_other = other_conns * other_per_conn;
    // The tomograph's pack must be served in full (no request dropped)...
    assert_eq!(
        tomo_served, expected_tomo,
        "tomograph lost requests: served {tomo_served}/{expected_tomo}"
    );
    // ...and the concurrent "other" clients' requests survived the flood.
    assert_eq!(
        other_served, expected_other,
        "other clients lost requests: served {other_served}/{expected_other}"
    );
}

#[test]
fn serve_proxied_identity_from_trusted_loopback_is_200() {
    // Loopback peer is in default trusted_proxies (127.0.0.0/8); X-Real-IP and
    // X-Authenticated-User are honored and the request proceeds (E-009).
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let req = b"GET /health HTTP/1.1\r\nHost: x\r\nX-Real-IP: 10.9.9.9\r\nX-Authenticated-User: dr-smith\r\nConnection: close\r\n\r\n";
    let resp = send_raw(addr, req);
    let head = String::from_utf8_lossy(&resp);
    assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn serve_missing_identity_strict_is_502() {
    // require_proxy_identity = true: a trusted peer that delivered X-Real-IP
    // but no X-Authenticated-User is a nginx misconfiguration -> 502.
    let ctx = setup_with(|c| c.require_proxy_identity = true);
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let req =
        b"GET /health HTTP/1.1\r\nHost: x\r\nX-Real-IP: 10.9.9.9\r\nConnection: close\r\n\r\n";
    let resp = send_raw(addr, req);
    let head = String::from_utf8_lossy(&resp);
    assert!(head.starts_with("HTTP/1.1 502"), "got: {head}");

    // LocalViewer (trusted peer, zero proxy headers) stays exempt: 200.
    let req2 = b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp2 = send_raw(addr, req2);
    let head2 = String::from_utf8_lossy(&resp2);
    assert!(head2.starts_with("HTTP/1.1 200"), "got: {head2}");

    shutdown.request_stop();
    let _ = handle.join();
}

// -- STOW over the real socket --------------------------------------------
// The head reader (8 KB buffer) can read body bytes into the head buffer in
// the same segment(s) as the request head. The body reader must consume those
// prefetched bytes first, or the multipart stream is truncated: RST for large
// bodies ("head too large"), hang for tiny ones. Live-wire regression tests.

/// Git-tracked charset-tagged DICOM instance (~8.8 KB - larger than the head
/// buffer, so the multipart body always spans the first head read).
const DICOM_INSTANCE: &[u8] = include_bytes!("../../tests/fixtures/charset/charset_cyrillic.dcm");

fn stow_multipart_body() -> Vec<u8> {
    let boundary = "TESTBOUNDARY";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(DICOM_INSTANCE);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[test]
fn serve_stow_multipart_content_length_prefetched_body() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let body = stow_multipart_body();
    let mut req = Vec::new();
    req.extend_from_slice(
        format!(
            "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"; boundary=TESTBOUNDARY\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&body);

    let resp = send_raw(addr, &req);
    shutdown.request_stop();
    let _ = handle.join();

    let head = String::from_utf8_lossy(&resp);
    assert!(head.starts_with("HTTP/1.1 2"), "got: {head}");
    let sep = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head terminator in response");
    let payload = String::from_utf8_lossy(&resp[sep + 4..]);
    let json = parse_json(payload.trim().as_bytes()).expect("valid STOW JSON body");
    assert!(
        json.obj_get("00081199").is_some(),
        "STOW response must carry ReferencedSOPSequence"
    );
}

#[test]
fn serve_stow_multipart_chunked_prefetched_body() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let body = stow_multipart_body();
    let mut req = Vec::new();
    req.extend_from_slice(
        b"POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"; boundary=TESTBOUNDARY\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    );
    req.extend_from_slice(format!("{:X}\r\n", body.len()).as_bytes());
    req.extend_from_slice(&body);
    req.extend_from_slice(b"\r\n0\r\n\r\n");

    let resp = send_raw(addr, &req);
    shutdown.request_stop();
    let _ = handle.join();

    let head = String::from_utf8_lossy(&resp);
    assert!(head.starts_with("HTTP/1.1 2"), "got: {head}");
    let sep = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head terminator in response");
    let payload = String::from_utf8_lossy(&resp[sep + 4..]);
    let json = parse_json(payload.trim().as_bytes()).expect("valid STOW JSON body");
    assert!(json.obj_get("00081199").is_some(), "got");
}

// -- Chunked == Content-Length parity for the buffered JSON routes -------
//
// `read_body_limited` (dispatch.rs:190) serves every buffered JSON POST, and it
// decides framing through `body::framing` -- chunked whenever a
// `Transfer-Encoding` header is present. The STOW path has had a chunked
// regression test since the prefetch fixes; the buffered route class had none,
// which is why a stale deployed build could answer 400 to a chunked Create
// without any test going red. These tests pin the parity per route family, so
// the class is guarded rather than a single route.
//
// ap101: N/A. `read_body_limited` allocates the whole JSON body into a `Vec` by
// design; it is not on the gated zero-allocation path (`ap101_http_body` gates
// the streaming decoder in clarus-core, which these tests do not reach).

/// Send `body` to `path` twice on fresh connections: once framed with
/// `Content-Length`, once with `Transfer-Encoding: chunked`. Returns the two
/// raw responses.
///
/// The chunk framing is byte-identical to what
/// `serve_stow_multipart_chunked_prefetched_body` puts on the wire:
/// `<hex len>\r\n<body>\r\n0\r\n\r\n`, i.e. exactly one chunk plus the
/// terminator, which is what a client that buffers a small JSON body sends.
fn post_both_framings(addr: std::net::SocketAddr, path: &str, body: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut len_req = Vec::new();
    len_req.extend_from_slice(
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nAccept: application/dicom+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    len_req.extend_from_slice(body);

    let mut chunked_req = Vec::new();
    chunked_req.extend_from_slice(
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nAccept: application/dicom+json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    );
    chunked_req.extend_from_slice(format!("{:X}\r\n", body.len()).as_bytes());
    chunked_req.extend_from_slice(body);
    chunked_req.extend_from_slice(b"\r\n0\r\n\r\n");

    let by_len = send_raw(addr, &len_req);
    let chunked = send_raw(addr, &chunked_req);
    (by_len, chunked)
}

/// Send one raw request against a FRESH server instance and return the reply.
///
/// For the routes whose answer depends on server state (a Create is one-shot
/// per UID, a Commit's Transaction UID is single-use), running the two framing
/// legs on one instance would compare more than the framing.
fn run_isolated(req: &[u8]) -> Vec<u8> {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));
    let resp = send_raw(addr, req);
    shutdown.request_stop();
    let _ = handle.join();
    resp
}

/// Split a raw response into (status line, body bytes).
fn split_status_and_body(resp: &[u8]) -> (String, Vec<u8>) {
    let sep = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head terminator in response: {resp:?}"));
    let head = String::from_utf8_lossy(&resp[..sep]).to_string();
    let status = head.lines().next().unwrap_or("").to_string();
    (status, resp[sep + 4..].to_vec())
}

/// Both framings must reach the handler with the same bytes and produce the
/// same answer. A framing bug shows up as a 4xx on one leg, so asserting the
/// status alone would already catch it; the body comparison additionally
/// catches a truncated read that the handler happened to tolerate.
fn assert_framing_parity(addr: std::net::SocketAddr, path: &str, body: &[u8], what: &str) {
    let (by_len, chunked) = post_both_framings(addr, path, body);
    let (len_status, len_body) = split_status_and_body(&by_len);
    let (chunk_status, chunk_body) = split_status_and_body(&chunked);
    assert!(
        len_status.starts_with("HTTP/1.1 2"),
        "{what}: Content-Length leg must succeed, got {len_status}: {}",
        String::from_utf8_lossy(&len_body)
    );
    assert_eq!(
        chunk_status,
        len_status,
        "{what}: chunked status differs from Content-Length (body: {})",
        String::from_utf8_lossy(&chunk_body)
    );
    assert_eq!(
        chunk_body, len_body,
        "{what}: chunked body differs from Content-Length"
    );
}

/// UPS Create (`POST /workitems?workitem={uid}`) - the exact route class the
/// 20-21.10 field test hit.
///
/// Each leg gets its OWN server. A Create is one-shot for a given UID, and the
/// response body echoes that UID (0008,0018), so reusing one server would make
/// the two legs differ for reasons that have nothing to do with framing. Same
/// UID, same body, two fresh instances: the only variable left is the transfer
/// framing, which is what the test is about.
#[test]
fn serve_chunked_equals_content_length_for_ups_create() {
    // The type-1 set of PS3.4 Table CC.2.5-3 (no Modality: UPS defines none).
    let body = br#"{"00404005":{"vr":"DT","Value":["20260917T103000"]},
          "00404041":{"vr":"CS","Value":["READY"]},
          "00741200":{"vr":"CS","Value":["MEDIUM"]},
          "00741204":{"vr":"SH","Value":["CAD"]}}"#;
    const PATH: &str = "/dicomweb/workitems?workitem=1.2.3.4.5.100";

    let mut len_req = Vec::new();
    len_req.extend_from_slice(
        format!(
            "POST {PATH} HTTP/1.1\r\nHost: x\r\nContent-Type: application/dicom+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    len_req.extend_from_slice(body);

    let mut chunked_req = Vec::new();
    chunked_req.extend_from_slice(
        format!(
            "POST {PATH} HTTP/1.1\r\nHost: x\r\nContent-Type: application/dicom+json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    );
    chunked_req.extend_from_slice(format!("{:X}\r\n", body.len()).as_bytes());
    chunked_req.extend_from_slice(body);
    chunked_req.extend_from_slice(b"\r\n0\r\n\r\n");

    let by_len = run_isolated(&len_req);
    let chunked = run_isolated(&chunked_req);

    let (len_status, len_body) = split_status_and_body(&by_len);
    let (chunk_status, chunk_body) = split_status_and_body(&chunked);
    assert_eq!(
        len_status,
        "HTTP/1.1 201 Created",
        "Content-Length Create must be 201, got {len_status}: {}",
        String::from_utf8_lossy(&len_body)
    );
    assert_eq!(
        chunk_status,
        len_status,
        "chunked Create must be 201 too, got {chunk_status}: {}",
        String::from_utf8_lossy(&chunk_body)
    );
    assert_eq!(
        chunk_body, len_body,
        "the Create response body must not depend on the request framing"
    );
}

/// UPS Search-with-body (`POST /workitems?search`) shares `read_body_limited`.
#[test]
fn serve_chunked_equals_content_length_for_ups_search() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    // An empty matching dataset matches everything: the response is the same
    // regardless of state, so the two legs are directly comparable.
    assert_framing_parity(addr, "/dicomweb/workitems?search", b"{}", "UPS Search");

    shutdown.request_stop();
    let _ = handle.join();
}

/// Storage Commitment Commit (`POST /commitment-requests/{uid}`) shares it too.
#[test]
fn serve_chunked_equals_content_length_for_commitment_commit() {
    // Per PS3.18 13.4 the body is the ReferencedSOPSequence of what to commit.
    // A Transaction UID is single-use and the commit is stateful, so the two
    // legs run against separate servers for the same reason as UPS Create.
    let body = br#"{"00081199":{"vr":"SQ","Value":[{"00081150":{"vr":"UI","Value":["1.2.840.10008.5.1.4.1.1.2"]},"00081155":{"vr":"UI","Value":["1.2.3.4.5.100.1"]}}]}}"#;
    const PATH: &str = "/dicomweb/commitment-requests/1.2.3.4.5.200";

    let mut len_req = Vec::new();
    len_req.extend_from_slice(
        format!(
            "POST {PATH} HTTP/1.1\r\nHost: x\r\nAccept: application/dicom+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    len_req.extend_from_slice(body);

    let mut chunked_req = Vec::new();
    chunked_req.extend_from_slice(
        format!(
            "POST {PATH} HTTP/1.1\r\nHost: x\r\nAccept: application/dicom+json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    );
    chunked_req.extend_from_slice(format!("{:X}\r\n", body.len()).as_bytes());
    chunked_req.extend_from_slice(body);
    chunked_req.extend_from_slice(b"\r\n0\r\n\r\n");

    let by_len = run_isolated(&len_req);
    let chunked = run_isolated(&chunked_req);
    let (len_status, len_body) = split_status_and_body(&by_len);
    let (chunk_status, chunk_body) = split_status_and_body(&chunked);
    assert!(
        len_status.starts_with("HTTP/1.1 2"),
        "Content-Length commit leg must succeed, got {len_status}: {}",
        String::from_utf8_lossy(&len_body)
    );
    assert_eq!(
        chunk_status,
        len_status,
        "chunked commit differs from Content-Length (body: {})",
        String::from_utf8_lossy(&chunk_body)
    );
    assert_eq!(
        chunk_body, len_body,
        "the commit response body must not depend on the request framing"
    );
}

/// An admin JSON POST (`/admin/v1/instances/delete`) shares the same reader.
///
/// Both legs are refused (unknown study), which is deliberate: the assertion
/// is that the framing does not change the answer, and a refusal is a stable,
/// side-effect-free answer. The dry-run flag is absent, but an unknown target
/// cannot be deleted, so no erase is scheduled.
#[test]
fn serve_chunked_equals_content_length_for_admin_json_post() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let body = br#"{"study_uid":"1.2.3.4.5.999","reason":"chunked parity probe"}"#;
    let (by_len, chunked) = post_both_framings(addr, "/admin/v1/instances/delete", body);
    let (len_status, len_body) = split_status_and_body(&by_len);
    let (chunk_status, chunk_body) = split_status_and_body(&chunked);
    // A 400 here would be indistinguishable from a framing bug, so the leg
    // must first prove the body was parsed: the handler can only name the
    // reason it was given if it read the JSON.
    assert_ne!(
        len_status,
        "HTTP/1.1 400 Bad Request",
        "the Content-Length leg must parse, not reject: {}",
        String::from_utf8_lossy(&len_body)
    );
    assert_eq!(
        chunk_status,
        len_status,
        "chunked admin POST differs from Content-Length (body: {})",
        String::from_utf8_lossy(&chunk_body)
    );
    assert_eq!(
        chunk_body, len_body,
        "the admin response body must not depend on the request framing"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn serve_stow_tiny_body_gets_400_not_hang() {
    // A ~60-byte multipart body fits entirely inside the first head read.
    // Before the prefetch fix the terminator was only checked at the chunk
    // tail, so the server kept waiting for more head bytes (hang).
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let boundary = "TESTBOUNDARY";
    let body =
        format!("--{boundary}\r\nContent-Type: application/json\r\n\r\n{{}}\r\n--{boundary}--\r\n")
            .into_bytes();
    let mut req = Vec::new();
    req.extend_from_slice(
        format!(
            "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"; boundary=TESTBOUNDARY\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&body);

    let resp = send_raw(addr, &req);
    shutdown.request_stop();
    let _ = handle.join();

    let head = String::from_utf8_lossy(&resp);
    assert!(head.starts_with("HTTP/1.1 400"), "got: {head}");
}

#[test]
fn serve_stow_then_keep_alive_get_stays_in_sync() {
    // STOW is KeepAlive::On: after the prefetched-body fix the connection
    // must remain perfectly in sync for the next request on the same socket.
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let body = stow_multipart_body();
    let mut req = Vec::new();
    req.extend_from_slice(
        format!(
            "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"; boundary=TESTBOUNDARY\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&body);

    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(&req).unwrap();
    let r1 = read_one_response(&mut c);
    assert!(
        String::from_utf8_lossy(&r1).starts_with("HTTP/1.1 2"),
        "r1: {:?}",
        r1
    );

    // Same socket, next request: no leftover body bytes may corrupt the head.
    c.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let r2 = read_one_response(&mut c);
    assert!(
        String::from_utf8_lossy(&r2).starts_with("HTTP/1.1 200"),
        "r2: {:?}",
        r2
    );

    // And the study is queryable end-to-end.
    let qido = send_raw(
        addr,
        b"GET /dicomweb/studies HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let qido_head = String::from_utf8_lossy(&qido);
    assert!(qido_head.starts_with("HTTP/1.1 200"), "qido: {qido_head}");
    assert!(
        qido_head.contains("00100010"),
        "QIDO must return the stored study's PatientName: {qido_head}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn serve_stow_then_miniviewer_walk_is_clean() {
    // End-to-end walk of the exact miniviewer request chain after a STOW:
    // studies (uppercase UID key) -> SearchForSeries -> instances. Two live
    // defects made this fail: the B-tree QIDO writer emitted tag keys in
    // lowercase (`0020000d`), so the viewer looked up an empty study UID and
    // hit BadUid 400; and the sync router had no `/studies/{study}/series`
    // route at all (404).
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    // STOW the charset fixture: study 1.2.3.4.5.100 / series 1.2.3.4.5.100.1.
    let body = stow_multipart_body();
    let mut req = Vec::new();
    req.extend_from_slice(
        format!(
            "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"; boundary=TESTBOUNDARY\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&body);
    let stow = send_raw(addr, &req);
    assert!(
        String::from_utf8_lossy(&stow).starts_with("HTTP/1.1 2"),
        "stow: {}",
        String::from_utf8_lossy(&stow)
    );

    // 1. Study-level QIDO: the StudyInstanceUID key must be the standard
    //    uppercase 0020000D (the viewer looks it up by that exact key).
    let studies = send_raw(
        addr,
        b"GET /dicomweb/studies?PatientID=CYR-TEST-001&limit=50 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let studies_head = String::from_utf8_lossy(&studies);
    assert!(
        studies_head.starts_with("HTTP/1.1 200"),
        "studies qido: {studies_head}"
    );
    assert!(
        studies_head.contains("\"0020000D\""),
        "StudyInstanceUID key must be uppercase per PS3.18: {studies_head}"
    );
    assert!(
        !studies_head.contains("\"0020000d\""),
        "lowercase StudyInstanceUID key breaks DICOMweb consumers: {studies_head}"
    );

    // 2. SearchForSeries: GET /studies/{study}/series (the viewer's next hop).
    let series = send_raw(
        addr,
        b"GET /dicomweb/studies/1.2.3.4.5.100/series HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let series_head = String::from_utf8_lossy(&series);
    assert!(
        series_head.starts_with("HTTP/1.1 200"),
        "series qido: {series_head}"
    );
    assert!(
        series_head.contains("0020000E"),
        "series response must carry SeriesInstanceUID: {series_head}"
    );

    // 3. Instances of the series (next viewer hop after series select).
    let instances = send_raw(
        addr,
        b"GET /dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances?limit=9999 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let instances_head = String::from_utf8_lossy(&instances);
    assert!(
        instances_head.starts_with("HTTP/1.1 200"),
        "instances qido: {instances_head}"
    );
    assert!(
        instances_head.contains("00080018"),
        "instances response must carry SOPInstanceUID: {instances_head}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

/// an exact re-STOW of the same file is a success and
/// MUST answer **200**, not 201 (fresh store) and not 204 (UPS-only semantics).
///
/// A duplicate does not create a DB row (UNIQUE sop_instance_uid) nor a CAS
/// file (BLAKE3 dedup), so QIDO must still show exactly one instance.
#[test]
fn serve_stow_duplicate_is_200_not_204() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 16);
    std::thread::sleep(Duration::from_millis(50));

    let body = stow_multipart_body();
    let mut req = Vec::new();
    req.extend_from_slice(
        format!(
            "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"; boundary=TESTBOUNDARY\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    req.extend_from_slice(&body);

    // First STOW: a fresh instance -> 200 OK (DICOMweb full success is 200,
    // success is always 200, fresh stores included).
    let first = send_raw(addr, &req);
    let first_head = String::from_utf8_lossy(&first);
    assert!(
        first_head.starts_with("HTTP/1.1 200"),
        "first STOW must be 200: {first_head}"
    );

    // Second STOW of the identical bytes: exact duplicate -> 200.
    let second = send_raw(addr, &req);
    let second_text = String::from_utf8_lossy(&second);
    assert!(
        second_text.starts_with("HTTP/1.1 200"),
        "duplicate STOW must be 200: {second_text}"
    );
    assert!(
        second_text.contains("00081155"),
        "200 body must still carry ReferencedSOPSequence/SOPInstanceUID: {second_text}"
    );

    // QIDO: the duplicate must NOT have created a second instance.
    let qido = send_raw(
        addr,
        b"GET /dicomweb/studies HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let qido_text = String::from_utf8_lossy(&qido);
    assert!(qido_text.starts_with("HTTP/1.1 200"), "qido: {qido_text}");
    // Exactly one study with exactly one instance is implied by a single
    // ReferencedSOPSequence earlier; assert the series/instance query returns
    // one element.
    let inst = send_raw(
        addr,
        b"GET /dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances?limit=9999 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let inst_text = String::from_utf8_lossy(&inst);
    assert!(
        inst_text.starts_with("HTTP/1.1 200"),
        "instances qido: {inst_text}"
    );
    let inst_json = parse_json(
        inst_text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or("[]")
            .as_bytes(),
    )
    .unwrap_or(JsonVal::Arr(Vec::new()));
    let n = inst_json
        .as_array()
        .map(|a| a.len())
        .or_else(|| {
            inst_json
                .obj_get("Value")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
        })
        .unwrap_or(0);
    assert_eq!(
        n, 1,
        "duplicate STOW must not duplicate the instance: {inst_text}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

/// The WADO series access aggregator is a pure function: it returns the
/// completed lines instead of writing `clarus.log`, so we assert on that
/// return directly. Three slices of one series (same user/ip/study/series)
/// yield exactly one `loaded` line when the series total is reached; the
/// first slice of the new bucket opens with a `started` line.
#[test]
fn serve_wado_series_aggregator_returns_one_loaded_line() {
    let study = b"1.2.3.4.5.100";
    let series = b"1.2.3.4.5.100.1";
    let user = b"(miniviewer)";
    let ip = b"127.0.0.1";
    let mut lines = Vec::new();
    // Slice 1 opens the bucket (started); slice 2 accumulates silently;
    // slice 3 reaches the total of 3 and emits the single `loaded` line.
    for _ in 0..3 {
        lines.extend(clarus_server::access_agg::record(
            user,
            ip,
            study,
            series,
            200,
            4096,
            1500,
            Some(3),
        ));
    }
    let loaded: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("loaded study=") && l.contains("study=1.2.3.4.5.100"))
        .collect();
    assert_eq!(loaded.len(), 1, "exactly one loaded line: {lines:?}");
    assert!(
        loaded[0].contains("slices=3 of=3"),
        "loaded line carries the totals: {}",
        loaded[0]
    );
    // A 4th slice (repeat of a SOP) no longer completes: the completed bucket
    // was removed, so a fresh session may open (its `started` line) but no
    // further `loaded` line is possible (start markers).
    let more = clarus_server::access_agg::record(user, ip, study, series, 200, 4096, 1500, Some(3));
    assert!(
        more.iter().all(|l| !l.contains("loaded study=")),
        "no loaded line after completion: {more:?}"
    );
}

/// RFC 9110 s10.1.1: with `Expect: 100-continue` and a non-empty body, the
/// server must answer an interim 100 BEFORE reading the body. The interim is
/// written straight to the socket, outside response_bytes.
#[test]
fn serve_expect_100_continue_answers_interim_before_body() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    // Head only - no body in the first packet, so the interim must come first.
    c.write_all(
        b"POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related;boundary=abc\r\nContent-Length: 2\r\nExpect: 100-continue\r\n\r\n",
    )
    .unwrap();
    let interim = read_one_response(&mut c);
    let interim_s = String::from_utf8_lossy(&interim);
    assert!(
        interim_s.starts_with("HTTP/1.1 100"),
        "interim 100 expected before body, got: {interim_s}"
    );
    // Now send the two body bytes and read the final status.
    c.write_all(b"hi").unwrap();
    let final_resp = read_one_response(&mut c);
    let final_s = String::from_utf8_lossy(&final_resp);
    assert!(
        final_s.starts_with("HTTP/1.1 "),
        "final status after body, got: {final_s}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

/// HTTP/1.0 ignores `Expect` (RFC) - the first reply is the final one.
#[test]
fn serve_expect_100_continue_http10_no_interim() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    let raw = send_raw(
        addr,
        b"POST /dicomweb/studies HTTP/1.0\r\nHost: x\r\nContent-Type: multipart/related;boundary=abc\r\nContent-Length: 2\r\nExpect: 100-continue\r\nConnection: close\r\n\r\nhi",
    );
    shutdown.request_stop();
    let _ = handle.join();

    let s = String::from_utf8_lossy(&raw);
    assert!(
        !s.starts_with("HTTP/1.1 100"),
        "HTTP/1.0 must skip the interim: {s}"
    );
    assert!(s.starts_with("HTTP/1.1 "), "final reply expected: {s}");
}

/// Without `Expect` there is no interim - the first reply is the final one.
#[test]
fn serve_no_expect_no_interim() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone(), 512);
    std::thread::sleep(Duration::from_millis(50));

    let raw = send_raw(
        addr,
        b"POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related;boundary=abc\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
    );
    shutdown.request_stop();
    let _ = handle.join();

    let s = String::from_utf8_lossy(&raw);
    assert!(
        !s.starts_with("HTTP/1.1 100"),
        "no Expect -> no interim: {s}"
    );
    assert!(s.starts_with("HTTP/1.1 "), "final reply expected: {s}");
}

/// m10: a raised shutdown flag closes keep-alive connections instead of
/// letting the worker park in the idle window.
///
/// The worker sits in `read_head` for the whole keep-alive idle timeout once a
/// response has been sent. Its socket was never handed to the drain, so a stop
/// had no way to reach it short of that timeout expiring - a Ctrl+C looked
/// like a hang for as long as the window (75 s by default). Here the window is
/// deliberately long, so only the flag can close the connection in time.
#[test]
fn shutdown_closes_a_keep_alive_connection_without_waiting_out_the_idle_window() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    // Idle window far longer than the assertion below: if the worker waited for
    // it, the test would time out rather than pass.
    let timeouts = Timeouts::from_secs(10, 30, 600, 60);
    let handle = start_server_with_timeouts(listener, &ctx, shutdown.clone(), 64, timeouts);

    std::thread::sleep(Duration::from_millis(50));

    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    // A WADO route keeps the socket open, so the worker stays in its request
    // loop after answering - which is where the idle window would trap it.
    c.write_all(
        b"GET /dicomweb/studies/1.2.3/series/4.5/instances/6.7 HTTP/1.1\r\nHost: x\r\n\r\n",
    )
    .unwrap();
    let r1 = read_one_response(&mut c);
    let h1 = String::from_utf8_lossy(&r1);
    assert!(h1.starts_with("HTTP/1.1 404"), "r1: {h1}");
    assert!(
        !h1.to_ascii_lowercase().contains("connection: close"),
        "the connection must be keep-alive before the stop: {h1}"
    );

    // PROVE the worker is back in its idle read before raising the flag.
    //
    // The first response is written from inside `handle_one`; the shutdown
    // check runs AFTER it returns. A stop raised in that window is answered by
    // the ordinary loop-exit path, not by the keep-alive check under test, so
    // the socket closes for a different reason and the test would pass or fail
    // on a race. A SECOND request on the same connection proves the worker has
    // completed a full iteration and is parked in `read_head`.
    c.write_all(
        b"GET /dicomweb/studies/1.2.3/series/4.5/instances/6.8 HTTP/1.1\r\nHost: x\r\n\r\n",
    )
    .unwrap();
    let r2 = read_one_response(&mut c);
    let h2 = String::from_utf8_lossy(&r2);
    assert!(h2.starts_with("HTTP/1.1 404"), "r2: {h2}");

    // The connection is now idle inside the worker's loop. Raise the flag.
    let raised = Instant::now();
    shutdown.request_stop();

    // The next read must return EOF promptly - the worker closed the socket
    // rather than waiting out the idle window for another request.
    let mut tail = [0u8; 256];
    let n = c.read(&mut tail).expect("read after stop");
    let elapsed = raised.elapsed();
    assert_eq!(n, 0, "expected EOF after the stop, got {n} bytes");

    let _ = handle.join();
    assert!(
        elapsed < Duration::from_secs(5),
        "connection lingered for {elapsed:?}; the worker waited out the idle window"
    );
}
