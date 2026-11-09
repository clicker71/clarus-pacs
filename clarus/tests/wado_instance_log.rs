// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A `wado_instance` request must be visible in the log BOTH ways.
//!
//! This file is a single test on purpose: `clog::set_sender` installs a
//! process-wide writer channel once, so a second test here would race it.
//! The assertions are:
//!
//! - the per-request ACCESS line is emitted for a slice (it was not before
//!   2409: the slice route fed only the aggregate, so a request that hung
//!   left no trace until its bucket swept);
//! - the aggregate `WADO_SERIES` lines are STILL emitted, so the fix adds a
//!   line rather than replacing one.
//!
//! RELEASE ONLY (`#![cfg(not(debug_assertions))]` at the foot of the file):
//! in a debug build `clog::emit` writes the record straight to stderr and
//! never pushes it to the writer channel, so there is nothing here to read
//! back. The line FORMAT is pinned in every mode by the `serve::tests`
//! unit tests (`a_slow_slice_is_named_by_its_uids`,
//! `a_slow_request_on_another_route_is_named_by_its_path`); what this file
//! adds is the end-to-end proof that BOTH lines are produced for one real
//! slice request, which needs the release routing.
//!
//! Run it with: cargo test --release -p clarus --test wado_instance_log

#![cfg(not(debug_assertions))]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

const STUDY: &str = "1.2.826.0.1.3680043.10.1234.7100";
const SERIES: &str = "1.2.826.0.1.3680043.10.1234.7200";
const SOP: &str = "1.2.826.0.1.3680043.10.1234.7301";

fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    // DICOM values have an EVEN length; a UI is padded with NUL and every
    // other VR with a space. Without this a UID of odd length produces a
    // malformed element and the parse fails before anything is stored.
    let pad = if value.len() % 2 == 1 {
        if *vr == *b"UI" { 0x00 } else { 0x20 }
    } else {
        u8::MAX
    };
    let value_len = value.len() + usize::from(pad != u8::MAX);
    if matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN") {
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(value_len as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(value_len as u16).to_le_bytes());
    }
    buf.extend_from_slice(value);
    if pad != u8::MAX {
        buf.push(pad);
    }
}

fn instance() -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", SOP.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WADO^LOG");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"WADOPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", STUDY.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", SERIES.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &[0x41; 64]);
    d
}

struct TestServer {
    addr: SocketAddr,
    _temp: tempfile::TempDir,
}

impl TestServer {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage_dir = temp.path().join("cas");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");
        let btree_root = temp.path().join("btree");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");
        let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");

        let mut config = Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

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
            shadow: ShadowTier::new(btree_root.join("idx")),
            study_scanner: StudyScanner::new(manifest_dir.clone(), idx_dir.clone()),
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(state);
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let sd = shutdown.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, state, sd, timeouts, u32::MAX);
        });

        Self { addr, _temp: temp }
    }

    fn post_single(&self, path: &str, body: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/dicom\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.addr.port(),
            body.len()
        );
        let mut req = head.into_bytes();
        req.extend_from_slice(body);
        stream.write_all(&req).expect("write req");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        buf
    }

    fn get(&self, path: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Accept: multipart/related; type=\"application/dicom\"; \
             transfer-syntax=1.2.840.10008.1.2.1\r\nConnection: close\r\n\r\n",
            self.addr.port()
        );
        stream.write_all(head.as_bytes()).expect("write req");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        buf
    }
}

/// Drain every record the writer channel received, without blocking once it
/// is empty.
fn drain(rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(record) = rx.try_recv() {
        out.push(String::from_utf8_lossy(&record).into_owned());
    }
    out
}

#[test]
fn a_slice_request_is_logged_as_an_access_line_and_an_aggregate() {
    // The writer channel is a plain sync_channel: the server's clog writer
    // thread is NOT started in this harness, so every record the handlers
    // emit lands directly in the channel and is read back here.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4096);
    clarus_server::clog::set_sender(tx);

    let srv = TestServer::start();
    let body = instance();
    let stored = srv.post_single("/dicomweb/studies", &body);
    assert!(
        stored.starts_with(b"HTTP/1.1 200"),
        "the seed must store: {}",
        String::from_utf8_lossy(&stored[..stored.len().min(400)])
    );
    // The seeded instance is the series' only slice, so the first slice
    // request BOTH closes the bucket (the `loaded` line) and gets its own
    // per-request ACCESS line - exactly the pair the fix must produce.
    let response = srv.get(&format!(
        "/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP}"
    ));
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "the slice must be served"
    );
    // Give the logging a moment to land in the channel (the emit is
    // synchronous on this thread, but the STOW above is too).
    std::thread::sleep(Duration::from_millis(200));

    let records = drain(&rx);
    let access: Vec<&String> = records
        .iter()
        .filter(|line| line.contains("ACCESS") && line.contains("/instances/"))
        .collect();
    let aggregate: Vec<&String> = records
        .iter()
        .filter(|line| line.contains("WADO_SERIES"))
        .collect();

    assert!(
        !access.is_empty(),
        "a slice request must produce a per-request ACCESS line: {records:#?}"
    );
    assert!(
        !aggregate.is_empty(),
        "the aggregate lines must still be produced: {records:#?}"
    );
    let access_line = access[0];
    assert!(access_line.contains("rt=wado-study"), "{access_line}");
    assert!(
        access_line.contains("status=200"),
        "the line carries the status: {access_line}"
    );
}
