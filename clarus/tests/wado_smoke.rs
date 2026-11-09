// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Freeze WADO-RS single-object wire behaviour.
//!
//! Boots the server over a real TCP socket, seeds instances through
//! STOW-RS (which populates CAS + the per-study CBOR manifest), then
//! exercises eleven WADO scenarios (PS3.18 Section 10.4). Deterministic scenarios
//! capture a byte-exact golden fixture (written once during the initial freeze);
//! the rest assert status + named headers + structural properties.
//!
//! The same scenarios also run against `wado_pure.rs`,
//! commit 2) and MUST pass without edits - the fixtures lock in the
//! pre-extraction bytes.
//!
//! Transcoding and thumbnail scenarios need the `transcode` cargo feature
//! (non-default). They are gated with `#[cfg(feature = "transcode")]` so the
//! default `cargo test --all --offline` build stays green; the full 11-scenario
//! freeze runs with `cargo test -p clarus --test wado_smoke --features transcode`.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::handlers::json_common::parse_json;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Fixture helpers -----------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("wado")
}

/// Assert `actual` is byte-exact with a committed golden fixture, unless the
/// `CLARUS_WADO_GENERATE` env var is set (initial freeze only - writes the
/// fixture from the OLD code's output). Once committed, that var is never set
/// in CI, so this becomes a read-only byte-exact check.
fn check_or_generate(name: &str, actual: &[u8]) {
    let path = fixtures_dir().join(name);
    if std::env::var_os("CLARUS_WADO_GENERATE").is_some() {
        std::fs::create_dir_all(fixtures_dir()).expect("wado fixtures dir");
        std::fs::write(&path, actual).expect("write wado fixture");
        return;
    }
    let expected =
        std::fs::read(&path).unwrap_or_else(|_| panic!("fixture missing: {}", path.display()));
    assert_eq!(actual, &expected, "wado fixture drift in {name}");
}

// -- Minimal DICOM builder (Explicit VR LE, uncompressed pixels) ---------

/// Append an Explicit VR DICOM element (PS3.5 Section 7.1).
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

/// Build a minimal Explicit VR LE DICOM instance with a fixed identity and
/// 8x8 monochrome pixel data (64 non-zero bytes). Deterministic golden.
fn build_instance(sop_uid: &str, patient_id: &str, study_uid: &str, series_uid: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WADO^SMOKE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", patient_id.as_bytes());
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
    // 8x8 = 64 bytes of a constant pixel value -> deterministic thumbnail.
    let pixels = [0x5a_u8; 64];
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
}

// -- Test server harness -------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    manifests_dir: PathBuf,
    #[allow(dead_code)]
    pool: clarus_server::state::TranscodePool,
    _temp: tempfile::TempDir,
}

const STOW_JSON_CT: &str = "application/dicom+json; charset=utf-8";

impl TestServer {
    /// Boot the full router with preflight disabled for determinism.
    fn start() -> Self {
        Self::start_with(|_| {})
    }

    /// Same, but lets the caller override config.
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
            // Default registry (not empty) so the WADO transcode path can
            // recognize the stored Explicit VR LE syntax (`?transferSyntax=`).
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
            // Pool size follows the config so a test can shrink it via
            // `start_with` and occupy the single permit for the busy-skip test.
            transcode_pool: clarus_server::state::TranscodePool::new(
                config.transcode.max_concurrent,
            ),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };
        let pool = state.transcode_pool.clone();

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
            manifests_dir: manifest_dir,
            pool,
            _temp: temp,
        }
    }

    fn post(&self, path: &str, content_type: &str, body: &[u8]) -> Response {
        self.request("POST", path, Some(content_type), &[], body)
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        self.request("GET", path, None, headers, &[])
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
            self.addr.port()
        );
        if let Some(ct) = content_type {
            head.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut req = head.into_bytes();
        req.extend_from_slice(body);
        stream.write_all(&req).expect("write req");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        if std::env::var_os("WADO_DEBUG").is_some() {
            eprintln!(
                "=== RESP raw ({} bytes) head: {:?}",
                buf.len(),
                &buf[..buf.len().min(300)]
            );
        }
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
}

/// Seed instances via the SINGLE application/dicom STOW path (writes the
/// per-study CBOR manifest + BLAKE3-addressed CAS, both of which WADO reads).
fn seed_stow(srv: &TestServer, parts: &[&[u8]]) {
    for part in parts {
        let r = srv.post("/dicomweb/studies", "application/dicom", part);
        // These fixtures carry empty Type 2 fields, so the store coerces them
        // and answers 202 "stored with warnings" (PS3.18 10.5.2). The seed
        // still succeeded - that is what this helper asserts.
        assert!(
            r.status == 200 || r.status == 202,
            "seed STOW must be accepted, got {}",
            r.status
        );
        r.assert_content_type(STOW_JSON_CT);
    }
}

/// Seed instances via the MULTIPART STOW path (one part per instance). Since
/// this path also writes the study
/// manifest, so WADO reads the instances back just like the bytes path.
fn seed_stow_multipart(srv: &TestServer, parts: &[&[u8]]) {
    const MB: &str = "----clarus_wado_multipart_2a1c";
    let body = multipart_body(MB, parts);
    let ct = format!("multipart/related; type=\"application/dicom\"; boundary={MB}");
    let r = srv.post("/dicomweb/studies", &ct, &body);
    // See `seed_stow`: a coerced store is "stored with warnings" (202).
    assert!(
        r.status == 200 || r.status == 202,
        "seed STOW must be accepted, got {}",
        r.status
    );
    r.assert_content_type(STOW_JSON_CT);
}

fn multipart_body(boundary: &str, parts: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: application/dicom\r\nContent-Length: {}\r\n\r\n",
                part.len()
            )
            .as_bytes(),
        );
        body.extend_from_slice(part);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

// -- Fixed identity ------------------------------------------------------
//
// WADO UID validation (validate_dicom_uid) permits only ASCII digits and
// dots - letters would yield a 400. All identifiers below are numeric-only.

const STUDY_A: &str = "1.2.826.0.1.3680043.10.1234.6001";
const SERIES_A: &str = "1.2.826.0.1.3680043.10.1234.6101";
const SOP_A: &str = "1.2.826.0.1.3680043.10.1234.6201";

fn sop_a() -> Vec<u8> {
    build_instance(SOP_A, "WADOPAT01", STUDY_A, SERIES_A)
}

const NESTED_SQ_STUDY: &str = "1.2.826.0.1.3680043.10.1234.7001";
const NESTED_SQ_SERIES: &str = "1.2.826.0.1.3680043.10.1234.7101";
const NESTED_SQ_SOP: &str = "1.2.826.0.1.3680043.10.1234.7201";

/// Build an instance whose top-level (0008,1198) SQ contains one item that
/// itself carries a nested (0008,0100) CodeSequence item. Exercises the
/// recursive `/metadata` SQ renderer end-to-end over the wire.
fn build_instance_nested_sq() -> Vec<u8> {
    // Inner CodeSequence value: one defined-length item with (0008,0100) SH.
    let mut inner_item = Vec::new();
    append_element(&mut inner_item, 0x0008, 0x0100, b"SH", b"NESTED^LEAF");
    let mut code_seq = Vec::new();
    append_item(&mut code_seq, &inner_item);

    // Content item: a UT value plus the nested CodeSequence element.
    let mut content_item = Vec::new();
    append_element(&mut content_item, 0x0040, 0xA160, b"UT", b"REF^CODE");
    append_element(&mut content_item, 0x0008, 0x0100, b"SQ", &code_seq);

    let mut content_seq = Vec::new();
    append_item(&mut content_seq, &content_item);

    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.88.33",
    );
    append_element(&mut d, 0x0008, 0x0018, b"UI", NESTED_SQ_SOP.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WADO^SQ");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"WADOSQ01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", NESTED_SQ_STUDY.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", NESTED_SQ_SERIES.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"SR");
    // Top-level SQ with the nested item stream.
    append_element(&mut d, 0x0008, 0x1198, b"SQ", &content_seq);
    d
}

