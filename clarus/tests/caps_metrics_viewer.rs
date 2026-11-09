// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Freeze capabilities + metrics + viewer wire behaviour.
//!
//! Boots the server over a real TCP socket on an ephemeral port and
//! verifies the on-wire behaviour of three endpoints:
//!
//! - `GET /dicomweb/`  -> capabilities JSON (PS3.18 Section 8.9)
//! - `GET /metrics`    -> Prometheus text (structural smoke - counters grow)
//! - `GET /`           -> embedded viewer HTML
//!
//! Capabilities needs care because of **Cargo feature unification**: the
//! workspace crate `cltranscode` enables `clarus-adapters/transcode` by
//! default, so under `cargo test --all` the codecs are compiled into the
//! registry EVEN IF `clarus-server/transcode` is off. An HTTP byte-exact
//! freeze against the runtime registry is therefore build-dependent. We split
//! it in two:
//!
//! - `caps_empty_byte_exact` boots the server with an **empty** `CodecRegistry`
//!   (`CodecRegistry::new()`), so `announceable_transfer_syntaxes()` is `[]`
//!   deterministically in ANY build - the empty-case render is pinned byte-exact.
//! - The non-empty case is pinned byte-exact at the pure-function level
//!   (`build_capabilities` with an explicit slice, commit 2) - immune to
//!   feature unification and to `HashMap` per-process ordering.
//!
//! The `capabilities_*.json` fixtures are written once and never regenerated -
//! they lock in the pre-extraction bytes. Any drift is a regression error.

#![forbid(unsafe_code)]

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

// -- Test server harness -------------------------------------------------

struct Response {
    status: u16,
    headers: std::collections::HashMap<String, Vec<String>>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .and_then(|v| v.first())
            .map(|s| s.as_str())
    }
    fn assert_status(&self, expected: u16) {
        assert_eq!(self.status, expected, "status should be {expected}");
    }
    fn assert_content_type(&self, expected: &str) {
        assert_eq!(self.header("content-type"), Some(expected));
    }
    fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Boot the full router on an ephemeral port with a fresh temp store and the
/// given `codec_registry`. Passing an explicit registry keeps capability
/// output deterministic regardless of Cargo feature unification.
fn start_server(
    codec_registry: clarus_adapters::codec::CodecRegistry,
) -> (SocketAddr, tempfile::TempDir) {
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
    config.port = 0;

    let state = AppState {
        cas,
        codec_registry,
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

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let state = Arc::new(state);
    let shutdown = clarus_core::shutdown::Shutdown::new();
    let sd = shutdown.clone();
    let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
    std::thread::spawn(move || {
        clarus_server::serve::serve(listener, state, sd, timeouts, u32::MAX);
    });

    (addr, temp)
}

fn http_get(addr: SocketAddr, path: &str) -> Response {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    stream.write_all(req.as_bytes()).expect("write req");

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read resp");

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

    Response {
        status,
        headers,
        body,
    }
}

/// A request with an explicit method, headers and optional body (used by the
/// OPTIONS / preflight capability checks).
fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Response {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let body = body.unwrap_or(&[]);
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\nConnection: close\r\n",
        addr.port(),
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).expect("write req");
    stream.write_all(body).expect("write body");

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read resp");

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
    Response {
        status,
        headers,
        body,
    }
}

// -- Capabilities (WADL) -------------------------------------------------

const WADL_CT: &str = "application/vnd.sun.wadl+xml";

/// The capabilities endpoint answers with a WADL Capabilities Description
/// (PS3.18 8.9 + Annex H): the WADL media type, an XML declaration, and an
/// `<application>` -> `<resources base>` -> `<resource>` tree.
#[test]
fn caps_is_wadl() {
    let (addr, _temp) = start_server(clarus_adapters::codec::CodecRegistry::new());
    let r = http_get(addr, "/dicomweb/");
    r.assert_status(200);
    r.assert_content_type(WADL_CT);
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#),
        "WADL must open with the XML declaration: {body}"
    );
    assert!(body.contains("<application"), "WADL application element");
    assert!(
        body.contains(r#"<resources base="http://"#),
        "WADL resources element with an absolute base attribute: {body}"
    );
    assert!(body.contains("<resource"), "WADL resource elements");
    assert!(
        body.contains("modality-scheduled-procedure-steps"),
        "WADL must describe the MWL resource"
    );
}

/// The base URI (bare `/dicomweb`, no trailing slash) answers the same WADL
/// document; Weasis probes the configured base verbatim and needs a 2xx.
#[test]
fn caps_base_without_slash_is_wadl() {
    let (addr, _temp) = start_server(clarus_adapters::codec::CodecRegistry::new());
    let r = http_get(addr, "/dicomweb");
    r.assert_status(200);
    r.assert_content_type(WADL_CT);
}

/// OPTIONS on the Base URI is the Retrieve Capabilities Transaction
/// (PS3.18 8.9.1) and answers the WADL document - NOT the 204 CORS preflight
/// that any other OPTIONS gets.
#[test]
fn caps_options_base_is_wadl() {
    let (addr, _temp) = start_server(clarus_adapters::codec::CodecRegistry::new());
    let r = http_request(addr, "OPTIONS", "/dicomweb", &[("Accept", WADL_CT)], None);
    r.assert_status(200);
    r.assert_content_type(WADL_CT);
    assert!(
        String::from_utf8_lossy(&r.body).contains("<application"),
        "OPTIONS base must return the WADL document"
    );
}

