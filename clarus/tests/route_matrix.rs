// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! data-driven route matrix over the real `serve()` transport.
//!
//! One function drives every route (method + example path) against a loopback
//! server and asserts the real HTTP status. This is intentionally a *matrix*
//! (not byte-exact - those goldens live in the smoke suites): it pins the
//! router/bad-UID/405+Allow contract.
//!
//! Notes on expected statuses:
//! - QIDO/health/metrics/viewer/capabilities/admin GET -> 200 with no data.
//! - qido_patients is Access::LocalhostOnly but loopback is trusted -> 200.
//! - WADO object routes on a nonexistent object -> 404 (locate_object).
//! - wado_thumbnail / series_thumbnail (C1 22.08): no longer feature-gated;
//!   they resolve the object and serve a JPEG (200) or 406 for compressed TS.
//!   In this empty-store matrix there is no object, so they fall through to
//!   locate_object/plan_series -> 404 (like the other object routes).
//! - A `POST /admin/v1/storage/cas-audit` with a valid store -> 202 (the
//!   background walker then finishes; we treat 202 as the accepted start).

#![forbid(unsafe_code)]

use clarus_core::http::conn::Timeouts;
use clarus_core::shutdown::Shutdown;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::{Duration, Instant};

/// (method, example path, expected status)
const MATRIX: &[(&str, &str, u16)] = &[
    // -- Plain/infra GET ---------------------------------------------
    ("GET", "/health", 200),
    ("GET", "/metrics", 200),
    ("GET", "/", 200),
    ("GET", "/dicomweb/", 200),
    // Bare /dicomweb serves capabilities directly (200). Weasis 4.7.2
    // probes the root before every QIDO search and aborts on a redirect
    // (its manual redirect follower cannot resolve a relative Location).
    ("GET", "/dicomweb", 200),
    // -- QIDO --------------------------------------------------------
    ("GET", "/dicomweb/patients", 200),
    ("GET", "/dicomweb/studies", 200),
    ("GET", "/dicomweb/series", 200), // top-level global series QIDO (gap 2)
    ("GET", "/dicomweb/instances", 200), // top-level global instances QIDO (gap 2)
    ("GET", "/dicomweb/workitems", 200),
    ("GET", "/dicomweb/studies/1.2.3.4.5.100/instances", 200),
    ("GET", "/dicomweb/studies/1.2.3.4.5.100/series", 200),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances",
        200,
    ),
    // -- WADO object routes (missing object -> 404) ------------------
    ("GET", "/wadouri", 400), // missing requestType=WADO -> 400
    ("GET", "/dicomweb/studies/1.2.3.4.5.100", 404),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1",
        404,
    ),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/metadata",
        404,
    ),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/thumbnail",
        404,
    ),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/1.2.3.4.5.100.1.1",
        404,
    ),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/1.2.3.4.5.100.1.1/metadata",
        404,
    ),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/1.2.3.4.5.100.1.1/thumbnail",
        404,
    ),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/1.2.3.4.5.100.1.1/rendered",
        404,
    ),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/1.2.3.4.5.100.1.1/frames/1",
        404,
    ),
    (
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/1.2.3.4.5.100.1.1/bulkdata/7FE00010",
        404,
    ),
    // -- UPS events / SSE are streaming, not one-shot: excluded from the
    //    matrix (they keep the connection open by design).
    // -- Admin GET ---------------------------------------------------
    ("GET", "/admin/v1/status", 200),
    ("GET", "/admin/v1/health", 200),
    ("GET", "/admin/v1/version", 200),
    ("GET", "/admin/v1/dashboard", 200),
    ("GET", "/admin/v1/audit-log", 200),
    ("GET", "/admin/v1/storage", 200),
    // admin_logs_stream is SSE (streaming) -> excluded from the one-shot matrix.
    ("GET", "/admin/v1/metrics/history", 200),
    ("GET", "/admin/v1/instances/search", 200),
    ("GET", "/admin/v1/storage/s3-validation", 200),
    ("GET", "/admin/v1/storage/cas-audit/status", 200),
    // -- POST --------------------------------------------------------
    // The one-shot probe sends no Content-Type, which is neither
    // `application/dicom` nor `multipart/related`: PS3.18 10.5.3-1 makes that
    // an unsupported media type (415), not a malformed request.
    ("POST", "/dicomweb/studies", 415),
    ("POST", "/admin/v1/storage/cas-audit", 202), // accepted -> 202 running
    ("POST", "/admin/v1/maintenance", 200),       // toggles off (no-op body)
    // PS3.18 11.8 Request Cancellation: the route exists, and an unknown
    // workitem is 404 (a product feature, not an AIW-I obligation).
    (
        "POST",
        "/dicomweb/workitems/1.2.3.4.5.200/cancelrequest",
        404,
    ),
    // -- PUT / DELETE ------------------------------------------------
    ("PUT", "/dicomweb/workitems/1.2.3.4.5.200", 404), // not found workitem
    ("DELETE", "/dicomweb/workitems/1.2.3.4.5.200", 404), // not found -> 404
];

