// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! CAS audit walker wire test.
//!
//! `POST /admin/v1/storage/cas-audit` (202 running) must drive the background
//! walker to `done` with correct `files_checked`/`errors` - the behaviour that
//! regressed when the axum wrapper was removed. This suite deliberately
//! does NOT touch the frozen `admin_smoke` (its dry-run case stays there).

#![forbid(unsafe_code)]

use clarus_server::config::Config;
use clarus_server::handlers::json_common::{JsonVal, parse_json};
use clarus_server::shadow::ShadowTier;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Test server harness (mirrors admin_smoke.rs) -------------------------

struct TestServer {
    addr: SocketAddr,
    cas_root: PathBuf,
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
            cas_root,
            _temp: temp,
        }
    }

    /// Write one valid CAS file (BLAKE3 of content matches filename) into the
    /// 2-2 bucket layout and return its path.
    fn write_valid_cas_file(&self, content: &[u8]) -> PathBuf {
        let name = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(content));
        let bucket = self.cas_root.join(&name[0..2]).join(&name[2..4]);
        std::fs::create_dir_all(&bucket).expect("bucket");
        let path = bucket.join(format!("{name}.dcm"));
        std::fs::write(&path, content).expect("write cas file");
        path
    }

    fn post_empty(&self, path: &str) -> Response {
        self.request("POST", path, &[], None)
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path, &[], None)
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
    fn json(&self) -> JsonVal {
        parse_json(&self.body).expect("json body")
    }
}

/// Poll GET status until terminal (`done` / `error:`) or timeout.
fn wait_done(srv: &TestServer, timeout: Duration) -> JsonVal {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let r = srv.get("/admin/v1/storage/cas-audit/status");
        r.assert_status(200);
        let j = r.json();
        let status = j.obj_get("status").and_then(|x| x.as_str()).unwrap_or("");
        if status == "done" || status.starts_with("error") {
            return j;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("CAS audit did not reach done in {:?}", timeout);
}

// -- Scenario 1: walker runs to done; a second start while running is 409 --

#[test]
fn cas_audit_runs_to_done_and_conflicts_while_running() {
    // The walker writes the process-global AUDIT_STATE, so this test owns it
    // for its duration: see AUDIT_TEST_LOCK.
    let _guard = AUDIT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_audit_state();

    let srv = TestServer::start();
    srv.write_valid_cas_file(b"payload-one");
    // 0810: also plant a corrupt blob - its filename claims a hash the content
    // does not produce. The audit must report the count AND the hash list.
    let bad_hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let bucket = srv.cas_root.join(&bad_hash[..2]).join(&bad_hash[2..4]);
    std::fs::create_dir_all(&bucket).expect("bucket");
    std::fs::write(bucket.join(format!("{bad_hash}.dcm")), b"tampered").expect("corrupt blob");

    let r = srv.post_empty("/admin/v1/storage/cas-audit");
    r.assert_status(202);
    let start = r.json();
    assert_eq!(
        start.obj_get("status").and_then(|x| x.as_str()),
        Some("running")
    );

    let done = wait_done(&srv, Duration::from_secs(10));
    assert_eq!(
        done.obj_get("status").and_then(|x| x.as_str()),
        Some("done")
    );
    assert_eq!(
        done.obj_get("files_checked").and_then(|x| x.as_u64()),
        Some(2)
    );
    // 0810: one corrupted blob -> errors == corrupted == 1, unreadable 0.
    assert_eq!(done.obj_get("errors").and_then(|x| x.as_u64()), Some(1));
    assert_eq!(done.obj_get("corrupted").and_then(|x| x.as_u64()), Some(1));
    assert_eq!(done.obj_get("unreadable").and_then(|x| x.as_u64()), Some(0));
    let list = done
        .obj_get("corrupted_list")
        .and_then(|x| x.as_array())
        .expect("corrupted_list present");
    assert_eq!(list.len(), 1, "one corrupted blob listed");
    assert_eq!(
        list[0].as_str(),
        Some(bad_hash),
        "the listed hash is the blob's FILE NAME (content address)"
    );

    // While forced into "running", a second start refuses with 409.
    {
        let mut st = clarus_server::handlers::admin::pure::AUDIT_STATE
            .write()
            .unwrap();
        st.status = "running".to_string();
    }
    let conflict = srv.post_empty("/admin/v1/storage/cas-audit");
    conflict.assert_status(409);
    assert_eq!(
        conflict.json().obj_get("status").and_then(|x| x.as_str()),
        Some("running")
    );

    // Reset the shared static so a later/parallel test starts clean.
    reset_audit_state();
}

// -- S42: only ONE audit may ever be claimed --

/// Two simultaneous starts must produce exactly one 202 and one 409.
///
/// The status was read under a read lock and the state rewritten under a
/// write lock, so both requests could observe "idle" and both spawn a walk -
/// two full CAS sweeps over the same store, each reporting the other's
/// progress. Check-and-claim under a single write lock is what closes it.
#[test]
fn concurrent_starts_claim_the_audit_only_once() {
    // The audit state is process-global, so this test owns it: any parallel
    // test that leaves it "running" would make both starts lose. Reducing
    // contention here is the honest way to keep the assertion about the
    // CLAIM rather than about test scheduling.
    let guard = AUDIT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_audit_state();

    let srv = TestServer::start();
    for i in 0..8 {
        srv.write_valid_cas_file(format!("payload-{i}").as_bytes());
    }

    // Release both threads through a barrier so they contend for the claim
    // rather than starting one after the other.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let addr = srv.addr;
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let b = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait();
                raw_post_status(addr, "/admin/v1/storage/cas-audit")
            })
        })
        .collect();

    let statuses: Vec<u16> = handles
        .into_iter()
        .map(|h| h.join().expect("join"))
        .collect();

    let accepted = statuses.iter().filter(|s| **s == 202).count();
    let conflicted = statuses.iter().filter(|s| **s == 409).count();
    assert_eq!(
        accepted, 1,
        "exactly one start may win the claim, got {statuses:?}"
    );
    assert_eq!(
        conflicted, 1,
        "the loser must be answered 409, got {statuses:?}"
    );

    let _ = wait_done(&srv, Duration::from_secs(10));
    reset_audit_state();
    drop(guard);
}

/// Serializes the tests that own the process-global audit state.
///
/// `AUDIT_STATE` is a single static, and the crate's tests run on parallel
/// threads in one process: without this lock a test that leaves the state
/// "running" makes another test's very first start lose the claim.
static AUDIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Post an empty body from a plain thread and return only the status code.
///
/// Needed by the concurrency test: `TestServer::post_empty` borrows the
/// server, which cannot cross a thread boundary, and the test only cares
/// about which request won the claim.
fn raw_post_status(addr: std::net::SocketAddr, path: &str) -> u16 {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    stream.write_all(head.as_bytes()).expect("write req");
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    parse_http(&buf).status
}

/// Put the shared audit state back to idle.
fn reset_audit_state() {
    let mut st = clarus_server::handlers::admin::pure::AUDIT_STATE
        .write()
        .unwrap();
    *st = clarus_server::handlers::admin::pure::CasAuditState {
        status: "idle".to_string(),
        files_checked: 0,
        files_total: 0,
        errors: 0,
        unreferenced: 0,
        corrupted: 0,
        unreadable: 0,
        corrupted_hashes: Vec::new(),
        started_at: None,
        eta_seconds: None,
    };
}
