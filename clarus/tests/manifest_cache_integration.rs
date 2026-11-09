// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Decoded-manifest cache integration.
//!
//!
//! Boots the real server over TCP and proves the cache discipline
//! end-to-end:
//! - STOW -> WADO re-fetch sees the NEW manifest immediately (no stale
//!   entry after a commit);
//! - the W3 update-in-place (replace) is visible on the next WADO hit;
//! - admin delete -> the next WADO is a 404 from disk (the cache must not
//!   serve the deleted study), and a re-STOW after the erase lift serves
//!   again.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::erase_worker::erase_once;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Minimal DICOM builder (Explicit VR LE, uncompressed) ----------------

fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let extended = matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UN" | b"UT"
    );
    if vr == b"UI" && !len.is_multiple_of(2) {
        if extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&((len + 1) as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&((len + 1) as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        buf.push(0x00);
    } else {
        if extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !len.is_multiple_of(2) {
            buf.push(0x00);
        }
    }
}

fn build_instance(sop: &str, study: &str, series: &str, pixel_byte: u8) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"CACHE^SMOKE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"CACHE");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &[pixel_byte; 64]);
    d
}

// -- Test server harness -------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    state: AppState,
    _temp: tempfile::TempDir,
}

impl TestServer {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");

        let storage_dir = temp.path().join("cas");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");
        let btree_root = temp.path().join("btree");
        std::fs::create_dir_all(&btree_root).expect("btree dir");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");

        let mut config = Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.btree.audit_dir = btree_root.join("audit").to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        let state = AppState {
            cas,
            codec_registry: clarus_adapters::codec::CodecRegistry::new(),
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
            study_scanner: StudyScanner::new(manifest_dir.clone(), idx_dir.clone()),
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let server_state = Arc::new(state.clone());
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let sd = shutdown.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, server_state, sd, timeouts, u32::MAX);
        });

        Self {
            addr,
            state,
            _temp: temp,
        }
    }

    fn post_dicom(&self, path: &str, body: &[u8]) -> Response {
        self.request("POST", path, "application/dicom", body)
    }

    fn post_json(&self, path: &str, body: &[u8]) -> Response {
        self.request("POST", path, "application/json", body)
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path, "", &[])
    }

    fn request(&self, method: &str, path: &str, content_type: &str, body: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let ct = if content_type.is_empty() {
            String::new()
        } else {
            format!("Content-Type: {content_type}\r\n")
        };
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{ct}Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.addr.port(),
            body.len()
        );
        let mut req = head.into_bytes();
        req.extend_from_slice(body);
        stream.write_all(&req).expect("write req");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        parse_http(&buf)
    }
}

fn parse_http(buf: &[u8]) -> Response {
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    let body = buf[head_end + 4..].to_vec();
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status");
    Response { status, body }
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

impl Response {
    fn assert_status(&self, expected: u16) {
        assert_eq!(
            self.status,
            expected,
            "status should be {expected} (body: {})",
            String::from_utf8_lossy(&self.body)
        );
    }
}

fn wado_instance(study: &str, series: &str, sop: &str) -> String {
    format!("/dicomweb/studies/{study}/series/{series}/instances/{sop}")
}

/// Drive the erase worker until all tombstones are drained (same shape as
/// erase_smoke's drain_worker).
fn drain(srv: &TestServer, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let stats = erase_once(&srv.state);
        if stats.errors == 0 && (stats.lifted > 0 || stats.tombstones == 0) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("erase worker did not drain tombstones in {timeout:?}");
}

// -- Scenario tests ------------------------------------------------------

/// B3: STOW -> WADO warms the cache; a W3 update (replace) commits a new
/// blob and the NEXT WADO hit serves it - a stale cache would serve the
/// pre-update bytes.
#[test]
fn cache_stow_wado_update_serves_new_blob() {
    let srv = TestServer::start();
    let study = "1.2.3.100";
    let series = "1.2.3.100.1";
    let sop = "1.2.3.100.1.1";

    let v1 = build_instance(sop, study, series, 0x42);
    srv.post_dicom("/dicomweb/studies", &v1).assert_status(200);

    // Warm the cache (this WADO goes through get_or_load: miss -> insert).
    let r1 = srv.get(&wado_instance(study, series, sop));
    r1.assert_status(200);

    // update: same SOP, different pixels -> Updated decision, new blob.
    // 202: an in-place replacement is "stored with warnings" (PS3.18 10.5.2).
    let v2 = build_instance(sop, study, series, 0x99);
    srv.post_dicom("/dicomweb/studies", &v2).assert_status(202);

    // The next WADO must serve the NEW blob (the commit invalidated the
    // cached entry; a stale cache would still return the 0x42 bytes).
    let r2 = srv.get(&wado_instance(study, series, sop));
    r2.assert_status(200);
    assert_ne!(
        r2.body, r1.body,
        "post-update WADO must serve the new blob, not the cached old one"
    );
}

/// B3: admin delete must invalidate the cache - the next WADO is a 404
/// from disk, never a cached 200. After the erase lift, a re-STOW of the
/// same study serves again.
#[test]
fn cache_delete_then_404_then_stow() {
    let srv = TestServer::start();
    let study = "1.2.3.101";
    let series = "1.2.3.101.1";
    let sop = "1.2.3.101.1.1";

    let v1 = build_instance(sop, study, series, 0x42);
    srv.post_dicom("/dicomweb/studies", &v1).assert_status(200);
    // Warm the cache.
    srv.get(&wado_instance(study, series, sop))
        .assert_status(200);

    // Study delete (tombstone rename + begin_write, no end_write).
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"cache delete"}}"#).as_bytes(),
    )
    .assert_status(200);

    // The cached entry must NOT serve the deleted study.
    srv.get(&wado_instance(study, series, sop))
        .assert_status(404);

    // Lift the tombstone (erase worker), then the study UID is free again.
    drain(&srv, Duration::from_secs(5));
    // Re-STOW a fresh SOP under the same study; WADO serves it.
    let sop2 = "1.2.3.101.1.2";
    let v2 = build_instance(sop2, study, series, 0x77);
    srv.post_dicom("/dicomweb/studies", &v2).assert_status(200);
    srv.get(&wado_instance(study, series, sop2))
        .assert_status(200);
}