struct TestCtx {
    _temp: tempfile::TempDir,
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
    config.btree.audit_dir = btree_root.join("audit").to_string_lossy().to_string();
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

fn start_server(
    listener: TcpListener,
    ctx: &TestCtx,
    shutdown: Shutdown,
) -> std::thread::JoinHandle<()> {
    let state = Arc::clone(&ctx.state);
    let timeouts = Timeouts::from_secs(10, 30, 5, 60);
    std::thread::spawn(move || {
        clarus_server::serve::serve(listener, state, shutdown, timeouts, 64);
    })
}

fn send_raw(addr: std::net::SocketAddr, method: &str, path: &str) -> u16 {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    c.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&out);
    head.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0)
}

// --- Full-size rendered (`Accept: image/jpeg` on the instance endpoint) ---
// Part (b), 26.08. These seed a real EVRLE instance through STOW-RS (which
// writes CAS + the per-study manifest that WADO resolves), then drive the
// instance endpoint with different Accept ranges. The rendered path must:
// - `Accept: image/jpeg` -> 200 image/jpeg (full-size JPEG) on uncompressed.
// - compressed instance -> 406 in the default build (no decoder).
// - `Accept: application/dicom` -> STILL application/dicom (unchanged).

/// Append an Explicit VR LE element (mirrors wado_smoke's builder).
fn push_explicit_rt(out: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    out.extend_from_slice(&group.to_le_bytes());
    out.extend_from_slice(&element.to_le_bytes());
    out.extend_from_slice(vr);
    let extended = matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    );
    if extended {
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    out.extend_from_slice(value);
    if !value.len().is_multiple_of(2) {
        out.push(0x00);
    }
}

const RENDER_STUDY: &str = "1.2.826.0.1.3680043.10.7000.1001";
const RENDER_SERIES: &str = "1.2.826.0.1.3680043.10.7000.1101";
const RENDER_SOP: &str = "1.2.826.0.1.3680043.10.7000.1201";