/// Append a defined-length item header carrying `body` (already padded).
fn append_item(buf: &mut Vec<u8>, body: &[u8]) {
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
    buf.extend_from_slice(&0xE000u16.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

/// Like `build_instance`, but with caller-controlled Rows/Columns (native
/// 8-bit monochrome pixels). Used by the high-bit geometry regression.
fn build_instance_geometry(
    sop_uid: &str,
    patient_id: &str,
    study_uid: &str,
    series_uid: &str,
    rows: u16,
    cols: u16,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WADO^GEOM");
    append_element(&mut d, 0x0010, 0x0020, b"LO", patient_id.as_bytes());
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    let pixels = vec![0x5a_u8; rows as usize * cols as usize];
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
}

/// Build a SINGLE-FRAME ENCAPSULATED instance - PixelData OB with undefined
/// length, an empty Basic Offset Table Item, one frame Item carrying a fake
/// SOI-bearing bitstream, and the sequence delimiter. The frames endpoint
/// must serve exactly the frame Item bytes, never the Item wrappers.
const SOP_B: &str = "1.2.826.0.1.3680043.10.1234.6202";
/// Single-frame encapsulated RLE (1.2.840.10008.1.2.5) frame-label target.
const SOP_C: &str = "1.2.826.0.1.3680043.10.1234.6203";
/// Single-frame encapsulated JPEG-LS (1.2.840.10008.1.2.4.70) frame-label target.
const SOP_D: &str = "1.2.826.0.1.3680043.10.1234.6204";
/// Single-frame encapsulated JPEG Baseline (1.2.840.10008.1.2.4.50) label target
/// (a distinct SOP from SOP_B so a test can host exactly one instance).
const SOP_E: &str = "1.2.826.0.1.3680043.10.1234.6205";
/// multiframe encapsulated RLE whole-instance render/thumbnail target.
const SOP_W4: &str = "1.2.826.0.1.3680043.10.1234.6206";
/// The SINGLE-FRAME RLE instance (14.10: compressed /rendered must decode).
#[cfg(feature = "transcode")]
const SOP_W5: &str = "1.2.826.0.1.3680043.10.1234.6207";

fn fake_jpeg_bitstream() -> Vec<u8> {
    // Minimal SOI..EOI JPEG skeleton - the bytes the browser decoder must see.
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    j.extend_from_slice(b"JFIF");
    j.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    j.extend_from_slice(&[0xFF, 0xD9]);
    j
}

fn build_encapsulated_single_frame(sop_uid: &str) -> Vec<u8> {
    // Single-frame encapsulated JPEG Baseline (kept for the historical
    // callers/regressions that expect exactly TS 1.2.840.10008.1.2.4.50).
    build_encapsulated_frame(sop_uid, b"1.2.840.10008.1.2.4.50", &fake_jpeg_bitstream())
}

/// Build a SINGLE-FRAME ENCAPSULATED instance under caller-chosen transfer
/// syntax `ts_uid` with a caller-chosen opaque frame `payload`. The manifest's
/// stored TS comes from (0002,0010), so the server labels the /frames part
/// from `ts_uid` regardless of the payload bytes - this lets the 02.09 W1
/// media/transfer-syntax labeling tests exercise JPEG (4.50), RLE (1.2.5)
/// and JPEG-LS (4.70) without embedding real codec streams.
fn build_encapsulated_frame(sop_uid: &str, ts_uid: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(512);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", ts_uid);
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"ATR^ENCAP");
    append_element(&mut d, 0x0020, 0x000d, b"UI", STUDY_A.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", SERIES_A.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &64u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &64u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    // PixelData (OB) - undefined length, encapsulated (PS3.5 Table A.4-1).
    d.extend_from_slice(&0x7FE0u16.to_le_bytes());
    d.extend_from_slice(&0x0010u16.to_le_bytes());
    d.extend_from_slice(b"OB");
    d.extend_from_slice(&[0x00, 0x00]);
    d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    // Basic Offset Table Item (empty).
    d.extend_from_slice(&0xFFFEu16.to_le_bytes());
    d.extend_from_slice(&0xE000u16.to_le_bytes());
    d.extend_from_slice(&0u32.to_le_bytes());
    // Frame Item #1 - the opaque payload.
    d.extend_from_slice(&0xFFFEu16.to_le_bytes());
    d.extend_from_slice(&0xE000u16.to_le_bytes());
    d.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    d.extend_from_slice(payload);
    // Sequence Delimiter Item.
    d.extend_from_slice(&0xFFFEu16.to_le_bytes());
    d.extend_from_slice(&0xE0DDu16.to_le_bytes());
    d.extend_from_slice(&0u32.to_le_bytes());
    d
}

fn instance_path() -> String {
    format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_A}")
}
fn metadata_path() -> String {
    format!("{}/metadata", instance_path())
}
fn frames_path(frame: &str) -> String {
    format!("{}/frames/{frame}", instance_path())
}
fn bulkdata_path(tag: &str) -> String {
    format!("{}/bulkdata/{tag}", instance_path())
}
#[cfg(feature = "transcode")]
fn thumbnail_path() -> String {
    format!("{}/thumbnail", instance_path())
}

/// Build a COMPRESSED (JPEG Baseline, 1.2.840.10008.1.2.4.50) instance
/// at runtime: the same constant-pixel identity as `build_instance`, but
/// 64x64 (the thumbnail path NEVER upscales, so a 64x64 source is required
/// for the literal 64x64 output assertion), the pixels JPEG-encoded with the
/// in-tree `jpeg_encoder` (the same encoder the adapters JPEG Baseline codec
/// wraps) and stored as an encapsulated PixelData element. `color` builds a
/// 3-plane RGB instance for the SPP != 1 -> 406 test.
#[cfg(feature = "transcode")]
fn build_compressed_instance(
    sop_uid: &str,
    patient_id: &str,
    study_uid: &str,
    series_uid: &str,
    color: bool,
) -> Vec<u8> {
    use clarus_adapters::jpeg_encoder::{ColorType, Encoder};

    let mut jpeg = Vec::new();
    let encoder = Encoder::new(&mut jpeg, 95);
    if color {
        let mut rgb = [0u8; 64 * 64 * 3]; // interleaved R,G,B
        for px in rgb.chunks_mut(3) {
            px.copy_from_slice(&[0x5a, 0x3c, 0x1e]);
        }
        encoder
            .encode(&rgb, 64, 64, ColorType::Rgb)
            .expect("jpeg rgb encode");
    } else {
        encoder
            .encode(&[0x5a_u8; 64 * 64], 64, 64, ColorType::Luma)
            .expect("jpeg luma encode");
    }

    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.50");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WADO^SMOKE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", patient_id.as_bytes());
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    if color {
        append_element(&mut d, 0x0028, 0x0002, b"US", &3u16.to_le_bytes());
        append_element(&mut d, 0x0028, 0x0004, b"CS", b"RGB");
    } else {
        append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
        append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    }
    append_element(&mut d, 0x0028, 0x0010, b"US", &64u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &64u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    // Encapsulated PixelData element (tag + OB + undefined length + BOT +
    // fragment + sequence delimiter), built by the adapters helper; appended
    // raw, NOT via append_element (it writes a defined-length header).
    let encapsulated = clarus_adapters::codec::build_encapsulated_pixeldata(&jpeg, false);
    d.extend_from_slice(&encapsulated);
    d
}

/// Read width/height from a JPEG SOF0 marker (0xFFC0): [len u16][precision 1]
/// [height u16][width u16] - big-endian.
#[cfg(feature = "transcode")]
fn jpeg_dimensions(body: &[u8]) -> Option<(u16, u16)> {
    let mut i = 0;
    while i + 9 < body.len() {
        if body[i] == 0xFF && body[i + 1] == 0xC0 {
            let h = u16::from_be_bytes([body[i + 5], body[i + 6]]);
            let w = u16::from_be_bytes([body[i + 7], body[i + 8]]);
            return Some((w, h));
        }
        i += 1;
    }
    None
}

/// build a MULTIFRAME ENCAPSULATED RLE instance (synthetic analogue of
/// the field US-003: RLE 1.2.840.10008.1.2.5, MONOCHROME2, NumberOfFrames>1).
/// The pixels are REAL RLE - produced by the in-tree RLE codec
/// (`encode_pixeldata`) so the STOWed bytes decode back to `frames` x rows x
/// cols in the WADO render path exactly as the field instance does. Without
/// this the whole-instance render/thumbnail decoded ALL frames but validated
/// for ONE (W4 500: "Decoded pixels invalid").
fn build_multiframe_rle_instance(
    sop_uid: &str,
    study_uid: &str,
    series_uid: &str,
    rows: u32,
    cols: u32,
    frames: u32,
) -> Vec<u8> {
    use clarus_adapters::codec::rle::RleCodec;
    use clarus_adapters::codec::{DecodedPixels, DicomCodec, DicomMeta};

    let pixel_count = (rows * cols) as usize;
    // A per-pixel ramp (varies per frame) so the auto-window percentile pass
    // in the full renderer sees a well-defined histogram, never a flat image.
    let mut data = Vec::with_capacity(pixel_count * frames as usize);
    for f in 0..frames {
        for p in 0..pixel_count {
            data.push(((p * 7 + f as usize * 11) % 251) as u8);
        }
    }
    let decoded = DecodedPixels {
        data,
        rows: rows as u16,
        columns: cols as u16,
        samples_per_pixel: 1,
        bits_allocated: 8,
        photometric_interpretation: "MONOCHROME2".into(),
    };
    let meta = DicomMeta {
        rows: rows as u16,
        columns: cols as u16,
        samples_per_pixel: 1,
        bits_allocated: 8,
        bits_stored: 8,
        photometric_interpretation: "MONOCHROME2".into(),
        planar_configuration: None,
        pixel_representation: 0,
        frames,
    };
    // Full encapsulated (7FE0,0010) element: one RLE Item per frame.
    let pixeldata = RleCodec
        .encode_pixeldata(&decoded, &meta)
        .expect("rle multiframe encode");

    let mut d = Vec::with_capacity(1024 + pixeldata.len());
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.5");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"MULTI^RLE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"W4PAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"US");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    // NumberOfFrames (0028,0008) is a DICOM IS (decimal string) - the RLE
    // decoder walks this many per-frame headers, so it must be text "N".
    let frames_txt = frames.to_string();
    append_element(&mut d, 0x0028, 0x0008, b"IS", frames_txt.as_bytes());
    append_element(&mut d, 0x0028, 0x0010, b"US", &(rows as u16).to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &(cols as u16).to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    d.extend_from_slice(&pixeldata);
    d
}

/// A SINGLE-FRAME encapsulated RLE instance, the case the early guard used to
/// refuse with 406 before the registered decoder was tried. Same in-tree
/// encoder as the multi-frame builder, one frame.
#[cfg(feature = "transcode")]
fn build_single_frame_rle_instance(
    sop_uid: &str,
    study_uid: &str,
    series_uid: &str,
    rows: u32,
    cols: u32,
) -> Vec<u8> {
    build_multiframe_rle_instance(sop_uid, study_uid, series_uid, rows, cols, 1)
}

// -- Scenario tests ------------------------------------------------------

/// Scenario 1: instance, no Range / no TS -> 200 application/dicom, full bytes.
#[test]
fn wado_instance_no_range_golden() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let r = srv.get(&instance_path(), &[]);
    r.assert_status(200);
    r.assert_content_type("application/dicom");
    check_or_generate("wado_instance_no_range.dcm", &r.body);
}

/// Read one full HTTP response off a keep-alive connection (head +
/// Content-Length body), leaving the stream in sync for the next request.
fn read_ka_response(c: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let sep;
    loop {
        let n = c.read(&mut chunk).expect("read head");
        assert!(n > 0, "connection closed mid-response");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            sep = p + 4;
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..sep]).to_string();
    let cl = head
        .to_ascii_lowercase()
        .lines()
        .find(|l| l.starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .expect("Content-Length present");
    let mut body = buf[sep..].to_vec();
    while body.len() < cl {
        let n = c.read(&mut chunk).expect("read body");
        assert!(n > 0, "connection closed mid-body");
        body.extend_from_slice(&chunk[..n]);
    }
    (head, body)
}

/// Keep-alive gate: ONE connection serves n>=10 single-instance WADO
/// GETs through the TransmitFile path (sendfile_response writes the head +
/// the file, then the loop re-parses the next request - the only writer that
/// bypasses add_response_bytes). Idle deadline between requests is the
/// keep-alive timeout (5 s here), requests are back-to-back.
#[test]
fn keepalive_instance_loop_smoke() {
    const N: usize = 12;
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);

    let mut c = TcpStream::connect(srv.addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");

    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        instance_path(),
        srv.addr.port()
    );
    let mut first = None;
    for _ in 0..N {
        c.write_all(req.as_bytes()).expect("write req");
        let (head, body) = read_ka_response(&mut c);
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "loop iteration must be 200, got: {head}"
        );
        match &first {
            None => first = Some(body),
            Some(f) => assert_eq!(f, &body, "all loop bodies must be identical"),
        }
    }
    // The connection is still alive after N requests - explicitly close it
    // from the client side and assert the server sees a clean EOF (the
    // connection was never closed by the server mid-loop).
    c.shutdown(std::net::Shutdown::Write).expect("shutdown");
}

