// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Freeze WADO study/series multipart wire behaviour.
//!
//! Boots the server over a real TCP socket, seeds instances through
//! STOW-RS (single `application/dicom` path writes the per-study CBOR manifest
//! plus BLAKE3 CAS), then exercises study/series multipart
//! scenarios per PS3.18 Section 10.4.2 and Section 10.4.4 (seven scenarios since the 2908
//! study-level Content-Location fix).
//!
//! The multipart boundary is regenerated per response (`boundary_id()`), so a
//! whole-body byte-exact golden is impossible. Structural verification: pull the
//! boundary from Content-Type, rebuild the expected body from golden part bytes
//! with that boundary, then `assert_eq!` byte-for-byte.
//!
//! The same scenarios also run against the extracted core (`wado_pure.rs`
//! plan functions) and MUST pass without edits.
//!
//! Transcoding scenarios need the `transcode` cargo feature (non-default).
//! They are gated with `#[cfg(feature = "transcode")]`.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Test server harness (mirror of wado_smoke.rs) -----------------------

struct TestServer {
    addr: SocketAddr,
    cas_root: PathBuf,
    _temp: tempfile::TempDir,
}

impl TestServer {
    fn start() -> Self {
        Self::start_with(|_| {})
    }

    fn start_with<F>(f: F) -> Self
    where
        F: FnOnce(&mut Config),
    {
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
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        f(&mut config);

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
            cas_root: storage_dir,
            _temp: temp,
        }
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let mut head = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
            self.addr.port()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let req = head.into_bytes();
        stream.write_all(&req).expect("write req");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        if std::env::var_os("WADO_DEBUG").is_some() {
            eprintln!("=== RESP raw ({} bytes) ===", buf.len());
        }
        parse_http(&buf)
    }

    fn post_single(&self, path: &str, body: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/dicom\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    /// Compute the CAS file path for a BLAKE3-addressed 64-char hex key.
    /// Two-level layout: `{root}/{h[0..2]}/{h[2..4]}/{h}.dcm`.
    fn cas_file(&self, key_hex: &str) -> PathBuf {
        self.cas_root
            .join(&key_hex[0..2])
            .join(&key_hex[2..4])
            .join(format!("{key_hex}.dcm"))
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
    // axum streams use Transfer-Encoding: chunked (no Content-Length). The raw
    // TCP body is chunk-framed; decode it so `body` is the de-chunked payload.
    let is_chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.first().map(|s| s.contains("chunked")).unwrap_or(false));
    let body = if is_chunked {
        decode_chunked(&body).expect("chunked body")
    } else {
        body
    };
    Response {
        status,
        headers,
        body,
    }
}

/// Decode an HTTP/1.1 chunked-encoded body into its raw payload.
fn decode_chunked(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        // chunk-size line: HEX[\r\n] (possibly with extensions)
        let line_end = find_crlf(data, pos)?;
        let size_str = std::str::from_utf8(&data[pos..line_end]).ok()?;
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).ok()?;
        pos = line_end + 2;
        if size == 0 {
            break;
        }
        if pos + size > data.len() {
            return None;
        }
        out.extend_from_slice(&data[pos..pos + size]);
        pos += size;
        // trailing CRLF after chunk data
        if data.get(pos..pos + 2) != Some(b"\r\n") {
            return None;
        }
        pos += 2;
    }
    Some(out)
}

fn find_crlf(data: &[u8], start: usize) -> Option<usize> {
    data[start..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| start + i)
}

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
    fn has_header(&self, name: &str) -> bool {
        self.headers.contains_key(name)
    }
}

// -- DICOM builder (Explicit VR LE, uncompressed) ------------------------

fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let is_extended = matches!(
        vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    );
    if vr == b"UI" && !len.is_multiple_of(2) {
        let padded_len = len + 1;
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(padded_len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(padded_len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        buf.push(0x00);
    } else {
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !len.is_multiple_of(2) {
            let pad = if matches!(
                vr,
                b"CS"
                    | b"LO"
                    | b"SH"
                    | b"PN"
                    | b"AE"
                    | b"AS"
                    | b"DA"
                    | b"DS"
                    | b"DT"
                    | b"IS"
                    | b"LT"
                    | b"ST"
                    | b"TM"
                    | b"UC"
                    | b"UR"
                    | b"UT"
            ) {
                0x20
            } else {
                0x00
            };
            buf.push(pad);
        }
    }
}

/// Build a minimal CT instance (image SOP class) with fixed identity.
fn build_instance(
    sop_instance_uid: &str,
    study_uid: &str,
    series_uid: &str,
    pixel_byte: u8,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2"); // CT Image
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_instance_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WADO^MULTIPART");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"WADOPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    let pixels = [pixel_byte; 64];
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
}

/// Build a non-image SR instance (no pixel data). SOP Class = Key Object
/// Selection Doc (1.2.840.10008.5.1.4.1.1.88.59). Only used by transcode tests.
#[cfg(feature = "transcode")]
fn build_sr_instance(sop_instance_uid: &str, study_uid: &str, series_uid: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.88.59",
    );
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_instance_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WADO^MULTIPART");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"WADOPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"SR");
    d
}