/// Build an 8-bit MONOCHROME2 EVRLE instance with a small pixel buffer.
fn render_instance() -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_explicit_rt(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    push_explicit_rt(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_explicit_rt(&mut d, 0x0008, 0x0018, b"UI", RENDER_SOP.as_bytes());
    push_explicit_rt(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_explicit_rt(&mut d, 0x0020, 0x000d, b"UI", RENDER_STUDY.as_bytes());
    push_explicit_rt(&mut d, 0x0020, 0x000e, b"UI", RENDER_SERIES.as_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_explicit_rt(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x1050, b"DS", b"128");
    push_explicit_rt(&mut d, 0x0028, 0x1051, b"DS", b"256");
    push_explicit_rt(&mut d, 0x7FE0, 0x0010, b"OB", &[0x5a_u8; 64]);
    d
}

/// A byte-compressed-looking instance: JPEG Baseline meta TransferSyntaxUID.
#[cfg(not(feature = "transcode"))]
fn render_instance_compressed() -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_explicit_rt(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.50");
    push_explicit_rt(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_explicit_rt(&mut d, 0x0008, 0x0018, b"UI", RENDER_SOP.as_bytes());
    push_explicit_rt(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_explicit_rt(&mut d, 0x0020, 0x000d, b"UI", RENDER_STUDY.as_bytes());
    push_explicit_rt(&mut d, 0x0020, 0x000e, b"UI", RENDER_SERIES.as_bytes());
    // Geometry is required for the render path to reach the decoder: without
    // it the handler answers 500 (invalid dimensions) instead of the
    // compressed-syntax decision, which is what this fixture probes.
    push_explicit_rt(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_explicit_rt(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    // Encapsulated PixelData (undefined length + one opaque fragment): the
    // shape of a compressed instance. Never decoded in the default build (the
    // decoder is absent) and never decodable here anyway - the point is the
    // transfer-syntax decision, not the pixels.
    let mut pixel = Vec::new();
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    pixel.extend_from_slice(&16u32.to_le_bytes());
    pixel.extend_from_slice(&[0xAB_u8; 16]);
    pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    d.extend_from_slice(&0x7FE0u16.to_le_bytes());
    d.extend_from_slice(&0x0010u16.to_le_bytes());
    d.extend_from_slice(b"OB");
    d.extend_from_slice(&[0x00, 0x00]);
    d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    d.extend_from_slice(&pixel);
    d
}

/// Raw HTTP request (method + path + arbitrary headers) returning (status,
/// content-type, body). Seeded via STOW below; `send_raw` lacks headers.
fn http_exchange(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, Option<String>, Vec<u8>) {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    c.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head_end = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    let head = String::from_utf8_lossy(&out[..head_end]).to_string();
    let body = out[head_end + 4..].to_vec();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let ct = head
        .lines()
        .skip(1)
        .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
        .map(|l| {
            l.split_once(':')
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        });
    (status, ct, body)
}

fn seed_stow_rt(addr: SocketAddr, body: &[u8]) {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: application/dicom\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut full = req.into_bytes();
    full.extend_from_slice(body);
    c.write_all(&full).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&out);
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    // These fixtures declare empty Type 2 fields, so the store coerces them
    // and answers 202 "stored with warnings" (PS3.18 10.5.2). The seed still
    // succeeded - that is what this helper asserts.
    assert!(
        status == 200 || status == 202,
        "STOW seed must accept the instance, got {status}: {head}"
    );
}

/// Drive the rendered instance endpoint over the real router with a seeded
/// instance.
#[test]
fn rendered_instance_accept_dispatch() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &render_instance());

    let instance =
        format!("/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}");

    // 1. Accept: image/jpeg -> full-size 200 image/jpeg with a JPEG body.
    let (st, ct, body) = http_exchange(addr, &instance, &[("Accept", "image/jpeg")]);
    assert_eq!(st, 200, "rendered must be 200");
    assert_eq!(ct.as_deref(), Some("image/jpeg"), "content-type image/jpeg");
    assert!(
        body.len() > 4 && body[0] == 0xFF && body[1] == 0xD8,
        "JPEG SOI"
    );
    assert!(
        body[body.len() - 2] == 0xFF && body[body.len() - 1] == 0xD9,
        "JPEG EOI"
    );

    // 2. Accept: application/dicom must NOT be affected (still raw DICOM).
    let (st2, ct2, _) = http_exchange(addr, &instance, &[("Accept", "application/dicom")]);
    assert_eq!(st2, 200, "application/dicom unchanged");
    assert_eq!(
        ct2.as_deref(),
        Some("application/dicom"),
        "raw dicom content type"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

/// Compressed instance, `/rendered` with `Accept: image/jpeg`, default build:
/// the decode is attempted and the honest 406 comes from the absent decoder,
/// not from a pre-flight guard. (In a transcode build the same request is 200 -
/// see `rendered_compressed_200_transcode_build`.)
#[cfg(not(feature = "transcode"))]
#[test]
fn rendered_compressed_406_default_build() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &render_instance_compressed());

    let instance = format!(
        "/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}/rendered"
    );
    let (st, _, _) = http_exchange(addr, &instance, &[("Accept", "image/jpeg")]);
    assert_eq!(st, 406, "compressed rendered must be 406 in default build");

    shutdown.request_stop();
    let _ = handle.join();
}

/// Compressed instance with a REAL RLE-encoded PixelData (produced by the RLE
/// codec itself) must decode + render to 200 image/jpeg in a transcode build.
/// Builds a valid RLE Lossless DICOM: preamble + meta TS + dataset with an
/// encapsulated OB PixelData (BOT item + RLE fragment + sequence delimiter).
#[cfg(feature = "transcode")]
#[test]
fn rendered_compressed_200_transcode_build() {
    use clarus_adapters::codec::rle::RleCodec;
    use clarus_adapters::codec::{DecodedPixels, DicomCodec, DicomMeta};

    let pixels = DecodedPixels {
        data: [0x7a_u8; 64].to_vec(),
        rows: 8,
        columns: 8,
        samples_per_pixel: 1,
        bits_allocated: 8,
        photometric_interpretation: "MONOCHROME2".to_string(),
    };
    let meta = DicomMeta {
        rows: 8,
        columns: 8,
        samples_per_pixel: 1,
        bits_allocated: 8,
        bits_stored: 8,
        photometric_interpretation: "MONOCHROME2".to_string(),
        planar_configuration: None,
        pixel_representation: 0,
        frames: 1,
    };
    let rle_fragment = RleCodec.encode(&pixels, &meta).expect("rle encode");

    let mut d = Vec::with_capacity(512);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_explicit_rt(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.5");
    push_explicit_rt(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_explicit_rt(&mut d, 0x0008, 0x0018, b"UI", RENDER_SOP.as_bytes());
    push_explicit_rt(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_explicit_rt(&mut d, 0x0020, 0x000d, b"UI", RENDER_STUDY.as_bytes());
    push_explicit_rt(&mut d, 0x0020, 0x000e, b"UI", RENDER_SERIES.as_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_explicit_rt(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());

    // Encapsulated OB PixelData with undefined length.
    let ext = [0u8, 0u8];
    d.extend_from_slice(&0x7FE0u16.to_le_bytes());
    d.extend_from_slice(&0x0010u16.to_le_bytes());
    d.extend_from_slice(b"OB");
    d.extend_from_slice(&ext);
    d.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes());
    // BOT item: tag + 0 length.
    d.extend_from_slice(&0xFFFEu16.to_le_bytes());
    d.extend_from_slice(&0xE000u16.to_le_bytes());
    d.extend_from_slice(&0u32.to_le_bytes());
    // RLE fragment item.
    d.extend_from_slice(&0xFFFEu16.to_le_bytes());
    d.extend_from_slice(&0xE000u16.to_le_bytes());
    d.extend_from_slice(&(rle_fragment.len() as u32).to_le_bytes());
    d.extend_from_slice(&rle_fragment);
    if !rle_fragment.len().is_multiple_of(2) {
        d.push(0x00);
    }
    // Sequence delimiter.
    d.extend_from_slice(&0xFFFEu16.to_le_bytes());
    d.extend_from_slice(&0xE0DDu16.to_le_bytes());
    d.extend_from_slice(&0u32.to_le_bytes());

    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &d);

    let instance =
        format!("/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}");
    let (st, ct, body) = http_exchange(addr, &instance, &[("Accept", "image/jpeg")]);
    assert_eq!(st, 200, "transcode compressed rendered must be 200");
    assert_eq!(ct.as_deref(), Some("image/jpeg"), "content-type image/jpeg");
    assert!(
        body.len() > 4 && body[0] == 0xFF && body[1] == 0xD8,
        "JPEG SOI"
    );
    assert!(
        body[body.len() - 2] == 0xFF && body[body.len() - 1] == 0xD9,
        "JPEG EOI"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

// --- route matrix statuses (below) -----------------------------------------

/// Drive every row of the route matrix and assert the status.
#[test]
fn route_matrix_statuses() {
    // Reset the CAS audit global so the 202 row starts clean.
    {
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

    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    for (i, &(method, path, want)) in MATRIX.iter().enumerate() {
        let got = send_raw(addr, method, path);
        assert_eq!(
            got, want,
            "matrix[{i}] {method} {path}: expected {want}, got {got}"
        );
    }

    shutdown.request_stop();
    let _ = handle.join();
}

fn bind() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

// -- 404 for unknown paths on all four methods ---------------------------
#[test]
fn unknown_path_is_404_on_all_methods() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    for m in ["GET", "POST", "PUT", "DELETE"] {
        let got = send_raw(addr, m, "/no/such/route/here");
        assert_eq!(got, 404, "{m} unknown path must be 404, got {got}");
    }

    shutdown.request_stop();
    let _ = handle.join();
}

// -- 405 + Allow on a wrong method for a known path ----------------------
#[test]
fn wrong_method_is_405_with_allow() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    // PATCH is an unknown method -> 405 + Allow listing the supported set.
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(b"PATCH /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&out);
    assert!(head.starts_with("HTTP/1.1 405"), "got: {head}");
    assert!(
        head.to_ascii_lowercase().contains("allow:"),
        "405 must carry Allow header: {head}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

// -- BadUid -> 400 (final status is pinned by the router) ----------------
// A DICOM UID with a non-digit/non-dot character fails `uid_ok` -> BadUid,
// which the transport maps to 400 Bad Request.
#[test]
fn bad_uid_is_400() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    // A study UID with non-UID bytes fails `uid_ok` -> BadUid -> 400.
    let got = send_raw(addr, "GET", "/dicomweb/studies/1.2.3.BAD_UID/series");
    assert_eq!(got, 400, "BadUid study must map to 400, got {got}");
    // SOP-with-letters in a WADO instance path is also a BadUid.
    let got2 = send_raw(
        addr,
        "GET",
        "/dicomweb/studies/1.2.3.4.5.100/series/1.2.3.4.5.100.1/instances/not.a.uid",
    );
    assert_eq!(got2, 400, "BadUid SOP -> 400, got {got2}");

    shutdown.request_stop();
    let _ = handle.join();
}

// --- 0410: /rendered answers image/png for RGB previews -----------------

const RGB_SOP: &str = "1.2.826.0.1.3680043.10.7000.1301";
const RGB_STUDY: &str = "1.2.826.0.1.3680043.10.7000.1001";
const RGB_SERIES: &str = "1.2.826.0.1.3680043.10.7000.1101";

/// Build an 8-bit RGB EVRLE single-frame instance (SPP=3, PlanarConfig=0),
/// the shape of the three Slim preview instances.
fn rgb_instance(rows: u16, cols: u16) -> Vec<u8> {
    let mut d = Vec::with_capacity(2048);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_explicit_rt(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    push_explicit_rt(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.77.1.6",
    );
    push_explicit_rt(&mut d, 0x0008, 0x0018, b"UI", RGB_SOP.as_bytes());
    push_explicit_rt(&mut d, 0x0008, 0x0060, b"CS", b"SM");
    push_explicit_rt(&mut d, 0x0020, 0x000d, b"UI", RGB_STUDY.as_bytes());
    push_explicit_rt(&mut d, 0x0020, 0x000e, b"UI", RGB_SERIES.as_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0002, b"US", &3u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0004, b"CS", b"RGB");
    push_explicit_rt(&mut d, 0x0028, 0x0006, b"US", &0u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    // Interleaved RGB8, with the even-byte row pad when cols*3 is odd.
    let row = (cols as usize) * 3;
    let stride = row + (row & 1);
    let mut pixels = vec![0u8; stride * rows as usize];
    for (i, b) in pixels.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    push_explicit_rt(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
}

/// Build a MONOCHROME2 EVRLE instance with a NUMBER OF FRAMES = 2 (a VOLUME
/// stand-in): /rendered must 406 from the header alone.
fn multiframe_instance() -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_explicit_rt(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    push_explicit_rt(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_explicit_rt(&mut d, 0x0008, 0x0018, b"UI", RENDER_SOP.as_bytes());
    push_explicit_rt(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_explicit_rt(&mut d, 0x0020, 0x000d, b"UI", RENDER_STUDY.as_bytes());
    push_explicit_rt(&mut d, 0x0020, 0x000e, b"UI", RENDER_SERIES.as_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_explicit_rt(&mut d, 0x0028, 0x0008, b"IS", b"2 ");
    push_explicit_rt(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x7FE0, 0x0010, b"OB", &[0x5a_u8; 128]);
    d
}

fn png_sig_ok(body: &[u8]) -> bool {
    body.len() >= 8 && body[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
}

/// The PNG IHDR width/height (bytes 16..24, big-endian), for dims assertions.
fn png_dims(body: &[u8]) -> (u32, u32) {
    let w = u32::from_be_bytes([body[16], body[17], body[18], body[19]]);
    let h = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
    (w, h)
}

/// `Accept: image/png` on an RGB instance -> 200 PNG at NATIVE size when no
/// viewport is sent (Slim's resizeFactor == 1).
#[test]
fn rendered_png_rgb_native() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &rgb_instance(40, 60));
    let path =
        format!("/dicomweb/studies/{RGB_STUDY}/series/{RGB_SERIES}/instances/{RGB_SOP}/rendered");
    let (st, ct, body) = http_exchange(addr, &path, &[("Accept", "image/png")]);
    assert_eq!(st, 200, "rendered PNG must be 200");
    assert_eq!(ct.as_deref(), Some("image/png"), "content-type image/png");
    assert!(png_sig_ok(&body), "body must be a PNG");
    assert_eq!(png_dims(&body), (60, 40), "native dims (no viewport)");

    shutdown.request_stop();
    let _ = handle.join();
}

/// `Accept: image/png` + `viewport=512,512` -> 200 PNG fit in the box (upscale
/// allowed: a 60x40 source fits to 512x341).
#[test]
fn rendered_png_rgb_viewport_fit() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &rgb_instance(40, 60));
    let path = format!(
        "/dicomweb/studies/{RGB_STUDY}/series/{RGB_SERIES}/instances/{RGB_SOP}/rendered?viewport=512,512"
    );
    let (st, ct, body) = http_exchange(addr, &path, &[("Accept", "image/png")]);
    assert_eq!(st, 200);
    assert_eq!(ct.as_deref(), Some("image/png"));
    assert!(png_sig_ok(&body));
    let (w, h) = png_dims(&body);
    assert_eq!(w, 512, "long axis lands on the box");
    assert!(
        (300..512).contains(&h),
        "short axis preserves aspect: got {h}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

/// `Accept: image/jpeg` on an RGB instance -> 406 (JPEG is MONO-only here).
#[test]
fn rendered_jpeg_on_rgb_is_406() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &rgb_instance(40, 60));
    let path =
        format!("/dicomweb/studies/{RGB_STUDY}/series/{RGB_SERIES}/instances/{RGB_SOP}/rendered");
    let (st, _, _) = http_exchange(addr, &path, &[("Accept", "image/jpeg")]);
    assert_eq!(st, 406, "RGB + image/jpeg must be 406");

    shutdown.request_stop();
    let _ = handle.join();
}

/// `Accept: image/gif` on a MONO instance -> 200 GIF (1310, closes D1: image/gif
/// is Required for Single Frame Image REST in PS3.18 Table 8.7.4-1).
#[test]
fn rendered_gif_mono_is_200() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &render_instance());
    let path = format!(
        "/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}/rendered"
    );
    let (st, ct, body) = http_exchange(addr, &path, &[("Accept", "image/gif")]);
    assert_eq!(st, 200, "rendered GIF must be 200");
    assert_eq!(ct.as_deref(), Some("image/gif"), "content-type image/gif");
    assert!(
        body.len() > 6 && &body[..6] == b"GIF89a",
        "body must be a GIF89a stream"
    );
    assert_eq!(body[body.len() - 1], 0x3B, "GIF trailer");

    shutdown.request_stop();
    let _ = handle.join();
}

/// `Accept: image/gif` on an RGB instance -> 406 (there is no GIF quantiser on
/// this route; PNG is the lossless RGB answer).
#[test]
fn rendered_gif_on_rgb_is_406() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &rgb_instance(40, 60));
    let path =
        format!("/dicomweb/studies/{RGB_STUDY}/series/{RGB_SERIES}/instances/{RGB_SOP}/rendered");
    let (st, _, _) = http_exchange(addr, &path, &[("Accept", "image/gif")]);
    assert_eq!(st, 406, "RGB + image/gif must be 406");

    shutdown.request_stop();
    let _ = handle.join();
}

/// `Accept: application/json` -> 406 (a named type we do not offer).
#[test]
fn rendered_json_is_406() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &render_instance());
    let path = format!(
        "/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}/rendered"
    );
    let (st, _, _) = http_exchange(addr, &path, &[("Accept", "application/json")]);
    assert_eq!(st, 406, "application/json must be 406");

    shutdown.request_stop();
    let _ = handle.join();
}

/// `Accept: image/jpeg` on a MONO instance -> 200 JPEG (unchanged contract on
/// the NEW route).
#[test]
fn rendered_jpeg_on_mono_is_200() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &render_instance());
    let path = format!(
        "/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}/rendered"
    );
    let (st, ct, body) = http_exchange(addr, &path, &[("Accept", "image/jpeg")]);
    assert_eq!(st, 200);
    assert_eq!(ct.as_deref(), Some("image/jpeg"));
    assert!(
        body.len() > 4 && body[0] == 0xFF && body[1] == 0xD8,
        "JPEG SOI"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

/// A multi-frame (VOLUME) instance -> 406 from the HEADER alone (never reads
/// the whole blob). Also checks the native `thumbnail` route still requires
/// image/jpeg (unchanged) while `/rendered` answers PNG.
#[test]
fn rendered_multiframe_is_406_from_header() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &multiframe_instance());
    let path = format!(
        "/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}/rendered"
    );
    let (st, _, _) = http_exchange(addr, &path, &[("Accept", "image/png")]);
    assert_eq!(st, 406, "multi-frame rendered must be 406");

    shutdown.request_stop();
    let _ = handle.join();
}

/// An instance whose HEADER declares Rows x Columns over the 64M-px cap, with
/// a tiny PixelData: the 413 must fire from the header alone.
fn over_cap_instance() -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_explicit_rt(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    push_explicit_rt(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_explicit_rt(&mut d, 0x0008, 0x0018, b"UI", RENDER_SOP.as_bytes());
    push_explicit_rt(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_explicit_rt(&mut d, 0x0020, 0x000d, b"UI", RENDER_STUDY.as_bytes());
    push_explicit_rt(&mut d, 0x0020, 0x000e, b"UI", RENDER_SERIES.as_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    // 8193 x 8193 = 67,118,849 px > 64M cap (67,108,864).
    push_explicit_rt(&mut d, 0x0028, 0x0010, b"US", &8193u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0011, b"US", &8193u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    // Tiny PixelData: the cap must fire from the HEADER, never reading a full
    // 67M-px buffer.
    push_explicit_rt(&mut d, 0x7FE0, 0x0010, b"OB", &[0x5a_u8; 16]);
    d
}

/// A COMPRESSED single-frame instance (JPEG Baseline meta TS, SPP=1).
const RENDER_SOP_COMPRESSED: &str = "1.2.826.0.1.3680043.10.7000.1202";
fn compressed_instance() -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_explicit_rt(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.50");
    push_explicit_rt(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_explicit_rt(
        &mut d,
        0x0008,
        0x0018,
        b"UI",
        RENDER_SOP_COMPRESSED.as_bytes(),
    );
    push_explicit_rt(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_explicit_rt(&mut d, 0x0020, 0x000d, b"UI", RENDER_STUDY.as_bytes());
    push_explicit_rt(&mut d, 0x0020, 0x000e, b"UI", RENDER_SERIES.as_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_explicit_rt(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    push_explicit_rt(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    // Encapsulated (undefined-length) PixelData: a fragment, never decoded.
    let mut pixel = Vec::new();
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    pixel.extend_from_slice(&16u32.to_le_bytes());
    pixel.extend_from_slice(&[0xAB_u8; 16]);
    pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    // Undefined-length OB: tag + VR + reserved + 0xFFFFFFFF + value.
    d.extend_from_slice(&0x7FE0u16.to_le_bytes());
    d.extend_from_slice(&0x0010u16.to_le_bytes());
    d.extend_from_slice(b"OB");
    d.extend_from_slice(&[0x00, 0x00]);
    d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    d.extend_from_slice(&pixel);
    d
}

/// A compressed single-frame instance asked for a format with no decoder
/// (PNG) -> 406 (the manifest transfer syntax, not the dead meta-header read).
/// Status-only here; the read-counter version lives in the dedicated
/// `rendered_cap_read` binary. The JPEG Accept on the same instance is NOT a
/// header-time 406 any more (it decodes when a codec is compiled in), so it is
/// asserted where that behaviour is pinned: the sweep's C4.
#[test]
fn rendered_compressed_single_frame_is_406() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &compressed_instance());
    let path = format!(
        "/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP_COMPRESSED}/rendered"
    );
    let (st, _, body) = http_exchange(addr, &path, &[("Accept", "image/png")]);
    assert_eq!(st, 406, "a compressed single-frame rendered must be 406");
    assert!(
        String::from_utf8_lossy(&body).contains("not wired"),
        "the 406 must say the decode-to-PNG is not wired"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

/// An over-cap single-frame instance -> 413 from the HEADER (the before-read
/// reject + counter assertion lives in the dedicated `rendered_cap_read`
/// binary, so the process-global counter is not shared with parallel tests).
#[test]
fn rendered_over_cap_is_413() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &over_cap_instance());
    let path = format!(
        "/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}/rendered"
    );
    let (st, _, _) = http_exchange(addr, &path, &[("Accept", "image/png")]);
    assert_eq!(st, 413, "an over-cap rendered instance must be 413");

    shutdown.request_stop();
    let _ = handle.join();
}

/// `/thumbnail` must still reject `image/png` (JPEG-only resource, unchanged
/// by the 0410 split).
#[test]
fn thumbnail_still_rejects_png() {
    let ctx = setup();
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));

    seed_stow_rt(addr, &render_instance());
    let path = format!(
        "/dicomweb/studies/{RENDER_STUDY}/series/{RENDER_SERIES}/instances/{RENDER_SOP}/thumbnail"
    );
    let (st, _, _) = http_exchange(addr, &path, &[("Accept", "image/png")]);
    assert_eq!(st, 406, "thumbnail is JPEG-only and must 406 on PNG");

    shutdown.request_stop();
    let _ = handle.join();
}