/// Multipart STOW must write the study manifest (so the instance is
/// visible to B-tree QIDO / StudyScanner) and be retrievable via WADO.
#[test]
fn wado_multipart_stow_manifest_roundtrip() {
    let srv = TestServer::start();
    seed_stow_multipart(&srv, &[&sop_a()]);

    // The per-study CBOR manifest must exist and expose the instance.
    let mpath = srv.manifests_dir.join(format!("{STUDY_A}.manifest"));
    assert!(
        mpath.exists(),
        "study manifest must exist after multipart STOW"
    );
    let m = clarus_core::manifest::read_manifest(&mpath).expect("decode manifest");
    assert!(
        m.find_instance(SOP_A).is_some(),
        "manifest must expose SOP_A"
    );

    // The instance must be retrievable through WADO (manifest + BLAKE3 CAS).
    let r = srv.get(&instance_path(), &[]);
    r.assert_status(200);
    r.assert_content_type("application/dicom");
    assert_eq!(
        r.body,
        sop_a(),
        "multipart STOW round-trip must be byte-exact"
    );
}

/// Scenario 2: Range without TS -> 200 full file (streaming path ignores Range).
#[test]
fn wado_range_without_ts_serves_full_file() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let r = srv.get(&instance_path(), &[("Range", "bytes=0-9")]);
    r.assert_status(200);
    r.assert_content_type("application/dicom");
    // Streaming path ignores Range -> full original bytes.
    let full = srv.get(&instance_path(), &[]);
    assert_eq!(r.body, full.body, "streaming path must ignore Range");
}

/// Scenario 10a: unknown SOP -> 404.
#[test]
fn wado_unknown_sop_404() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let path = format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/1.2.826.0.1.3680043.10.1234.6299"
    );
    let r = srv.get(&path, &[]);
    r.assert_status(404);
}

/// Scenario 10b: invalid UID -> 400.
#[test]
fn wado_invalid_uid_400() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    // Letters in a UID fail validate_dicom_uid -> 400 Bad Request.
    // Must still match the route (a single path segment) so the UID validator runs.
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/not_a_valid_uid");
    let r = srv.get(&path, &[]);
    r.assert_status(400);
}

