// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Top-level `/dicomweb/series` and
//! `/dicomweb/instances` global QIDO scan.
//!
//! These are NEW endpoints over the B-tree archive (no SQLite study path), so
//! they get their own harness and fixtures here - the frozen per-study
//! QIDO goldens are intentionally untouched.
//!
//! Coverage: global UID-sorted series/instance search, series filter, the
//! unified `limit`/`offset` Search-Transaction contract (PS3.18 Table
//! 8.3.4-1) with Warning 299, and the always-on XML variant.

#![forbid(unsafe_code)]

use clarus_core::http::conn::{Conn, HEADER_BUF_SIZE};
use clarus_core::http::parse::{parse_headers, parse_request_line};
use clarus_core::http::router::{RouterOutcome, match_route};
use clarus_server::config::Config;
use clarus_server::dispatch::{RequestContext, dispatch};
use clarus_server::handlers::json_common::parse_json;
use clarus_server::shadow::ShadowTier;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Response capture (mirrors dispatch_smoke) ---------------------------

struct Response {
    status: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
}

fn parse_response(raw: &[u8]) -> Response {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head terminator");
    let (head_bytes, body) = (&raw[..split], &raw[split + 4..]);
    let head_str = String::from_utf8_lossy(head_bytes);
    let mut lines = head_str.split("\r\n");
    let status = lines.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(": ") {
            headers.push((k.to_string(), v.to_string()));
        } else if let Some((k, v)) = line.split_once(":") {
            headers.push((k.to_string(), v.trim_start().to_string()));
        }
    }
    Response {
        status,
        headers,
        body: body.to_vec(),
    }
}

// -- Test setup ----------------------------------------------------------

struct TestCtx {
    _temp: tempfile::TempDir,
    manifest_dir: std::path::PathBuf,
    state: Arc<AppState>,
}

fn setup() -> TestCtx {
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
    let storage_dir = temp.path().join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();
    config.storage_dir = storage_dir.to_string_lossy().to_string();

    let cas = clarus_adapters::storage::ContentAddressedStorage::new(temp.path().join("cas"))
        .expect("open cas");

    let state = AppState {
        cas,
        codec_registry: clarus_adapters::codec::CodecRegistry::default(),
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
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir.clone(), idx_dir.clone()),
        config: config.clone(),
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(32),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    };

    TestCtx {
        _temp: temp,
        manifest_dir,
        state: Arc::new(state),
    }
}

/// Write a study manifest directly (no STOW) with one series and one instance.
/// UIDs are caller-supplied so tests control the global sort order.
fn write_manifest(ctx: &TestCtx, study: &str, series: &str, modality: &str, desc: &str, sop: &str) {
    let mut attrs = clarus_core::stow::StudyManifestAttrs {
        patient_name: Some("GLOBAL^QIDO".into()),
        patient_id: Some("GLOBAL".into()),
        study_instance_uid: study.to_string(),
        modality: modality.to_string(),
        ..Default::default()
    };
    let instance = clarus_core::stow::InstanceModel {
        sop_instance_uid: sop.to_string(),
        blake3: [0u8; 32],
        storage_tier: clarus_core::manifest::TIER_HOT,
        transfer_syntax_uid: "1.2.840.10008.1.2.1".to_string(),
        file_size: 64,
        ..Default::default()
    };
    let series_model = [clarus_core::stow::SeriesModel {
        series_instance_uid: series.to_string(),
        modality: modality.to_string(),
        series_number: None,
        series_date: None,
        series_time: None,
        pps_start_date: None,
        pps_start_time: None,
        scheduled_procedure_step_id: None,
        requested_procedure_id: None,
        station_name: None,
        manufacturer: None,
        institutional_department_name: None,
        operators_name: None,
        series_description: Some(desc.to_string()),
        body_part_examined: None,
        protocol_name: None,
        laterality: None,
        instances: vec![instance],
    }];
    clarus_core::stow::write_study_manifest_full(&ctx.manifest_dir, &mut attrs, &series_model)
        .expect("write direct manifest");
}

/// Close the test connection the way a server does: stop writing, then drain
/// what the peer still has in flight.
///
/// Ending the worker with `conn.into_stream()` as the tail expression moved the
/// socket into the unjoined handle's return value, so it was never dropped. The
/// client's read loop then sat out its own 30 s timeout on every request.
fn close_politely(mut conn: Conn) {
    let _ = conn.stream().shutdown(std::net::Shutdown::Write);
    // Bounded: the reply is already out, this only waits for a tail arriving
    // now. Without it the kernel answers the close with a reset that can
    // discard an already-written reply.
    let _ = conn
        .stream()
        .set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 4096];
    while let Ok(n) = std::io::Read::read(conn.stream_mut(), &mut buf) {
        if n == 0 {
            break;
        }
    }
}