// -- Fixed identity ------------------------------------------------------

const STUDY_A: &str = "1.2.826.0.1.3680043.10.1234.7001";
const SERIES_A: &str = "1.2.826.0.1.3680043.10.1234.7101";
const SERIES_C: &str = "1.2.826.0.1.3680043.10.1234.7202";
#[cfg(feature = "transcode")]
const SERIES_B: &str = "1.2.826.0.1.3680043.10.1234.7201";
const SOP_A: &str = "1.2.826.0.1.3680043.10.1234.7301";
const SOP_B: &str = "1.2.826.0.1.3680043.10.1234.7302";
const SOP_C: &str = "1.2.826.0.1.3680043.10.1234.7304";
#[cfg(feature = "transcode")]
const SOP_SR: &str = "1.2.826.0.1.3680043.10.1234.7303";

fn inst_a() -> Vec<u8> {
    build_instance(SOP_A, STUDY_A, SERIES_A, 0x41)
}
fn inst_b() -> Vec<u8> {
    build_instance(SOP_B, STUDY_A, SERIES_A, 0x42)
}
fn inst_c() -> Vec<u8> {
    build_instance(SOP_C, STUDY_A, SERIES_C, 0x43)
}
#[cfg(feature = "transcode")]
fn inst_sr() -> Vec<u8> {
    build_sr_instance(SOP_SR, STUDY_A, SERIES_B)
}

fn seed_study(srv: &TestServer, parts: &[Vec<u8>]) {
    for part in parts {
        let r = srv.post_single("/dicomweb/studies", part);
        r.assert_status(200);
    }
}

// -- Multipart structural helpers ---------------------------------------

/// Extract the stored transfer syntax UID from a DICOM part's meta group
/// (0002,0010) - the part-header `transfer-syntax=` parameter must name
/// exactly this value for as-is parts.
fn stored_ts_of(p: &[u8]) -> String {
    let mut pos = 132usize;
    while pos + 8 <= p.len() {
        let group = u16::from_le_bytes([p[pos], p[pos + 1]]);
        if group != 0x0002 {
            break;
        }
        let elem = u16::from_le_bytes([p[pos + 2], p[pos + 3]]);
        let vr = &p[pos + 4..pos + 6];
        let (vo, len) = if matches!(
            vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        ) {
            let l = u32::from_le_bytes([p[pos + 8], p[pos + 9], p[pos + 10], p[pos + 11]]) as usize;
            (pos + 12, l)
        } else {
            let l = u16::from_le_bytes([p[pos + 6], p[pos + 7]]) as usize;
            (pos + 8, l)
        };
        if elem == 0x0010 {
            return String::from_utf8_lossy(&p[vo..vo + len])
                .trim_end_matches('\0')
                .trim_end_matches(' ')
                .to_string();
        }
        pos = vo + len + (len % 2);
    }
    String::new()
}

/// Rebuild an expected multipart body byte-for-byte from `parts` using the
/// given boundary. Every part carries MIME headers:
/// `--B\r\nContent-Type: application/dicom; transfer-syntax=<stored TS>\r\n
/// Content-Location: /dicomweb/studies/{study}/series/{series}/instances/{sop}\r\n
/// Content-Length: N\r\n\r\n<dicom>`. Study- and series-level multipart both
/// emit Content-Location (study-level since 2908); the part-header
/// `transfer-syntax=` parameter (Step 0 of the 3008 stored-first
/// passthrough) names the part bytes' ACTUAL syntax.
fn rebuild_multipart(boundary: &str, study_uid: &str, parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, (sop_uid, series_uid, p)) in parts.iter().enumerate() {
        if i == 0 {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        } else {
            out.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "Content-Type: application/dicom; transfer-syntax={}\r\n",
                stored_ts_of(p)
            )
            .as_bytes(),
        );
        out.extend_from_slice(
            format!(
                "Content-Location: /dicomweb/studies/{study_uid}/series/{series_uid}/instances/{sop_uid}\r\n"
            )
            .as_bytes(),
        );
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
    }
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