/// Scenario 8: metadata -> dicom+json, BulkDataURI present, no inline pixels.
#[test]
fn wado_metadata_bulkdata_uri() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let r = srv.get(&metadata_path(), &[]);
    r.assert_status(200);
    r.assert_content_type(STOW_JSON_CT);
    let v = parse_json(&r.body).expect("metadata json");
    // PixelData always injected as BulkDataURI (DEV-015).
    let pixel = v.obj_get("7FE00010").expect("PixelData tag");
    assert!(
        pixel.obj_get("BulkDataURI").is_some(),
        "PixelData must carry a BulkDataURI"
    );
    assert!(
        pixel.obj_get("Value").is_none(),
        "PixelData must not carry inline Value"
    );
    check_or_generate("wado_metadata.json", &r.body);
}

/// WADO-RS `/metadata` renders nested SQ recursively (PS3.18 8.7.3.2).
/// Before the fix every sequence rendered `{"Value":[],"vr":"SQ"}`, which
/// blanked WSI OpticalPathSequence and broke Slim's channel discovery.
#[test]
fn wado_metadata_nested_sq_renders_recursively() {
    let srv = TestServer::start();
    let dcm = build_instance_nested_sq();
    seed_stow(&srv, &[&dcm]);

    let sop = NESTED_SQ_SOP;
    let study = NESTED_SQ_STUDY;
    let series = NESTED_SQ_SERIES;
    let path = format!("/dicomweb/studies/{study}/series/{series}/instances/{sop}/metadata");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    r.assert_content_type(STOW_JSON_CT);
    let body = String::from_utf8_lossy(&r.body).into_owned();

    // Top-level SQ is an array with one item object, not empty.
    assert!(
        body.contains("\"00081198\":{\"Value\":[{"),
        "top-level SQ must render one item object: {body}"
    );
    // The nested CodeSequence inside the item renders too.
    assert!(
        body.contains("\"00080100\":{\"Value\":[{"),
        "nested CodeSequence must render recursively: {body}"
    );
    // Leaf values inside the nested items are present.
    assert!(
        body.contains("\"REF^CODE\"") && body.contains("\"NESTED^LEAF\""),
        "leaf values inside nested items must render: {body}"
    );
    // No sequence is left as the pre-fix empty shape.
    assert!(
        !body.contains("\"Value\":[],\"vr\":\"SQ\""),
        "no SQ may render as an empty array: {body}"
    );
}

/// Scenario 8b: series metadata, multipart accept -> multipart/related with
/// dicom+json parts (the Required media type, Weasis contract).
#[test]
fn wado_series_metadata_multipart() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/metadata");
    let r = srv.get(
        &path,
        &[(
            "Accept",
            "multipart/related; type=\"application/dicom+json\"",
        )],
    );
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type header");
    assert!(
        ct.starts_with("multipart/related;"),
        "series metadata with multipart accept must answer multipart, got {ct}"
    );
    let boundary = ct
        .split(';')
        .find_map(|p| {
            p.trim()
                .strip_prefix("boundary=")
                .map(|b| b.trim_matches('"').to_string())
        })
        .expect("boundary param");
    // The test client keeps chunk framing (does not decode
    // Transfer-Encoding), so search for the MIME part instead of asserting a
    // byte-exact body prefix.
    let opening = format!("--{boundary}\r\nContent-Type: application/dicom+json");
    assert!(
        find_subslice(&r.body, opening.as_bytes()).is_some(),
        "multipart body must contain the first part header"
    );
    let closing = format!("--{boundary}--");
    assert!(
        find_subslice(&r.body, closing.as_bytes()).is_some(),
        "multipart body must contain the closing delimiter"
    );
    // The single seeded instance metadata JSON is inside the first part.
    assert!(
        find_subslice(&r.body, b"\"0020000D\"").is_some(),
        "instance metadata keys must be present in the multipart body"
    );
}

/// Scenario 8c: series metadata, OHIF-style single JSON accept
/// (`application/dicom+json`) -> single-part JSON array (02.09 field
/// incident: OHIF sets responseType=json and crashes on multipart).
/// Absent Accept also gets the single array (SQL-era behavior that OHIF
/// and Weasis were field-verified against).
#[test]
fn wado_series_metadata_single_json() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/metadata");
    // Both OHIF's explicit dicom+json accept and a missing Accept header
    // must yield the single-part array.
    for accept in [Some("application/dicom+json"), None] {
        let r = match accept {
            Some(a) => srv.get(&path, &[("Accept", a)]),
            None => srv.get(&path, &[]),
        };
        r.assert_status(200);
        let ct = r.header("content-type").expect("content-type header");
        assert!(
            ct.starts_with("application/dicom+json"),
            "accept={accept:?}: must answer application/dicom+json, got {ct}"
        );
        assert!(
            !ct.contains("multipart"),
            "accept={accept:?}: single-JSON response must not be multipart"
        );
        let arr = parse_json(&r.body).expect("series metadata JSON array");
        let items = arr.as_array().expect("array");
        assert_eq!(items.len(), 1);
        assert!(
            items[0].obj_get("0020000D").is_some(),
            "StudyInstanceUID present"
        );
    }
}

/// Study Metadata (PS3.18 10.4.1.1.2 Table 10.4.1-2): `GET
/// /dicomweb/studies/{study}/metadata` answers 200 with the metadata of EVERY
/// instance in the study, in study order, across all its series. Before the
/// fix the route had no four-segment branch and answered 404 for an existing
/// study.
#[test]
fn wado_study_metadata_single_json() {
    let srv = TestServer::start();
    // A study with TWO series, two instances each, so the study form is not
    // just the series form in disguise.
    let s2 = "1.2.826.0.1.3680043.10.1234.6111";
    seed_stow(
        &srv,
        &[
            &build_instance(SOP_A, "WADOPAT01", STUDY_A, SERIES_A),
            &build_instance(
                "1.2.826.0.1.3680043.10.1234.6206",
                "WADOPAT01",
                STUDY_A,
                SERIES_A,
            ),
            &build_instance("1.2.826.0.1.3680043.10.1234.6207", "WADOPAT01", STUDY_A, s2),
            &build_instance("1.2.826.0.1.3680043.10.1234.6208", "WADOPAT01", STUDY_A, s2),
        ],
    );

    let path = format!("/dicomweb/studies/{STUDY_A}/metadata");
    // Both OHIF's explicit dicom+json accept and a missing Accept header
    // must yield the single-part array (same contract as the series form).
    for accept in [Some("application/dicom+json"), None] {
        let r = match accept {
            Some(a) => srv.get(&path, &[("Accept", a)]),
            None => srv.get(&path, &[]),
        };
        r.assert_status(200);
        let ct = r.header("content-type").expect("content-type header");
        assert!(
            ct.starts_with("application/dicom+json"),
            "accept={accept:?}: must answer application/dicom+json, got {ct}"
        );
        assert!(
            !ct.contains("multipart"),
            "accept={accept:?}: single-JSON response must not be multipart"
        );
        let arr = parse_json(&r.body).expect("study metadata JSON array");
        let items = arr.as_array().expect("array");
        assert_eq!(
            items.len(),
            4,
            "study metadata covers all four instances across both series"
        );
        for item in items {
            // A metadata attribute is the usual DICOM JSON object
            // ({Tag:{Value:[...],vr:"UI"}}), so read the first Value entry.
            assert_eq!(
                item.obj_get("0020000D")
                    .and_then(|v| v.obj_get("Value"))
                    .and_then(|v| v.as_str_array_first()),
                Some(STUDY_A),
                "every item states the requested StudyInstanceUID"
            );
        }
    }
}

/// A MULTIPART Accept on the study form yields `multipart/related;
/// type="application/dicom+json"` with one part per instance.
#[test]
fn wado_study_metadata_multipart() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let path = format!("/dicomweb/studies/{STUDY_A}/metadata");
    let r = srv.get(
        &path,
        &[(
            "Accept",
            "multipart/related; type=\"application/dicom+json\"",
        )],
    );
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type header");
    assert!(
        ct.starts_with("multipart/related"),
        "study metadata with multipart accept must answer multipart, got {ct}"
    );
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("\"0020000D\""),
        "instance metadata keys must be present in the multipart body"
    );
}

