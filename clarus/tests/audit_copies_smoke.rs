// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! 3008: `GET /admin/v1/storage/audit-copies` wire smoke.
//!
//! The endpoint serves ONLY sidecar facts from `manifests/` - never CAS.
//! Boots the full router on an empty store, forces a snapshot via
//! `clarus_server::audit_copies::refresh`, then asserts the exact JSON.

#![forbid(unsafe_code)]

use clarus_server::config::Config;
use clarus_server::handlers::json_common::{JsonVal, parse_json};
use clarus_server::shadow::ShadowTier;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};

/// Both wire tests refresh the PROCESS-GLOBAL AUDIT_COPIES_STATE, so they
/// must not run in parallel (the last refresh would win).
static STATE_GUARD: Mutex<()> = Mutex::new(());

// -- Test server harness (mirrors cas_audit_smoke.rs) --------------------

struct TestServer {
    addr: SocketAddr,
    manifest_dir: PathBuf,
    _temp: tempfile::TempDir,
}

impl TestServer {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let cas_root = temp.path().join("cas");
        std::fs::create_dir_all(&cas_root).expect("cas dir");

        let btree_root = temp.path().join("btree");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let mut config = Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.btree.audit_dir = btree_root.join("audit").to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        let state = AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(cas_root.clone())
                .expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::new(),
            ups_events: clarus_server::sse_fanout::DualEventSink::new(),
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
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir.clone(), idx_dir),
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
            manifest_dir,
            _temp: temp,
        }
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path)
    }

    fn request(&self, method: &str, path: &str) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            self.addr.port()
        );
        stream.write_all(head.as_bytes()).expect("write req");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        let head_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header terminator");
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .expect("status");
        Response {
            status,
            body: buf[head_end + 4..].to_vec(),
        }
    }
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
    fn json(&self) -> JsonVal {
        parse_json(&self.body).expect("json body")
    }
}

// -- Scenarios ------------------------------------------------------------

/// Boot on an empty store: the spawned scanner's first pass runs at startup,
/// so the snapshot is ready with zeros (never the startup pending shape in a
/// live process - but the wire test forces its own refresh for determinism).
#[test]
fn empty_store_ready_with_zeros() {
    let _guard = STATE_GUARD.lock().expect("state guard");
    let srv = TestServer::start();
    clarus_server::audit_copies::refresh(&srv.manifest_dir);
    let r = srv.get("/admin/v1/storage/audit-copies");
    r.assert_status(200);
    let body = String::from_utf8_lossy(&r.body).to_string();
    assert!(
        body.starts_with("{\"status\":\"ready\""),
        "ready after refresh: {body}"
    );
    let j = r.json();
    assert_eq!(j.obj_get("total_studies").and_then(|x| x.as_u64()), Some(0));
    assert_eq!(j.obj_get("total_tracked").and_then(|x| x.as_u64()), Some(0));
    assert_eq!(j.obj_get("total_orphans").and_then(|x| x.as_u64()), Some(0));
    assert_eq!(
        j.obj_get("studies")
            .and_then(|x| x.as_array())
            .map(|a| a.len()),
        Some(0)
    );
    assert!(j.obj_get("generated_at").and_then(|x| x.as_str()).is_some());
}

/// Sidecar facts with a malformed line: valid lines appear, the bad one is
/// skipped, totals match the valid line counts.
#[test]
fn sidecar_facts_exact_and_malformed_skipped() {
    let _guard = STATE_GUARD.lock().expect("state guard");
    let srv = TestServer::start();
    let study = "1.2.3.4000";
    let hex_a = "ab".repeat(32);
    let hex_b = "cd".repeat(32);
    let hex_o = "ef".repeat(32);
    std::fs::write(
        srv.manifest_dir.join(format!("{study}.audited")),
        format!("{hex_a} 1\n\nnot a pair line\n{hex_b} 2\n"),
    )
    .expect("write audited sidecar");
    std::fs::write(
        srv.manifest_dir.join(format!("{study}.orphans")),
        format!("{hex_o} 1\n"),
    )
    .expect("write orphans sidecar");

    clarus_server::audit_copies::refresh(&srv.manifest_dir);
    let r = srv.get("/admin/v1/storage/audit-copies");
    r.assert_status(200);
    let j = r.json();
    assert_eq!(j.obj_get("status").and_then(|x| x.as_str()), Some("ready"));
    assert_eq!(j.obj_get("total_studies").and_then(|x| x.as_u64()), Some(1));
    assert_eq!(j.obj_get("total_tracked").and_then(|x| x.as_u64()), Some(2));
    assert_eq!(j.obj_get("total_orphans").and_then(|x| x.as_u64()), Some(1));

    let studies = j
        .obj_get("studies")
        .and_then(|x| x.as_array())
        .expect("array");
    assert_eq!(studies.len(), 1, "one study: {studies:?}");
    let s = &studies[0];
    assert_eq!(s.obj_get("study_uid").and_then(|x| x.as_str()), Some(study));
    let tracked = s
        .obj_get("tracked")
        .and_then(|x| x.as_array())
        .expect("tracked");
    assert_eq!(tracked.len(), 2, "two valid tracked lines: {tracked:?}");
    assert_eq!(
        tracked[0].obj_get("hash").and_then(|x| x.as_str()),
        Some(hex_a.as_str()),
        "sorted by hash"
    );
    assert_eq!(
        tracked[1].obj_get("hash").and_then(|x| x.as_str()),
        Some(hex_b.as_str())
    );
    assert_eq!(tracked[0].obj_get("tier").and_then(|x| x.as_u64()), Some(1));
    assert_eq!(tracked[1].obj_get("tier").and_then(|x| x.as_u64()), Some(2));
    let orphans = s
        .obj_get("orphans")
        .and_then(|x| x.as_array())
        .expect("orphans");
    assert_eq!(orphans.len(), 1, "one orphan: {orphans:?}");
    assert_eq!(
        orphans[0].obj_get("hash").and_then(|x| x.as_str()),
        Some(hex_o.as_str())
    );
    assert_eq!(orphans[0].obj_get("tier").and_then(|x| x.as_u64()), Some(1));
}