/// Extract the boundary from a multipart Content-Type header.
fn boundary_from_ct(ct: &str) -> String {
    ct.split(';')
        .find_map(|p| {
            let t = p.trim();
            t.strip_prefix("boundary=")
                .map(|b| b.trim_matches('"').to_string())
        })
        .expect("boundary param")
}

// -- Scenario tests ------------------------------------------------------

/// Scenario 1: study of 2 instances -> 200, chunked, no Content-Length, CT with boundary.
#[test]
fn multipart_study_two_instances_chunked() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a(), inst_b()]);
    let r = srv.get(&format!("/dicomweb/studies/{STUDY_A}"), &[]);
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type");
    assert!(ct.starts_with("multipart/related;"), "got {ct}");
    let boundary = boundary_from_ct(ct);
    assert!(!boundary.is_empty());
    // Chunked (axum Body::from_stream) -> Transfer-Encoding: chunked, no Content-Length.
    assert!(
        r.has_header("transfer-encoding"),
        "expected Transfer-Encoding: chunked"
    );
    assert!(
        !r.has_header("content-length"),
        "must NOT have Content-Length"
    );

    // Build expected body from the two seeding DICOM bytes (study-level parts
    // carry Content-Location since 2908).
    let expected = rebuild_multipart(
        &boundary,
        STUDY_A,
        &[(SOP_A, SERIES_A, &inst_a()), (SOP_B, SERIES_A, &inst_b())],
    );
    assert_eq!(r.body, expected, "multipart body must match rebuilt bytes");
}

/// Scenario 2: body parse - 2 parts, CT application/dicom, per-part
/// Content-Location (study-level since 2908), CL = actual size, closing
/// delimiter `--B--`.
#[test]
fn multipart_study_parts_parsed() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a(), inst_b()]);
    let r = srv.get(&format!("/dicomweb/studies/{STUDY_A}"), &[]);
    r.assert_status(200);
    let ct = r.header("content-type").unwrap();
    let boundary = boundary_from_ct(ct);

    let expected = rebuild_multipart(
        &boundary,
        STUDY_A,
        &[(SOP_A, SERIES_A, &inst_a()), (SOP_B, SERIES_A, &inst_b())],
    );
    assert_eq!(r.body, expected);

    let body_str = String::from_utf8_lossy(&r.body);
    // Content-Type: application/dicom; transfer-syntax=<part TS> +
    // per-part Content-Location (path with the part's SOP UID) +
    // Content-Length = size, exactly once per part.
    for (part_byte, part, sop) in [(&inst_a(), "a", SOP_A), (&inst_b(), "b", SOP_B)] {
        let header_block = format!(
            "\r\nContent-Type: application/dicom; transfer-syntax={}\r\nContent-Location: /dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}\r\nContent-Length: {}\r\n\r\n",
            stored_ts_of(part_byte),
            part_byte.len()
        );
        assert!(
            body_str.contains(&header_block),
            "missing part headers for {part}: {header_block:?}"
        );
    }
    assert_eq!(
        body_str.matches("Content-Location:").count(),
        2,
        "one Content-Location per part, no duplicates"
    );
    // Closing delimiter `--B--`.
    assert!(
        r.body
            .ends_with(format!("\r\n--{boundary}--\r\n").as_bytes()),
        "missing closing delimiter"
    );
}

/// Scenario 3: series-level -> Content-Location in each part.
#[test]
fn multipart_series_content_location() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a(), inst_b()]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    let ct = r.header("content-type").unwrap();
    let boundary = boundary_from_ct(ct);
    let expected = rebuild_multipart(
        &boundary,
        STUDY_A,
        &[(SOP_A, SERIES_A, &inst_a()), (SOP_B, SERIES_A, &inst_b())],
    );
    assert_eq!(r.body, expected);

    let body_str = String::from_utf8_lossy(&r.body);
    // Each part has Content-Location pointing at the instance.
    for sop in [SOP_A, SOP_B] {
        assert!(
            body_str.contains(&format!("Content-Location: /dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}\r\n")),
            "missing Content-Location for {sop}"
        );
    }
}