/// An unknown study -> 404, not 200 with an empty array.
#[test]
fn wado_study_metadata_unknown_study_404() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let r = srv.get("/dicomweb/studies/2.25.999/metadata", &[]);
    r.assert_status(404);
}

/// Scenario 9: bulkdata -> raw bytes of the tag.
#[test]
fn wado_bulkdata_raw_bytes() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    // 7FE00010 = PixelData (64 bytes).
    let r = srv.get(&bulkdata_path("7FE00010"), &[]);
    r.assert_status(200);
    r.assert_content_type("application/octet-stream");
    assert_eq!(r.body.len(), 64, "pixel data must be exactly 64 bytes");
    assert_eq!(r.body, vec![0x5a_u8; 64]);
}

/// Extract the dynamic `boundary="..."` value from a multipart Content-Type.
fn boundary_param(ct: &str) -> String {
    ct.split(';')
        .find_map(|p| {
            p.trim()
                .strip_prefix("boundary=")
                .map(|b| b.trim_matches('"').to_string())
        })
        .expect("boundary param")
}

/// assert the single /frames part header names the stored syntax -
/// `expected` is the exact Content-Type value after the `Content-Type: `
/// prefix, i.e. `<media>; transfer-syntax=<ts>`.
fn assert_frame_part_label(body: &[u8], boundary: &str, expected: &str) {
    let header = format!("--{boundary}\r\nContent-Type: {expected}\r\n\r\n");
    assert!(
        find_subslice(body, header.as_bytes()).is_some(),
        "frame part header must be `Content-Type: {expected}`; body opened: {}",
        String::from_utf8_lossy(&body[..body.len().min(180)])
    );
}

/// Scenario 6a: frame 1 -> multipart/related; type="application/octet-stream"
/// (PS3.18 10.4.4 - one part per frame; OHIF decodes frames as multipart).
#[test]
fn wado_frame_one_multipart_related() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let r = srv.get(&frames_path("1"), &[]);
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type header");
    assert!(
        ct.starts_with("multipart/related;"),
        "frames must answer multipart/related, got {ct}"
    );
    assert!(
        ct.contains("type=\"application/octet-stream\""),
        "frames part type must be application/octet-stream, got {ct}"
    );
    let boundary = boundary_param(ct);
    assert!(
        r.body.starts_with(format!("--{boundary}\r\n").as_bytes()),
        "frames body must open with the multipart boundary"
    );
    assert!(
        r.body
            .ends_with(format!("\r\n--{boundary}--\r\n").as_bytes()),
        "frames body must close with the final boundary"
    );
    // native: sop_a() stores Explicit VR LE -> octet-stream + the stored TS.
    assert_frame_part_label(
        &r.body,
        &boundary,
        "application/octet-stream; transfer-syntax=1.2.840.10008.1.2.1",
    );
    // The single part carries the 64 bytes of constant pixel data (0x5A).
    assert!(
        find_subslice(&r.body, &[0x5a_u8; 64]).is_some(),
        "frame pixel bytes must be present inside the part"
    );
}

/// Scenario 6b (W1, JPEG-Baseline 4.50): the /frames part of an encapsulated
/// JPEG Baseline single-frame instance is labelled `image/jpeg` WITH the
/// stored 4.50 syntax - without it the OHIF loader collapses to Implicit LE
/// and decodes compressed bytes as garbage.
#[test]
fn wado_frame_part_label_jpeg_baseline() {
    let srv = TestServer::start();
    seed_stow(
        &srv,
        &[&build_encapsulated_frame(
            SOP_E,
            b"1.2.840.10008.1.2.4.50",
            &fake_jpeg_bitstream(),
        )],
    );
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_E}/frames/1");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    let boundary = boundary_param(r.header("content-type").expect("content-type header"));
    assert_frame_part_label(
        &r.body,
        &boundary,
        "image/jpeg; transfer-syntax=1.2.840.10008.1.2.4.50",
    );
}

/// Scenario 6c (W1, RLE): the /frames part of an encapsulated RLE
/// (1.2.840.10008.1.2.5) instance is labelled `image/x-dicom-rle`.
#[test]
fn wado_frame_part_label_rle() {
    let srv = TestServer::start();
    seed_stow(
        &srv,
        &[&build_encapsulated_frame(
            SOP_C,
            b"1.2.840.10008.1.2.5",
            b"\x01\x00\x5a\x5a",
        )],
    );
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_C}/frames/1");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    let boundary = boundary_param(r.header("content-type").expect("content-type header"));
    assert_frame_part_label(
        &r.body,
        &boundary,
        "image/x-dicom-rle; transfer-syntax=1.2.840.10008.1.2.5",
    );
}

/// Scenario 6d (W1, JPEG-LS 4.70): the /frames part of an encapsulated JPEG-LS
/// instance is labelled `image/jpeg` WITH the stored 4.70 syntax.
#[test]
fn wado_frame_part_label_jpeg_ls_470() {
    let srv = TestServer::start();
    seed_stow(
        &srv,
        &[&build_encapsulated_frame(
            SOP_D,
            b"1.2.840.10008.1.2.4.70",
            &fake_jpeg_bitstream(),
        )],
    );
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_D}/frames/1");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    let boundary = boundary_param(r.header("content-type").expect("content-type header"));
    assert_frame_part_label(
        &r.body,
        &boundary,
        "image/jpeg; transfer-syntax=1.2.840.10008.1.2.4.70",
    );
}

/// Scenario 6a-enc: frame 1 of a single-frame ENCAPSULATED instance must be
/// the pure bitstream of the frame Item (02.09 OHIF gray-squares incident:
/// serving the whole element with BOT + Item wrappers breaks the JS decoder).
#[test]
fn wado_frame_encapsulated_single_frame_unwraps_items() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&build_encapsulated_single_frame(SOP_B)]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_B}/frames/1");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    // 6a-enc is a JPEG Baseline 4.50 instance - the part must be labelled.
    let boundary = boundary_param(r.header("content-type").expect("content-type header"));
    assert_frame_part_label(
        &r.body,
        &boundary,
        "image/jpeg; transfer-syntax=1.2.840.10008.1.2.4.50",
    );
    let jpeg = fake_jpeg_bitstream();
    // The part body is exactly the JPEG item bytes.
    assert!(
        find_subslice(&r.body, &jpeg).is_some(),
        "frame response must carry the raw bitstream"
    );
    // The regression assert: no Item tag bytes (FFFE,E000) may leak into
    // the response - neither the BOT header nor the frame Item header.
    assert!(
        find_subslice(&r.body, &[0xFE, 0xFF, 0x00, 0xE0]).is_none(),
        "encapsulated Item wrappers must not leak into the frame bytes"
    );
    // The bitstream is the part body between the MIME part header and the
    // closing delimiter (outer Content-Length covers the whole multipart).
    let jpeg_end = r
        .body
        .windows(jpeg.len())
        .position(|w| w == jpeg.as_slice())
        .expect("bitstream position")
        + jpeg.len();
    assert_eq!(
        &r.body[jpeg_end..jpeg_end + 2],
        b"\r\n",
        "bitstream followed by CRLF"
    );
}

/// Scenario 6a-rows430: Rows=430 -> element bytes [0xAE,0x01]; 0xAE is not
/// valid UTF-8, and parse_dicom_int previously aborted before its binary
/// fallback - every /frames request on such geometry answered 500
/// (02.09 RLE US incident, study 999.999.3859744).
#[test]
fn wado_frame_rows_430_high_byte() {
    let srv = TestServer::start();
    seed_stow(
        &srv,
        &[&build_instance_geometry(
            SOP_B, "WADOROWS", STUDY_A, SERIES_A, 430, 600,
        )],
    );
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_B}/frames/1");
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type header");
    assert!(ct.starts_with("multipart/related;"));
    // 430x600 bytes of constant pixel data inside the single part.
    assert!(
        find_subslice(&r.body, &vec![0x5a_u8; 430 * 600]).is_some(),
        "frame pixel bytes must be present inside the part"
    );
}