/// OPTIONS on a NON-base path stays the CORS preflight: 204, no body. The
/// base-URI guard must not swallow every preflight.
#[test]
fn caps_options_non_base_is_preflight_204() {
    let (addr, _temp) = start_server(clarus_adapters::codec::CodecRegistry::new());
    let r = http_request(
        addr,
        "OPTIONS",
        "/dicomweb/studies",
        &[("Access-Control-Request-Method", "POST")],
        None,
    );
    r.assert_status(204);
}

/// Every concrete (non-template) `resource path` in the served WADL under the
/// `<study>` subtree that has NO `{...}` placeholder must be GET-routable.
/// This preserves the old "URL = route" invariant in the WADL form: the
/// top-level collection paths a client can call as-is are checked against the
/// router.
#[test]
fn capabilities_resource_paths_are_routes() {
    use clarus_core::http::parse::Method;
    use clarus_core::http::router::{RouterOutcome, match_route};

    let (status, _, body) =
        clarus_server::handlers::capabilities::build_capabilities(Some("http://h"));
    assert_eq!(status, 200);
    let wadl = String::from_utf8(body).expect("wadl utf-8");

    // The managed collections, each a concrete top-level path.
    let mut checked = 0usize;
    for path in [
        "/dicomweb/studies",
        "/dicomweb/workitems",
        "/dicomweb/commitment-requests",
        "/dicomweb/modality-scheduled-procedure-steps",
        "/dicomweb/modality-performed-procedure-steps",
    ] {
        assert!(
            wadl.contains(&format!(
                "path=\"{}\"",
                path.trim_start_matches("/dicomweb/")
            )),
            "WADL must describe the {path} resource"
        );
        checked += 1;
    }
    assert!(checked >= 5, "expected >=5 concrete advertised paths");

    // Of those, the ones a client may GET as a collection. PS3.18 13.3 puts
    // both Storage Commitment transactions on {TransactionUID} - there is no
    // collection transaction - and 15.3 forbids listing MPPS. Advertising the
    // path in the WADL does not make it routable, so only the searchable
    // collections are checked here.
    for path in ["/dicomweb/studies", "/dicomweb/workitems"] {
        assert!(
            matches!(
                match_route(Method::Get, path.as_bytes()),
                RouterOutcome::Match(_)
            ),
            "advertised collection path not routable: {path}"
        );
    }
}

// -- Metrics freeze (structural - counters grow) -------------------------

const METRICS_CT: &str = "text/plain; version=0.0.4";

/// Fixed set of metric names that must always be present in the scrape.
const METRIC_NAMES: &[&str] = &[
    "clarus_http_requests_total",
    "clarus_http_requests_active",
    "clarus_http_request_duration_microseconds_total",
    "clarus_http_request_duration_count",
    "clarus_http_requests_labeled",
    "clarus_http_request_duration_seconds",
    "clarus_storage_objects_total",
    "clarus_storage_bytes_total",
    "clarus_storage_instances_total",
    "clarus_storage_unreferenced_total",
    "clarus_orphans_pending",
    "clarus_dicom_stow_parse_errors_total",
    "clarus_chunks_total",
    "clarus_chunks_unique",
    "clarus_disk_probe_latency_ms",
    "clarus_idx_stale_objects",
    "clarus_process_rss_bytes",
];

#[test]
fn metrics_freeze_smoke() {
    let (addr, _temp) = start_server(clarus_adapters::codec::CodecRegistry::new());
    let r = http_get(addr, "/metrics");
    r.assert_status(200);
    r.assert_content_type(METRICS_CT);
    let body = r.body_str();
    assert!(
        body.contains("# TYPE clarus_http_requests_total counter"),
        "TYPE line for clarus_http_requests_total missing"
    );
    for name in METRIC_NAMES {
        assert!(
            body.contains(name),
            "metric {name} missing from /metrics output"
        );
    }
}

// -- Viewer freeze -------------------------------------------------------

const VIEWER_CT: &str = "text/html; charset=utf-8";

#[test]
fn viewer_freeze_smoke() {
    let (addr, _temp) = start_server(clarus_adapters::codec::CodecRegistry::new());
    let r = http_get(addr, "/");
    r.assert_status(200);
    r.assert_content_type(VIEWER_CT);
    let body = r.body_str();
    assert!(
        body.starts_with("<!DOCTYPE html>"),
        "viewer must start with doctype"
    );
    let expected_title = clarus_server::version::version_line();
    assert!(
        body.contains(&expected_title),
        "viewer must contain the version line: {expected_title}"
    );
    assert!(
        !body.contains("{VERSION}"),
        "viewer must not contain the literal {{VERSION}} placeholder"
    );
    // 0810: the CAS integrity badge is driven by the server's tri-state
    // `audit_state`. The viewer must read all three fields and never hardcode
    // red - the RED class is only emitted for the `corrupt` state.
    assert!(
        body.contains("audit_state"),
        "viewer must read the audit tri-state"
    );
    assert!(
        body.contains("audit_corrupted") && body.contains("audit_unreadable"),
        "viewer must read the audit counts"
    );
    assert!(
        body.contains("'unknown'"),
        "viewer must handle the not-yet-audited (unknown) state"
    );
}