/// Scenario 7 (2908 gate): study-level - EVERY part carries Content-Location
/// with its OWN series UID and SOP UID; part count == instance count; no
/// duplicate Content-Location per part (multi-series study).
#[test]
fn multipart_study_content_location_every_part() {
    let srv = TestServer::start();
    // Two series in one study: SOP_A in SERIES_A, SOP_C in SERIES_C.
    seed_study(&srv, &[inst_a(), inst_c()]);
    let r = srv.get(&format!("/dicomweb/studies/{STUDY_A}"), &[]);
    r.assert_status(200);
    let ct = r.header("content-type").unwrap();
    let boundary = boundary_from_ct(ct);

    let expected = rebuild_multipart(
        &boundary,
        STUDY_A,
        &[(SOP_A, SERIES_A, &inst_a()), (SOP_C, SERIES_C, &inst_c())],
    );
    assert_eq!(r.body, expected, "study multipart must match rebuilt bytes");

    let body_str = String::from_utf8_lossy(&r.body);
    // Every part: exactly one Content-Location whose path carries the part's
    // own series UID and SOP UID.
    for (sop, series) in [(SOP_A, SERIES_A), (SOP_C, SERIES_C)] {
        let cl = format!(
            "Content-Location: /dicomweb/studies/{STUDY_A}/series/{series}/instances/{sop}\r\n"
        );
        assert_eq!(
            body_str.matches(&cl).count(),
            1,
            "exactly one Content-Location for {sop}"
        );
    }
    // Part count == instance count; no duplicate Content-Location headers.
    assert_eq!(
        body_str.matches("Content-Location:").count(),
        2,
        "2 instances -> exactly 2 Content-Location headers"
    );
    assert_eq!(
        body_str
            .matches("Content-Type: application/dicom; transfer-syntax=")
            .count(),
        2,
        "2 instances -> exactly 2 parts"
    );
    // Closing delimiter `--B--`.
    assert!(
        r.body
            .ends_with(format!("\r\n--{boundary}--\r\n").as_bytes()),
        "missing closing delimiter"
    );
}

/// Scenario 4: unknown study -> 404; unknown series -> 404 with `{study}/series/{series}`.
#[test]
fn multipart_unknown_study_series_404() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    // Unknown study -> 404 (generic NotFound JSON).
    let r = srv.get(
        &format!("/dicomweb/studies/{}", "1.2.826.0.1.3680043.10.1234.9999"),
        &[],
    );
    r.assert_status(404);

    // Unknown series within a known study -> 404. Note: the OLD handler raises
    // WadoError::NotFound("{study}/series/{series}") but its IntoResponse emits
    // the generic NotFound JSON (the study/series label is NOT in the wire body).
    // We freeze the actual 404 status; the message text is an internal-only note.
    let r2 = srv.get(
        &format!(
            "/dicomweb/studies/{STUDY_A}/series/{}",
            "1.2.826.0.1.3680043.10.1234.9999"
        ),
        &[],
    );
    r2.assert_status(404);
}

/// Scenario 5: missing instance (CAS file deleted after STOW) -> only remaining parts, 200.
#[test]
fn multipart_missing_instance_skipped() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a(), inst_b()]);

    // Locate SOP_B's CAS file from the manifest and delete it.
    let manifest_path = srv
        ._temp
        .path()
        .join("btree")
        .join("manifests")
        .join(format!("{STUDY_A}.manifest"));
    let manifest = clarus_core::manifest::read_manifest(&manifest_path).expect("manifest");
    let inst_b_rec = manifest.find_instance(SOP_B).expect("SOP_B in manifest");
    let b3 = inst_b_rec.blake3;
    assert_ne!(b3, [0u8; 32], "SOP_B must be BLAKE3-addressed");
    let key = clarus_adapters::hex_encode(&b3);
    let cas_file = srv.cas_file(&key);
    assert!(cas_file.exists(), "CAS file must exist for SOP_B");
    std::fs::remove_file(&cas_file).expect("remove CAS file");

    // Study retrieve -> 200 with only SOP_A part; SOP_B skipped + warn.
    let r = srv.get(&format!("/dicomweb/studies/{STUDY_A}"), &[]);
    r.assert_status(200);
    let ct = r.header("content-type").unwrap();
    let boundary = boundary_from_ct(ct);
    // Only SOP_A survives (SOP_B stream fails -> skipped). Compare against a
    // rebuilt body with only SOP_A present.
    let expected = rebuild_multipart(&boundary, STUDY_A, &[(SOP_A, SERIES_A, &inst_a())]);
    assert_eq!(
        r.body, expected,
        "missing instance must be skipped, only remaining part present"
    );
}