/// Scenario 6b: a frame number outside [1, NumberOfFrames] is an absent
/// Target Resource -> 404 (PS3.18 10.4.3.1 Table 10.4.3-1). The frame URI
/// segment is NOT the byte-Range HEADER mechanism, so 416 is wrong here.
/// `frames/0` (frames are 1-indexed) and `frames/99999` on a 1-frame
/// instance are both 404 with the JSON error body.
#[test]
fn wado_frame_out_of_range_404() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);

    for frame in ["0", "99999"] {
        let r = srv.get(&frames_path(frame), &[]);
        r.assert_status(404);
        r.assert_content_type("application/json; charset=utf-8");
        let body = String::from_utf8_lossy(&r.body);
        assert!(
            body.contains("\"status\":404"),
            "frames/{frame} must answer the 404 JSON body, got {body}"
        );
    }
}

/// Scenario 11: `Accept: multipart/related` -> structural check (dynamic boundary).
#[test]
fn wado_accept_multipart_structural() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let r = srv.get(
        &instance_path(),
        &[("Accept", "multipart/related; type=\"application/dicom\"")],
    );
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type header");
    assert!(
        ct.starts_with("multipart/related;"),
        "expected multipart/related, got {ct}"
    );
    // Extract the dynamic boundary from Content-Type.
    let boundary = ct
        .split(';')
        .find_map(|p| {
            let t = p.trim();
            t.strip_prefix("boundary=")
                .map(|b| b.trim_matches('"').to_string())
        })
        .expect("boundary param");
    assert!(!boundary.is_empty(), "boundary must not be empty");
    // Body must be: CRLF --boundary CRLF header CRLF CRLF <dicom> CRLF --boundary-- CRLF
    // The part header names the part bytes' ACTUAL syntax (Step 0 of the
    // 3008 stored-first passthrough); sop_a() stores Explicit VR LE.
    // Search byte-wise (the DICOM payload is binary, not UTF-8).
    let opening = format!(
        "\r\n--{boundary}\r\nContent-Type: application/dicom; transfer-syntax=1.2.840.10008.1.2.1\r\n\r\n"
    );
    let closing = format!("\r\n--{boundary}--\r\n");
    let ob = opening.as_bytes();
    let cb = closing.as_bytes();
    let start = find_subslice(&r.body, ob).expect("multipart opening header");
    let end = rfind_subslice(&r.body, cb).expect("multipart closing marker");
    assert!(start < end, "opening must precede closing");
    // The payload between header and footer must be the full DICOM instance.
    let payload = &r.body[start + ob.len()..end];
    assert_eq!(
        payload,
        &sop_a(),
        "multipart payload must be the full instance"
    );
}

/// Index of the first occurrence of `needle` in `haystack`, or `None`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Index of the last occurrence of `needle` in `haystack`, or `None`.
fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

// -- Transcode-gated scenarios (device requires `--features transcode`) --

/// Scenario 3: Range + `?transferSyntax=` (stored TS is uncompressed;
/// requesting it matches the stored TS -> no transcode) - but to hit the
/// 206 path we request the EXPLICITLY supported uncompressed TS via the
/// Accept header form and provide a Range. Because the stored TS equals the
/// requested TS (transferSyntax 1.2.840.10008.1.2.1), no transcode runs and
/// the STREAMING path is bypassed only when the query param is present.
#[cfg(feature = "transcode")]
#[test]
fn wado_range_with_ts_206() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    // Query param forces the buffered transcode path; asking for the stored TS
    // produces a byte-for-byte copy, then Range is applied -> 206.
    let path = format!("{}?transferSyntax=1.2.840.10008.1.2.1", instance_path());
    let r = srv.get(&path, &[("Range", "bytes=0-9")]);
    r.assert_status(206);
    let cr = r.header("content-range").expect("Content-Range header");
    assert!(
        cr.starts_with("bytes 0-9/"),
        "expected bytes 0-9/total, got {cr}"
    );
    // RFC 9110 14.1.2 makes the Content-Range end inclusive, so `bytes=0-9`
    // is TEN bytes: header and body must describe the same span. The earlier
    // wire sliced `[0..9]` (nine bytes) under a header that promised ten.
    let full = srv.get(&instance_path(), &[]);
    assert_eq!(full.status, 200);
    assert_eq!(r.body.len(), 10, "bytes=0-9 is ten bytes inclusive");
    assert_eq!(
        r.body,
        full.body[0..10],
        "partial body must be the [0..=9] span the header names"
    );
    // Leading bytes must match the original DICOM preamble (128 zero + DICM).
    assert_eq!(&r.body[..4], b"\x00\x00\x00\x00");
}

/// Scenario 4: `?transferSyntax=` = stored TS -> 200 original (no transcode).
#[cfg(feature = "transcode")]
#[test]
fn wado_transfer_syntax_stored_200_original() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let stored = "1.2.840.10008.1.2.1"; // Explicit VR Little Endian -- the stored TS.
    let path = format!("{}?transferSyntax={stored}", instance_path());
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    // The single-part response now declares the served transfer syntax
    // (PS3.18 8.7.3); requesting the STORED syntax is still a no-op.
    r.assert_content_type("application/dicom; transfer-syntax=1.2.840.10008.1.2.1");
    // Requesting the stored TS is a no-op -> exact original bytes.
    let full = srv.get(&instance_path(), &[]);
    assert_eq!(
        r.body, full.body,
        "stored-TS request must serve original bytes"
    );
}

/// Scenario 5: `?transferSyntax=` unsupported -> 406 with "Available:".
#[cfg(feature = "transcode")]
#[test]
fn wado_transfer_syntax_unsupported_406() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    // A private/unknown transfer syntax that no registered codec claims ->
    // `registry.supports` is false -> 406 with an "Available:" list.
    let unsupported = "1.2.826.0.1.3680043.10.9.9.1";
    let path = format!("{}?transferSyntax={unsupported}", instance_path());
    let r = srv.get(&path, &[]);
    r.assert_status(406);
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("Available:"),
        "406 body must list available syntaxes, got: {body}"
    );
}

/// 2608 option (b): a saturated transcode pool inside a chunked multipart
/// stream MUST NOT 503 mid-body. The busy parts are skipped (same graceful
/// skip as a transcode failure) and the stream stays a valid 200 multipart.
#[cfg(feature = "transcode")]
#[test]
fn wado_multipart_busy_pool_skips_parts_keeps_stream() {
    let srv = TestServer::start_with(|c| c.transcode.max_concurrent = 1);
    seed_stow(&srv, &[&sop_a()]);
    // Occupy the single permit so every TranscodeTo part hits the busy path.
    assert!(srv.pool.try_acquire(), "take the one permit");
    // JPEG Baseline is a supported compressed target != stored EVRLE -> the
    // part resolves to TranscodeTo and tries to acquire the (busy) permit.
    let path = format!(
        "/dicomweb/studies/{STUDY_A}?transferSyntax={}",
        clarus_adapters::codec::ts_uid::JPEG_BASELINE
    );
    let r = srv.get(
        &path,
        &[("Accept", "multipart/related; type=\"application/dicom\"")],
    );
    r.assert_status(200);
    let ct = r.header("content-type").expect("content-type header");
    assert!(
        ct.starts_with("multipart/related;"),
        "expected multipart/related, got {ct}"
    );
    // The stream must still carry the closing boundary (not truncated / mid-
    // body), proving a busy part was skipped cleanly rather than aborting.
    let boundary = ct
        .split(';')
        .find_map(|p| {
            let t = p.trim();
            t.strip_prefix("boundary=")
                .map(|b| b.trim_matches('"').to_string())
        })
        .expect("boundary param");
    let closing = format!("\r\n--{boundary}--\r\n");
    assert!(
        find_subslice(&r.body, closing.as_bytes()).is_some(),
        "multipart must close cleanly even when a part was busy-skipped"
    );
    srv.pool.release();
}