/// Send one raw request through the dispatch layer and read the reply.
fn serve(state: &Arc<AppState>, method: &str, path: &str, accept: Option<&str>) -> Response {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let state = Arc::clone(state);
    let worker = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut conn = Conn::new(stream, b"127.0.0.1".to_vec());
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let n = conn
            .read_head(&mut head_buf, Duration::from_secs(10))
            .expect("read head");
        let head_buf = &head_buf[..n];
        let (mut head, off) = parse_request_line(head_buf).expect("line");
        let _ = parse_headers(&mut head, &head_buf[off + 2..]).expect("hdrs");
        match match_route(head.method, head.path) {
            RouterOutcome::Match(route) => {
                let mut ctx = RequestContext {
                    head: &head,
                    uids: route.uids,
                    n_uids: route.n_uids,
                    client_ip: b"127.0.0.1",
                    user: b"anonymous",
                    via: "loopback",
                    state: &state,
                    keep_alive: route.keep_alive,
                    conn: &mut conn,
                    wado_series_expected: None,
                    stow_accepted: 0,
                    proxy_trusted: false,
                    route,
                };
                let _ = dispatch(&route, &mut ctx);
            }
            RouterOutcome::NotFound => {
                let _ = conn.send_error(404, "Not Found", false);
            }
            RouterOutcome::MethodNotAllowed { allow } => {
                conn.header.extend_from_slice(
                    format!("HTTP/1.1 405 Method Not Allowed\r\nAllow: {allow}\r\nContent-Length: 0\r\n\r\n")
                        .as_bytes(),
                );
                let _ = conn.write_head();
            }
            RouterOutcome::BadUid => {
                let _ = conn.send_error(400, "Bad Request", false);
            }
            RouterOutcome::Options => {
                let _ = conn.send_error(204, "No Content", false);
            }
        }
        close_politely(conn);
    });

    let mut c = TcpStream::connect(addr).expect("connect");
    let mut head_bytes = method.as_bytes().to_vec();
    head_bytes.extend_from_slice(b" ");
    head_bytes.extend_from_slice(path.as_bytes());
    head_bytes.extend_from_slice(b" HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nAccept: ");
    head_bytes.extend_from_slice(accept.unwrap_or("application/dicom+json").as_bytes());
    head_bytes.extend_from_slice(b"\r\n\r\n");
    c.write_all(&head_bytes).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(m) => all.extend_from_slice(&buf[..m]),
            Err(_) => break,
        }
    }
    let _ = worker.join();
    parse_response(&all)
}

/// The SeriesInstanceUID values present in a JSON QIDO response body, in order.
fn series_uids(resp: &Response) -> Vec<String> {
    parse_json(&resp.body)
        .expect("valid JSON")
        .as_array()
        .expect("array")
        .iter()
        .map(|o| {
            o.obj_get("0020000E")
                .and_then(|v| v.obj_get("Value"))
                .and_then(|v| v.as_str_array_first())
                .expect("series uid")
                .to_string()
        })
        .collect()
}

/// The SOPInstanceUID values present in an instances JSON body, in order.
fn sop_uids(resp: &Response) -> Vec<String> {
    parse_json(&resp.body)
        .expect("valid JSON")
        .as_array()
        .expect("array")
        .iter()
        .map(|o| {
            o.obj_get("00080018")
                .and_then(|v| v.obj_get("Value"))
                .and_then(|v| v.as_str_array_first())
                .expect("sop uid")
                .to_string()
        })
        .collect()
}

// -- Fixtures: 3 studies, series UIDs written OUT of global UID order --

const CT_B: (&str, &str, &str, &str) = ("1.2.840.1.100", "2.2.2", "CT", "B-Chest"); // series 2.2.2
const MR_C: (&str, &str, &str, &str) = ("1.2.840.1.101", "3.3.3", "MR", "C-Head"); // series 3.3.3
const CT_A: (&str, &str, &str, &str) = ("1.2.840.1.102", "1.1.1", "CT", "A-Lung"); // series 1.1.1

fn seed_archive(ctx: &TestCtx) {
    // SOP UIDs also out of order to prove global SOP sort.
    write_manifest(ctx, CT_B.0, CT_B.1, CT_B.2, CT_B.3, "9.9.9");
    write_manifest(ctx, MR_C.0, MR_C.1, MR_C.2, MR_C.3, "7.7.7");
    write_manifest(ctx, CT_A.0, CT_A.1, CT_A.2, CT_A.3, "5.5.5");
}

// -- Tests ---------------------------------------------------------------