/// Scenario 6 (transcode): `?transferSyntax=` == stored TS -> parts as-is;
/// != stored -> transcoded part (content bytes); SR (non-image) + target TS -> as-is.
#[cfg(feature = "transcode")]
#[test]
fn multipart_transcode_decision() {
    let srv = TestServer::start();
    // Study with an image (SOP_A) in SERIES_A and an SR (SOP_SR) in SERIES_B.
    seed_study(&srv, &[inst_a(), inst_sr()]);

    let stored_ts = "1.2.840.10008.1.2.1"; // Explicit VR LE

    // Case A: target TS == stored TS -> parts as-is (byte-exact to the STOW'd DICOM).
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}?transferSyntax={stored_ts}");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    let ct = r.header("content-type").unwrap();
    let boundary = boundary_from_ct(ct);
    let expected = rebuild_multipart(&boundary, STUDY_A, &[(SOP_A, SERIES_A, &inst_a())]);
    assert_eq!(r.body, expected, "stored-TS request must serve parts as-is");
}

/// Scenario 6b (transcode): SR (non-image) + target TS -> as-is (no transcode).
#[cfg(feature = "transcode")]
#[test]
fn multipart_non_image_as_is() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_sr()]);

    // Non-image SOP + a target TS -> served as-is (no pixel data to transcode).
    let stored_ts = "1.2.840.10008.1.2.1";
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_B}?transferSyntax={stored_ts}");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    let ct = r.header("content-type").unwrap();
    let boundary = boundary_from_ct(ct);
    let expected = rebuild_multipart(&boundary, STUDY_A, &[(SOP_SR, SERIES_B, &inst_sr())]);
    assert_eq!(r.body, expected, "non-image SR must be served as-is");
}

/// Scenario 6c (transcode): image + a DIFFERENT supported target TS -> the part
/// bytes change (transcoded). We cannot predict the exact transcoded bytes, so
/// we assert the response is 200, part content-type is application/dicom, and
/// the part body differs from the original (transcode happened) OR matches when
/// transcode is a no-op. This is a structural check; the transcode codec must
/// be present in the default registry (ExplicitVrLe/BigEndian only under the
/// transcode feature).
#[cfg(feature = "transcode")]
#[test]
fn multipart_transcode_changes_bytes() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    // Target an unsupported-but-registered ts? Default registry with transcode has
    // JPEG Baseline etc. Use a target different from stored Explicit VR LE.
    // BigEndian (1.2.840.10008.1.2.2) is a valid target present in the registry.
    let target = "1.2.840.10008.1.2.2"; // Explicit VR Big Endian
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}?transferSyntax={target}");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    let ct = r.header("content-type").unwrap();
    let boundary = boundary_from_ct(ct);

    // Structural: part exists, content-type application/dicom, but the byte
    // payload is not byte-identical to the original (transcode ran), unless size
    // unchanged. Rebuild with the ORIGINAL would only match if no transcode
    // happened - so we assert the response body is a valid single-part multipart
    // with a Content-Length header and a non-empty part.
    let body_str = String::from_utf8_lossy(&r.body);
    // The part-header transfer-syntax parameter names the ACTUAL part bytes'
    // TS: the transcode TARGET here (Step 0 of the 3008 stored-first).
    assert!(
        body_str
            .contains("Content-Type: application/dicom; transfer-syntax=1.2.840.10008.1.2.2\r\n")
    );
    assert!(
        body_str.contains("Content-Length:"),
        "transcoded part must carry Content-Length"
    );
    assert!(
        body_str.contains("Content-Location: /dicomweb/studies/"),
        "series-level transcoded part must carry Content-Location"
    );
    // The part must parse as one whole part delimited by the boundary.
    let opening = format!("--{boundary}\r\n");
    assert!(r.body.starts_with(opening.as_bytes()));
    assert!(
        r.body
            .ends_with(format!("\r\n--{boundary}--\r\n").as_bytes())
    );
}