/// Scenario 7: thumbnail -> image/jpeg + ETag + Cache-Control, JPEG byte-exact.
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_jpeg_etag() {
    let srv = TestServer::start();
    let inst = sop_a();
    seed_stow(&srv, &[&inst]);
    let r = srv.get(&thumbnail_path(), &[]);
    r.assert_status(200);
    r.assert_content_type("image/jpeg");
    // ETag is the quoted CAS key (BLAKE3 hex) - wire-preserving pre-extraction value.
    let expected_etag = format!(
        "\"{}\"",
        clarus_adapters::hex_encode(&clarus_server::shadow::blake3_of(&inst))
    );
    assert_eq!(r.header("etag"), Some(expected_etag.as_str()));
    assert_eq!(
        r.header("cache-control"),
        Some("public, max-age=3600"),
        "thumbnail must carry Cache-Control"
    );
    assert!(
        r.body.starts_with(&[0xFF, 0xD8]),
        "body must be a JPEG (SOI marker)"
    );
    check_or_generate("wado_thumbnail.jpg", &r.body);
}

/// Scenario 7b: thumbnail is deterministic - two requests -> byte-identical JPEG.
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_deterministic() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    let r1 = srv.get(&thumbnail_path(), &[]);
    let r2 = srv.get(&thumbnail_path(), &[]);
    r1.assert_status(200);
    r2.assert_status(200);
    assert_eq!(r1.body, r2.body, "thumbnail encoder must be deterministic");
}

/// Scenario 7d (PS3.18 8.3.5.1.3 / 8.3.5.2): the `viewport` query parameter
/// sets the encoded size. An absent parameter keeps the documented default,
/// and the two sizes the field actually asks for - OHIF's 64 and Weasis's 256
/// - both come back at the size requested.
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_viewport_sets_the_encoded_size() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);

    // No viewport -> the documented default (64), so OHIF is unchanged.
    let default = srv.get(&thumbnail_path(), &[]);
    default.assert_status(200);
    assert_eq!(
        jpeg_dimensions(&default.body),
        Some((64, 64)),
        "an absent viewport must keep the documented default"
    );

    // OHIF's explicit request and Weasis's larger request.
    for size in [64u16, 256] {
        let path = format!("{}/thumbnail?viewport={size},{size}", instance_path());
        let r = srv.get(&path, &[]);
        r.assert_status(200);
        assert_eq!(r.header("content-type"), Some("image/jpeg"));
        assert_eq!(
            jpeg_dimensions(&r.body),
            Some((size, size)),
            "viewport={size} must render at {size}x{size}"
        );
    }
}

/// Scenario 7e (PS3.18 8.3.5.1.3): an ill-formed viewport on a Thumbnail is a
/// 400 with a Status Report body, never a silent default. Covers non-numeric,
/// zero, a single component, the four-value crop form (which a Thumbnail does
/// not accept) and a negative dimension.
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_ill_formed_viewport_is_400() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    for value in ["abc", "64", "0,0", "64,0", "-64,64", "512,512,0,0"] {
        let path = format!("{}/thumbnail?viewport={value}", instance_path());
        let r = srv.get(&path, &[]);
        assert_eq!(
            r.status, 400,
            "viewport={value:?} must be a 400, got {}",
            r.status
        );
    }
}

/// Scenario 7f (PS3.18 8.3.5.1.3): a well-formed but unsupported viewport is
/// also a 400, and the supported maximum is documented (10.4.5 requires the
/// min/max in the Conformance Statement).
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_oversized_viewport_is_400() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);
    for value in ["513,513", "100000,100000", "513,64"] {
        let path = format!("{}/thumbnail?viewport={value}", instance_path());
        let r = srv.get(&path, &[]);
        assert_eq!(
            r.status, 400,
            "viewport={value:?} exceeds the supported maximum and must be a 400, got {}",
            r.status
        );
    }
    // Exactly the documented maximum is still served.
    let path = format!("{}/thumbnail?viewport=512,512", instance_path());
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    assert_eq!(jpeg_dimensions(&r.body), Some((512, 512)));
}

/// Scenario 7g (PS3.18 10.4.4 Table 10.4.4-1): `image/jpeg` is the only
/// Thumbnail media type, so an Accept naming something else is a 406 and a
/// bare wildcard still gets the JPEG (no preference -> the default).
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_accept_negotiation() {
    let srv = TestServer::start();
    seed_stow(&srv, &[&sop_a()]);

    // Acceptable: the media type itself, with parameters, and a wildcard.
    for accept in ["image/jpeg", "image/jpeg;q=0.9", "*/*", "image/*"] {
        let r = srv.get(&thumbnail_path(), &[("Accept", accept)]);
        assert_eq!(r.status, 200, "Accept: {accept} must be served");
        assert_eq!(r.header("content-type"), Some("image/jpeg"));
    }

    // Unacceptable: a type this resource cannot produce (10.4.4 lists only
    // image/jpeg for Thumbnail Resources).
    for accept in ["application/dicom+json", "text/plain"] {
        let r = srv.get(&thumbnail_path(), &[("Accept", accept)]);
        assert_eq!(
            r.status, 406,
            "Accept: {accept} names no acceptable Thumbnail media type"
        );
    }
}

/// Scenario 7c: COMPRESSED (JPEG Baseline) instance thumbnail in a transcode
/// build -> 200 image/jpeg 64x64, deterministic, series/study 200, ETag =
/// quoted CAS key (the raw-path 406 no longer applies to the _any
/// wrappers).
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_compressed_transcode_build() {
    let srv = TestServer::start();
    let inst = build_compressed_instance(SOP_A, "WADOPAT01", STUDY_A, SERIES_A, false);
    seed_stow(&srv, &[&inst]);

    let r = srv.get(&thumbnail_path(), &[]);
    r.assert_status(200);
    r.assert_content_type("image/jpeg");
    assert!(r.body.starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
    assert_eq!(
        jpeg_dimensions(&r.body),
        Some((64, 64)),
        "compressed thumbnail must be 64x64"
    );
    // ETag is the quoted CAS key, the same wire contract as the raw path.
    let expected_etag = format!(
        "\"{}\"",
        clarus_adapters::hex_encode(&clarus_server::shadow::blake3_of(&inst))
    );
    assert_eq!(r.header("etag"), Some(expected_etag.as_str()));

    // Determinism: two requests -> byte-identical JPEG bodies.
    let r2 = srv.get(&thumbnail_path(), &[]);
    r2.assert_status(200);
    assert_eq!(
        r.body, r2.body,
        "compressed thumbnail must be deterministic"
    );

    // Series- and study-level thumbnails of the same study -> 200 JPEG.
    let series_path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/thumbnail");
    let sr = srv.get(&series_path, &[]);
    sr.assert_status(200);
    sr.assert_content_type("image/jpeg");
    let study_path = format!("/dicomweb/studies/{STUDY_A}/thumbnail");
    let tr = srv.get(&study_path, &[]);
    tr.assert_status(200);
    tr.assert_content_type("image/jpeg");
}

/// Scenario 7d: frame thumbnail of a compressed instance (frame 1) -> 200
/// JPEG; frame 2 on a single-frame instance -> 404 (nonexistent frame target,
/// PS3.18 10.4.3.1 - not a byte-range 416).
#[cfg(feature = "transcode")]
#[test]
fn wado_frame_thumbnail_compressed() {
    let srv = TestServer::start();
    let inst = build_compressed_instance(SOP_A, "WADOPAT01", STUDY_A, SERIES_A, false);
    seed_stow(&srv, &[&inst]);
    let path = format!("{}/frames/1/thumbnail", instance_path());
    let r = srv.get(&path, &[]);
    r.assert_status(200);
    r.assert_content_type("image/jpeg");
    let path2 = format!("{}/frames/2/thumbnail", instance_path());
    let r2 = srv.get(&path2, &[]);
    r2.assert_status(404);
}

/// Scenario 8 (W4): a MULTIFRAME encapsulated RLE instance full-renders via
/// `Accept: image/jpeg` -> 200 JPEG. Regression pin for the field OHIF 500:
/// the decode returns ALL frames (frame-major), so the render validated for
/// the real frame count (validate_frames(N)) and rendered frame 1's slice,
/// instead of the old `validate()` (==1 frame) -> "Decoded pixels invalid".
#[cfg(feature = "transcode")]
#[test]
fn wado_multiframe_rle_full_render_200() {
    let srv = TestServer::start();
    let inst = build_multiframe_rle_instance(SOP_W4, STUDY_A, SERIES_A, 8, 8, 3);
    seed_stow(&srv, &[&inst]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_W4}");
    let r = srv.get(&path, &[("Accept", "image/jpeg")]);
    r.assert_status(200);
    r.assert_content_type("image/jpeg");
    assert!(
        r.body.starts_with(&[0xFF, 0xD8]),
        "full render must be a JPEG (SOI)"
    );
}

/// Scenario 8b (W4, re-pinned 14.10): the same multiframe RLE instance
/// `/rendered` -> 406, but now for ONE reason only. The `/rendered` resource
/// still refuses a multi-frame instance from the header (8.7.4-1 gives no
/// multi-frame image/jpeg; `/frames/{N}` is the frame access). It no longer
/// refuses a COMPRESSED instance: that early guard fired before the decoder
/// that CAN satisfy a single-frame compressed Accept (see 8i), so it was a
/// 10.4.3.3.3 non-conformance. The 406 here is therefore the multi-frame
/// contract, and it must be the multi-frame message.
#[test]
fn wado_multiframe_rle_instance_rendered_406() {
    let srv = TestServer::start();
    let inst = build_multiframe_rle_instance(SOP_W4, STUDY_A, SERIES_A, 8, 8, 3);
    seed_stow(&srv, &[&inst]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_W4}/rendered");
    let r = srv.get(&path, &[]);
    r.assert_status(406);
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("multi-frame"),
        "the 406 must name the multi-frame reason, not a missing decoder: {body}"
    );
}