/// All series across the archive, globally sorted by SeriesInstanceUID ASC.
#[test]
fn global_series_sorted_by_uid() {
    let ctx = setup();
    seed_archive(&ctx);
    let resp = serve(&ctx.state, "GET", "/dicomweb/series", None);
    assert!(resp.status.contains("200"), "{}", resp.status);
    assert!(
        resp.header("Content-Type")
            .as_deref()
            .map(|c| c.starts_with("application/dicom+json"))
            .unwrap_or(false),
        "{}",
        resp.header("Content-Type").as_deref().unwrap_or("<none>")
    );
    assert_eq!(
        series_uids(&resp),
        vec!["1.1.1", "2.2.2", "3.3.3"],
        "SeriesInstanceUID ASC across all manifests"
    );
}

/// Series filter (`Modality=CT`) keeps only the matching series, still sorted.
#[test]
fn global_series_filter() {
    let ctx = setup();
    seed_archive(&ctx);
    let resp = serve(&ctx.state, "GET", "/dicomweb/series?Modality=CT", None);
    assert!(resp.status.contains("200"), "{}", resp.status);
    assert_eq!(series_uids(&resp), vec!["1.1.1", "2.2.2"], "only CT series");
}

/// limit + offset paginate the globally-sorted list without dupes/gaps.
#[test]
fn global_series_limit_offset_no_dupes_or_gaps() {
    let ctx = setup();
    seed_archive(&ctx);
    let p0 = serve(&ctx.state, "GET", "/dicomweb/series?limit=2", None);
    let p1 = serve(&ctx.state, "GET", "/dicomweb/series?limit=2&offset=2", None);
    let first = series_uids(&p0);
    let second = series_uids(&p1);
    assert_eq!(first, vec!["1.1.1", "2.2.2"]);
    assert_eq!(second, vec!["3.3.3"]);
    // Cover [1.1.1, 2.2.2, 3.3.3] exactly once each.
    let mut seen = first.clone();
    seen.extend(second);
    seen.sort();
    assert_eq!(seen, vec!["1.1.1", "2.2.2", "3.3.3"], "no dupes or gaps");
}

/// Truncation sets the fixed PS3.18 8.3.4.4.1 `Warning: 299 <service>: There
/// are <remaining> additional results that can be requested`. The three seeded
/// series with `limit=2` leave exactly one match beyond the page. `<service>`
/// is the request's Host header (this harness sends `Host: x`).
#[test]
fn global_series_limit_triggers_warning_299() {
    let ctx = setup();
    seed_archive(&ctx);
    let resp = serve(&ctx.state, "GET", "/dicomweb/series?limit=2", None);
    assert!(resp.status.contains("200"), "{}", resp.status);
    assert_eq!(
        resp.header("Warning").as_deref(),
        Some("299 x: There are 1 additional results that can be requested")
    );
    // No limit -> no warning.
    let full = serve(&ctx.state, "GET", "/dicomweb/series", None);
    assert_eq!(full.header("Warning"), None);
}

/// Global instances, sorted by SOPInstanceUID ASC with injected UIDs.
#[test]
fn global_instances_sorted_by_uid() {
    let ctx = setup();
    seed_archive(&ctx);
    let resp = serve(&ctx.state, "GET", "/dicomweb/instances", None);
    assert!(resp.status.contains("200"), "{}", resp.status);
    assert_eq!(
        sop_uids(&resp),
        vec!["5.5.5", "7.7.7", "9.9.9"],
        "SOPInstanceUID ASC across all manifests"
    );
}

/// The always-on XML variant works for the global routes.
#[test]
fn global_series_xml_variant() {
    let ctx = setup();
    seed_archive(&ctx);
    let resp = serve(
        &ctx.state,
        "GET",
        "/dicomweb/series",
        Some("application/dicom+xml"),
    );
    assert!(resp.status.contains("200"), "{}", resp.status);
    assert_eq!(
        resp.header("Content-Type").as_deref(),
        Some("application/dicom+xml")
    );
    let text = String::from_utf8_lossy(&resp.body);
    assert!(text.contains("<NativeDicomModel"), "XML root");
    // Every series emits its SeriesInstanceUID (0020,000E) in the XML form.
    for uid in ["1.1.1", "2.2.2", "3.3.3"] {
        assert!(text.contains(uid), "series {uid} present in XML");
    }
}

/// An unknown query key is ignored (no 400), even on the
/// global routes - the known/expressible keys drive the match.
#[test]
fn global_series_ignores_unknown_key() {
    let ctx = setup();
    seed_archive(&ctx);
    let resp = serve(&ctx.state, "GET", "/dicomweb/series?bogus=1", None);
    assert!(resp.status.contains("200"), "{}", resp.status);
}