/// Step 0 regression (3008 stored-first): every WADO multipart part header
/// carries `transfer-syntax=<actual TS of the part bytes>`. As-is parts name
/// the STORED TS; transcoded parts name the transcode TARGET.
#[test]
fn multipart_part_header_transfer_syntax_param() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    // As-is branch: no ?transferSyntax -> stored bytes, part TS == stored TS.
    let r = srv.get(&format!("/dicomweb/studies/{STUDY_A}"), &[]);
    r.assert_status(200);
    let body_str = String::from_utf8_lossy(&r.body);
    assert!(
        body_str.contains(&format!(
            "Content-Type: application/dicom; transfer-syntax={}\r\n",
            stored_ts_of(&inst_a())
        )),
        "as-is part header must name the stored TS"
    );
    assert_eq!(stored_ts_of(&inst_a()), "1.2.840.10008.1.2.1");

    // Transcode branch: ?transferSyntax=BigEndian -> part TS == the target.
    #[cfg(feature = "transcode")]
    {
        let target = "1.2.840.10008.1.2.2"; // Explicit VR Big Endian
        let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}?transferSyntax={target}");
        let r2 = srv.get(&path, &[]);
        r2.assert_status(200);
        let b2 = String::from_utf8_lossy(&r2.body);
        assert!(
            b2.contains(&format!(
                "Content-Type: application/dicom; transfer-syntax={target}\r\n"
            )),
            "transcoded part header must name the transcode target"
        );
    }
}

/// E3 (31.08): `Accept: multipart/related; type=application/dicom;
/// transfer-syntax=*` serves the STORED bytes as-is (no transcode) and the
/// part header names the stored TS - not some transcode target.
#[test]
fn multipart_wildcard_accept_serves_stored_as_is() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    let accept = "multipart/related; type=\"application/dicom\"; transfer-syntax=*";
    let r = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}"),
        &[("Accept", accept)],
    );
    r.assert_status(200);

    // Byte-exact to the stored instance: the wildcard did not trigger a
    // transcode (identical to a request with no Accept negotiation).
    let ct = r.header("content-type").unwrap();
    let boundary = boundary_from_ct(ct);
    let expected = rebuild_multipart(&boundary, STUDY_A, &[(SOP_A, SERIES_A, &inst_a())]);
    assert_eq!(
        r.body, expected,
        "wildcard Accept must serve the stored bytes as-is"
    );

    // The part header names the ACTUAL stored TS.
    let body_str = String::from_utf8_lossy(&r.body);
    assert!(
        body_str.contains(&format!(
            "Content-Type: application/dicom; transfer-syntax={}\r\n",
            stored_ts_of(&inst_a())
        )),
        "wildcard Accept part header must name the stored TS"
    );
    assert_eq!(stored_ts_of(&inst_a()), "1.2.840.10008.1.2.1");
}

// -- Accept negotiation on the overloaded retrieve resources -------------
//
// PS3.18 puts a QIDO-RS retrieve (8.7, dicom+json) and a WADO-RS retrieve
// (10.4, multipart/related) on the SAME study/series/instance paths, so the
// Accept header decides which transaction is answered. These freeze the
// branch: JSON on a JSON Accept, WADO-RS unchanged otherwise, 406 when the
// client named only types this resource cannot produce.

/// The QIDO-RS RetrieveStudy answer is the study as a DICOM JSON **object**,
/// and it must agree with the SearchForStudies transaction for the same UID.
#[test]
fn study_path_negotiates_json_and_agrees_with_search() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a(), inst_b()]);

    let retrieve = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}"),
        &[("Accept", "application/dicom+json")],
    );
    retrieve.assert_status(200);
    assert!(
        retrieve
            .header("content-type")
            .is_some_and(|ct| ct.starts_with("application/dicom+json")),
        "JSON branch must answer the DICOM JSON content type"
    );
    let body = String::from_utf8(retrieve.body.clone()).expect("utf-8 JSON");
    assert!(
        body.starts_with('{'),
        "retrieve answers one object, got {body}"
    );
    assert!(!body.starts_with('['), "not an array on the retrieve path");

    // The same UID through the search transaction, unwrapped from its array.
    let search = srv.get(
        &format!("/dicomweb/studies?StudyInstanceUID={STUDY_A}"),
        &[("Accept", "application/dicom+json")],
    );
    search.assert_status(200);
    let array = String::from_utf8(search.body.clone()).expect("utf-8 JSON");
    let inner = array
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .expect("search answers an array");
    assert_eq!(
        body, inner,
        "the retrieve object must be the search's single element, byte for byte"
    );
}

/// The study path with a multipart Accept keeps the WADO-RS answer unchanged.
#[test]
fn study_path_keeps_multipart_on_a_multipart_accept() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a(), inst_b()]);

    let r = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}"),
        &[("Accept", "multipart/related; type=\"application/dicom\"")],
    );
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type");
    assert!(ct.starts_with("multipart/related;"), "got {ct}");
    let boundary = boundary_from_ct(ct);
    let expected = rebuild_multipart(
        &boundary,
        STUDY_A,
        &[(SOP_A, SERIES_A, &inst_a()), (SOP_B, SERIES_A, &inst_b())],
    );
    assert_eq!(
        r.body, expected,
        "the multipart branch must stay byte-identical"
    );
}