/// Scenario 8c (W4, re-pinned 14.10): series-level `/rendered` is NOT a
/// resource - the study/series levels used to alias the thumbnail handler,
/// which served image/jpeg under a name that 8.7.4-1 gives no multi-frame
/// media type for (a MANDATORY deviation, 10.4.3.3.3). The name is now an
/// honest 404. Series `/thumbnail` is unaffected (scenario 7c).
#[test]
fn wado_multiframe_rle_series_rendered_is_404() {
    let srv = TestServer::start();
    let inst = build_multiframe_rle_instance(SOP_W4, STUDY_A, SERIES_A, 8, 8, 3);
    seed_stow(&srv, &[&inst]);
    for path in [
        format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/rendered"),
        format!("/dicomweb/studies/{STUDY_A}/rendered"),
    ] {
        let r = srv.get(&path, &[]);
        r.assert_status(404);
    }
    // The thumbnail names the same study/series still answer image/jpeg. The
    // thumbnail of a compressed study needs a decoder, so it is 200 only in a
    // transcode build - that contract is 7c's, asserted here just for the
    // narrowing (a 404 on `/rendered` must not have taken `/thumbnail` with it).
    let r = srv.get(
        &format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/thumbnail"),
        &[],
    );
    if cfg!(feature = "transcode") {
        r.assert_status(200);
        r.assert_content_type("image/jpeg");
    } else {
        r.assert_status(406);
    }
}

/// Scenario 8i (14.10): a SINGLE-FRAME compressed instance renders like any
/// other. The old early guard answered 406 on the manifest syntax before the
/// registered decoder was consulted; the codec registry ships RLE in every
/// build, so with `transcode` the same instance must answer 200 image/jpeg.
#[cfg(feature = "transcode")]
#[test]
fn wado_single_frame_rle_instance_rendered_200() {
    let srv = TestServer::start();
    let inst = build_single_frame_rle_instance(SOP_W5, STUDY_A, SERIES_A, 8, 8);
    seed_stow(&srv, &[&inst]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_W5}/rendered");
    let r = srv.get(&path, &[("Accept", "image/jpeg")]);
    r.assert_status(200);
    r.assert_content_type("image/jpeg");
    assert_eq!(
        &r.body[0..2],
        &[0xFF, 0xD8],
        "rendered must be a JPEG (SOI)"
    );
}

/// Scenario 8g (DEFAULT build, re-pinned 14.10): a multi-frame RLE instance
/// on `/rendered` -> 406 in a default build too. The reason is the multi-frame
/// header reject, which is build-independent, so the message must name it and
/// not a missing transcoder: with the early compressed guard gone, a
/// single-frame compressed instance is the only case whose status depends on
/// the decoder (8i), and the default build's honest answer for that is 406.
#[cfg(not(feature = "transcode"))]
#[test]
fn wado_multiframe_rle_render_default_406() {
    let srv = TestServer::start();
    let inst = build_multiframe_rle_instance(SOP_W4, STUDY_A, SERIES_A, 8, 8, 3);
    seed_stow(&srv, &[&inst]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_W4}/rendered");
    let r = srv.get(&path, &[("Accept", "image/jpeg")]);
    r.assert_status(406);
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("multi-frame"),
        "default-build multi-frame render 406 must name multi-frame, got: {body}"
    );
}

/// Scenario 7e: a saturated transcode pool -> 503 for a compressed thumbnail
/// (Busy BEFORE decode, the same bounded-pool contract as
/// `render_full_from_bytes`); after release the same request -> 200, proving
/// the permit is not leaked on the error path.
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_compressed_busy_pool_503() {
    let srv = TestServer::start_with(|c| c.transcode.max_concurrent = 1);
    let inst = build_compressed_instance(SOP_A, "WADOPAT01", STUDY_A, SERIES_A, false);
    seed_stow(&srv, &[&inst]);
    assert!(srv.pool.try_acquire(), "take the one permit");
    let r = srv.get(&thumbnail_path(), &[]);
    r.assert_status(503);
    srv.pool.release();
    let ok = srv.get(&thumbnail_path(), &[]);
    ok.assert_status(200);
}

/// Scenario 7f: SPP=3 (RGB) compressed instance -> 406 colour unsupported
/// (MONOCHROME-only v1, same contract as `render_full_compressed`).
#[cfg(feature = "transcode")]
#[test]
fn wado_thumbnail_compressed_colour_406() {
    let srv = TestServer::start();
    let inst = build_compressed_instance(SOP_A, "WADOPAT01", STUDY_A, SERIES_A, true);
    seed_stow(&srv, &[&inst]);
    let r = srv.get(&thumbnail_path(), &[]);
    r.assert_status(406);
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("colour unsupported"),
        "406 body must mention colour, got: {body}"
    );
}

/// Scenario 7g (DEFAULT build): a compressed instance thumbnail -> 406 with
/// the PRE-EXISTING message - the `_any` wrapper's default-build double is
/// wire-compatible with the old feature-gate contract (the gate reads ONLY
/// the transfer syntax; the PixelData here is arbitrary bytes).
#[cfg(not(feature = "transcode"))]
#[test]
fn wado_thumbnail_compressed_default_406() {
    // Mirror of build_instance (same tag set, so STOW accepts it) but with a
    // COMPRESSED transfer syntax (JPEG-LS 4.70) and arbitrary PixelData - the
    // thumbnail gate reads ONLY the transfer syntax.
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.70");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", SOP_A.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WADO^SMOKE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"WADOPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", STUDY_A.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", SERIES_A.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", b"junk-pixel-data");
    let srv = TestServer::start();
    seed_stow(&srv, &[&d]);
    let path = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_A}/thumbnail");
    let r = srv.get(&path, &[]);
    r.assert_status(406);
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("thumbnail requires transcode"),
        "406 body must keep the pre-existing message, got: {body}"
    );
}