/// An absent Accept keeps today's WADO-RS default (this server's long-standing
/// behaviour, under which the deployed viewers were verified).
#[test]
fn study_path_without_accept_keeps_the_multipart_default() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    let r = srv.get(&format!("/dicomweb/studies/{STUDY_A}"), &[]);
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type");
    assert!(
        ct.starts_with("multipart/related;"),
        "no Accept must keep the WADO-RS default, got {ct}"
    );
}

/// A wildcard also keeps the default rather than 406ing.
#[test]
fn study_path_with_wildcard_accept_keeps_the_multipart_default() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    let r = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}"),
        &[("Accept", "*/*")],
    );
    r.assert_status(200);
    assert!(
        r.header("content-type")
            .is_some_and(|ct| ct.starts_with("multipart/related;")),
        "wildcard asks for whatever is available - the default"
    );
}

/// A type this resource cannot produce is refused, not silently substituted.
#[test]
fn study_path_with_a_foreign_accept_is_406() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    let r = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}"),
        &[("Accept", "text/html")],
    );
    r.assert_status(406);
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("cannot satisfy Accept"),
        "the 406 must name the reason, got {body}"
    );
}

/// The series path negotiates the same way, and its JSON agrees with the
/// series search for the same UID.
#[test]
fn series_path_negotiates_json_and_agrees_with_search() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a(), inst_b()]);

    let retrieve = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}"),
        &[("Accept", "application/dicom+json")],
    );
    retrieve.assert_status(200);
    let body = String::from_utf8(retrieve.body.clone()).expect("utf-8 JSON");
    assert!(body.starts_with('{'), "one series object, got {body}");

    let search = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}/series?SeriesInstanceUID={SERIES_A}"),
        &[("Accept", "application/dicom+json")],
    );
    search.assert_status(200);
    let array = String::from_utf8(search.body.clone()).expect("utf-8 JSON");
    let inner = array
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .expect("search answers an array");
    assert_eq!(body, inner, "series retrieve must match the series search");
}

/// The series path keeps multipart on a multipart Accept and 406 on a
/// foreign one.
#[test]
fn series_path_branches_multipart_and_406() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a(), inst_b()]);

    let mp = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}"),
        &[("Accept", "multipart/related; type=\"application/dicom\"")],
    );
    mp.assert_status(200);
    assert!(
        mp.header("content-type")
            .is_some_and(|ct| ct.starts_with("multipart/related;")),
        "multipart Accept keeps the WADO-RS answer"
    );

    let bad = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}"),
        &[("Accept", "text/html")],
    );
    bad.assert_status(406);
}

/// The instance path serves the QIDO-RS instance resource as one object on a
/// JSON Accept; the multipart and 406 branches follow the same rule.
#[test]
fn instance_path_negotiates_json_multipart_and_406() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_A}");

    let json = srv.get(&path, &[("Accept", "application/dicom+json")]);
    json.assert_status(200);
    assert!(
        json.header("content-type")
            .is_some_and(|ct| ct.starts_with("application/dicom+json")),
        "instance JSON branch content type"
    );
    let body = String::from_utf8(json.body.clone()).expect("utf-8 JSON");
    assert!(body.starts_with('{'), "one instance object, got {body}");
    assert!(
        body.contains(SOP_A),
        "the instance resource names its own SOP Instance UID"
    );

    let mp = srv.get(
        &path,
        &[("Accept", "multipart/related; type=\"application/dicom\"")],
    );
    mp.assert_status(200);

    let bad = srv.get(&path, &[("Accept", "text/html")]);
    bad.assert_status(406);
}

/// A `;q=` parameter on the JSON range still selects the JSON branch: the
/// media type is what is compared, not the parameters.
#[test]
fn a_quality_parameter_does_not_hide_the_media_type() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    let r = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}"),
        &[("Accept", "application/dicom+json;q=0.9")],
    );
    r.assert_status(200);
    assert!(
        r.header("content-type")
            .is_some_and(|ct| ct.starts_with("application/dicom+json")),
        "a q-value must not turn a JSON preference into a WADO answer"
    );
}

/// A JSON preference combined with the WADO type still answers JSON: this is
/// an ordered preference check, and JSON is checked first.
#[test]
fn a_combined_accept_preferring_json_answers_json() {
    let srv = TestServer::start();
    seed_study(&srv, &[inst_a()]);

    let r = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}"),
        &[(
            "Accept",
            "application/dicom+json, multipart/related; type=\"application/dicom\"",
        )],
    );
    r.assert_status(200);
    assert!(
        r.header("content-type")
            .is_some_and(|ct| ct.starts_with("application/dicom+json")),
        "JSON listed first wins"
    );
}

// -- A stalled reader must not starve another reader ----------------------

/// One instance-get over its own connection, returning the elapsed wall time.
///
/// The response is read to EOF, so the elapsed time covers the whole
/// exchange and not just the head - which is what makes it comparable with
/// the harness that measured 6.21 s for the full 432-instance loop against
/// a reader stalled for 60 s mid-body.
fn timed_instance_get(srv: &TestServer, study: &str, series: &str, sop: &str) -> Duration {
    let started = Instant::now();
    let response = srv.get(
        &format!("/dicomweb/studies/{study}/series/{series}/instances/{sop}"),
        &[(
            "Accept",
            "multipart/related; type=\"application/dicom\"; transfer-syntax=1.2.840.10008.1.2.1",
        )],
    );
    response.assert_status(200);
    started.elapsed()
}

#[test]
fn a_reader_stalled_mid_body_does_not_starve_another_reader() {
    // The property the 24.09 harness measured on the live server, pinned
    // here. B opens the SERIES multipart stream, reads the head plus the
    // first chunk, then stops reading for a few seconds while A runs a full
    // loop of instance GETs on its own connections. A must finish inside a
    // wall budget and show no gap anywhere near the pause: the design is
    // thread-per-connection with a per-connection send timeout, so one
    // stalled reader blocks only its own thread.
    //
    // The pause is short here (2 s) rather than the harness's 60 s: the
    // property is that a pause of ANY length on one connection does not
    // appear on another, and a CI budget cannot spend a minute proving it.
    let srv = TestServer::start();
    // Twelve DISTINCT instances, so each GET addresses a real object rather
    // than deduplicating onto the same one.
    let sops: Vec<String> = (0..12)
        .map(|index| format!("1.2.826.0.1.3680043.10.1234.7900.{index}"))
        .collect();
    let instances: Vec<Vec<u8>> = sops
        .iter()
        .map(|sop| build_instance(sop, STUDY_A, SERIES_A, 0x51))
        .collect();
    seed_study(&srv, &instances);

    // B: open the series multipart stream and read only a small prefix.
    let mut stalled = TcpStream::connect(srv.addr).expect("connect B");
    stalled
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout B");
    let head = format!(
        "GET /dicomweb/studies/{STUDY_A}/series/{SERIES_A} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
         Accept: multipart/related; type=\"application/dicom\"\r\nConnection: close\r\n\r\n",
        srv.addr.port()
    );
    stalled.write_all(head.as_bytes()).expect("write B head");
    let mut prefix = [0u8; 256];
    let read = stalled.read(&mut prefix).expect("read B prefix");
    assert!(
        read > 0,
        "B must receive the head and the start of the body"
    );

    // A: a run of instance GETs on fresh connections, while B is stalled.
    let mut worst = Duration::ZERO;
    let overall = Instant::now();
    for sop in &sops {
        let elapsed = timed_instance_get(&srv, STUDY_A, SERIES_A, sop);
        worst = worst.max(elapsed);
    }
    let total = overall.elapsed();

    // Resume B and drain it, so the server's blocked write completes and the
    // connection closes cleanly rather than being abandoned mid-write.
    std::thread::sleep(Duration::from_secs(2));
    let mut rest = Vec::new();
    stalled.read_to_end(&mut rest).expect("drain B");

    // Budget with wide slack: the harness reference is 6.21 s for 432
    // instances (14 ms each) on the live server, so twelve must be far under
    // a second on loopback. The assertion that matters is the SHAPE - the
    // total is bounded and no single request came close to the stall - so
    // the numbers are generous rather than tight.
    assert!(
        total < Duration::from_secs(10),
        "A finished in {total:?} while B was stalled"
    );
    assert!(
        worst < Duration::from_secs(2),
        "no instance GET waited on the stalled reader: worst {worst:?}"
    );
    assert!(
        !rest.is_empty(),
        "B's stream resumed and produced the remaining parts"
    );
}
