// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Freeze STOW-RS wire behaviour.
//!
//! Boots the server over a real TCP socket and exercises seven STOW
//! scenarios (PS3.18 Section 10.5). Deterministic scenarios capture a byte-exact
//! golden fixture (written once during the initial freeze); the rest assert
//! status + named headers + structural properties.
//!
//! The same scenarios also run against `stow_pure.rs`,
//! commit 2) and MUST pass without edits - the fixtures lock in the
//! pre-extraction bytes.
//!
//! All fixture UIDs are NUMERIC-ONLY - the UID grammar
//! gate (ceeb4ef) rejects letter-bearing UIDs, so the letter-UID fixtures
//! frozen here on 13.08 drifted deterministically red. The UIDs were mapped
//! to numeric equivalents and the two byte-exact goldens were DELIBERATELY
//! regenerated (documented act, not a drift to hide).
//! `stow_rejects_invalid_uid_400` pins the gate at the smoke level.
//!
//! Scenarios 11-14 pin the update-in-place semantics (2808):
//! corrected-attribute re-STOW replaces the stored blob and re-stamps QIDO,
//! pixel-only replacement embeds no (0400,0561), byte-identical re-STOW stays
//! Idempotent with zero CAS churn (embed determinism), and the multipart path
//! has update parity with the bytes path. Scenario 15 (update + erase) lives
//! in erase_smoke.rs; the U journal line in access_journal_smoke.rs.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::handlers::json_common::{JsonVal, parse_json};
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
        .join("stow")
}

/// Assert `actual` is byte-exact with a committed golden fixture, unless the
/// `CLARUS_STOW_GENERATE` env var is set (initial freeze only - writes the
/// fixture from the OLD code's output). Once committed, that var is never set
/// in CI, so this becomes a read-only byte-exact check.
fn check_or_generate(name: &str, actual: &[u8]) {
    let path = fixtures_dir().join(name);
    if std::env::var_os("CLARUS_STOW_GENERATE").is_some() {
        std::fs::create_dir_all(fixtures_dir()).expect("stow fixtures dir");
        std::fs::write(&path, actual).expect("write stow fixture");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("fixture missing: {}", path.display()));
    assert_eq!(
        String::from_utf8_lossy(actual),
        expected,
        "stow fixture drift in {name}"
    );
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
/// non-empty pixel data. `patient_id`/`study`/`series` are overridable so the
/// conflict scenario (same SOP UID, different PatientID) stays possible.
#[allow(clippy::too_many_arguments)]
fn build_instance(
    sop_instance_uid: &str,
    patient_id: &str,
    study_uid: &str,
    series_uid: &str,
    pixel_byte: u8,
) -> Vec<u8> {
    build_instance_named(
        sop_instance_uid,
        patient_id,
        "STOW^SMOKE",
        study_uid,
        series_uid,
        pixel_byte,
    )
}

/// same as [`build_instance`] but with an overridable PatientName,
/// so the corrected-name update scenarios can send OLD^NAME then NEW^NAME.
#[allow(clippy::too_many_arguments)]
fn build_instance_named(
    sop_instance_uid: &str,
    patient_id: &str,
    patient_name: &str,
    study_uid: &str,
    series_uid: &str,
    pixel_byte: u8,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_instance_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", patient_name.as_bytes());
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
    let pixels = [pixel_byte; 64];
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
}

/// Build a minimal instance with EMPTY PatientID and PatientName so FR-001
/// `validate_and_coerce` must generate them (AUTOPAT_ / Anonymous^). Used to
/// exercise the STOW Warn Reason (0008,1196) = 45056 (B000) in the response.
fn build_coerced_instance(
    sop_instance_uid: &str,
    study_uid: &str,
    series_uid: &str,
    pixel_byte: u8,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_instance_uid.as_bytes());
    // PatientName (0010,0010) and PatientID (0010,0020) are intentionally absent.
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

// -- Test server harness -------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    _temp: tempfile::TempDir,
    /// Cold-tier CAS root dir, present when booted with `start_cold`.
    cold_root: Option<PathBuf>,
    /// Hot CAS root (W3 scenarios count blobs directly).
    cas_root: PathBuf,
    /// Study-manifest dir (W3 scenarios read the stored blake3 back).
    manifest_dir: PathBuf,
}

impl TestServer {
    /// Boot the full router with preflight disabled for determinism.
    fn start() -> Self {
        Self::start_with(|_| {})
    }

    /// Same, but lets the caller override config (e.g. tiny size limit).
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
        // Disable preflight checks so the freeze is deterministic.
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        f(&mut config);

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
            _temp: temp,
            cold_root: None,
            cas_root: storage_dir,
            manifest_dir,
        }
    }

    /// Boot with an additional filesystem COLD CAS tier (FR-017b), so the
    /// cold-migration layout can be exercised end-to-end (hot -> cold -> WADO).
    fn start_cold() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");

        let storage_dir = temp.path().join("cas");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");
        let cold_dir = temp.path().join("coldcas");
        std::fs::create_dir_all(&cold_dir).expect("cold cas dir");
        let btree_root = temp.path().join("btree");
        std::fs::create_dir_all(&btree_root).expect("btree dir");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");
        let cold_cas = ContentAddressedStorage::new(cold_dir.clone()).expect("open cold cas");

        let mut config = Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        // Disable preflight checks so the freeze is deterministic.
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
            cold_cas: Some(cold_cas),
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
            _temp: temp,
            cold_root: Some(cold_dir),
            cas_root: storage_dir,
            manifest_dir,
        }
    }

    /// Count files under the hot CAS root (W3 churn checks).
    fn cas_file_count(&self) -> usize {
        let mut n = 0;
        let mut dirs = vec![self.cas_root.clone()];
        while let Some(d) = dirs.pop() {
            if let Ok(entries) = std::fs::read_dir(&d) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        dirs.push(p);
                    } else {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    fn post(&self, path: &str, content_type: &str, body: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.addr.port(),
            body.len()
        );
        let mut req = head.into_bytes();
        req.extend_from_slice(body);
        stream.write_all(&req).expect("write req");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        if std::env::var_os("STOW_DEBUG").is_some() {
            eprintln!(
                "=== RESP raw ({} bytes) head: {:?}",
                buf.len(),
                &buf[..buf.len().min(300)]
            );
        }
        parse_http(&buf)
    }

    fn get(&self, path: &str) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            self.addr.port()
        );
        stream.write_all(req.as_bytes()).expect("write req");

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

    /// The single part payload of a `multipart/related` frames response.
    ///
    /// The boundary is taken from Content-Type (not a byte search), so the
    /// extraction is unambiguous: body = `--B\r\n<part headers>\r\n\r\n<part
    /// bytes>\r\n--B--\r\n`.
    fn multipart_part_body(&self) -> Vec<u8> {
        let ct = self.header("content-type").expect("Content-Type header");
        assert!(
            ct.contains("multipart/related"),
            "frames response must be multipart/related"
        );
        let idx = ct.find("boundary=").expect("multipart boundary");
        let boundary = ct[idx + "boundary=".len()..]
            .trim_matches('"')
            .trim_end_matches(';')
            .trim()
            .to_string();
        let open = format!("--{boundary}\r\n");
        let close = format!("\r\n--{boundary}--");
        let start =
            find_subslice(&self.body, open.as_bytes()).expect("opening boundary") + open.len();
        let part_start =
            start + find_subslice(&self.body[start..], b"\r\n\r\n").expect("part header end") + 4;
        let part_end = find_subslice(&self.body[part_start..], close.as_bytes())
            .expect("closing boundary")
            + part_start;
        self.body[part_start..part_end].to_vec()
    }
}

/// First index of `needle` inside `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// -- Multipart request builder -------------------------------------------

const BOUNDARY: &str = "----clarus_stow_boundary_7f3a";

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

fn multipart_ct() -> String {
    format!("multipart/related; type=\"application/dicom\"; boundary={BOUNDARY}")
}

// -- Fixed instances ------------------------------------------------------

const STUDY_A: &str = "1.2.826.0.1.3680043.10.1234.1101";
const SERIES_A: &str = "1.2.826.0.1.3680043.10.1234.1201";

fn sop_a() -> Vec<u8> {
    build_instance(
        "1.2.826.0.1.3680043.10.1234.1301",
        "STOWPAT01",
        STUDY_A,
        SERIES_A,
        0x41,
    )
}
fn sop_b() -> Vec<u8> {
    build_instance(
        "1.2.826.0.1.3680043.10.1234.1302",
        "STOWPAT01",
        STUDY_A,
        SERIES_A,
        0x42,
    )
}

// -- Scenario tests ------------------------------------------------------

const STOW_JSON_CT: &str = "application/dicom+json; charset=utf-8";

/// Scenario 1: multipart, 2 DICOM parts -> 200 + ReferencedSOPSequence (2).
/// Full success is always 200 (fresh stores included).
#[test]
fn stow_multipart_two_instances_golden() {
    let srv = TestServer::start();
    let body = multipart_body(BOUNDARY, &[&sop_a(), &sop_b()]);
    let r = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    r.assert_status(200);
    r.assert_content_type(STOW_JSON_CT);
    let v = parse_json(&r.body).expect("stow json");
    let refs = v
        .obj_get("00081199")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("ReferencedSOPSequence");
    assert_eq!(refs.len(), 2);
    check_or_generate("stow_multipart_two_instances.json", &r.body);
}

/// Scenario 2: re-STOW same two instances -> byte-identical body.
#[test]
fn stow_re_stow_byte_identical() {
    let srv = TestServer::start();
    let body = multipart_body(BOUNDARY, &[&sop_a(), &sop_b()]);
    let r1 = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    r1.assert_status(200);
    // Both fresh stores and exact-duplicate re-STOWs
    // are full success -> 200. The response body stays byte-identical to the
    // first STOW.
    let r2 = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    r2.assert_status(200);
    assert_eq!(
        r2.body, r1.body,
        "re-STOW must be byte-identical to the first STOW"
    );
}

/// Scenario 3: single application/dicom -> 200.
#[test]
fn stow_single_instance_golden() {
    let srv = TestServer::start();
    let r = srv.post("/dicomweb/studies", "application/dicom", &sop_a());
    r.assert_status(200);
    r.assert_content_type(STOW_JSON_CT);
    check_or_generate("stow_single_instance.json", &r.body);
}

// -- Study-scoped STOW: POST /dicomweb/studies/{study} (PS3.18 10.5.1.1) --

/// The study-scoped target (Table 10.5.1-1) accepts an instance whose Study
/// Instance UID equals the one in the URL. Before the route existed this was
/// a 404; the transaction is defined, so 404 was never correct.
#[test]
fn stow_study_scoped_single_instance_is_200() {
    let srv = TestServer::start();
    let path = format!("/dicomweb/studies/{STUDY_A}");
    let r = srv.post(&path, "application/dicom", &sop_a());
    r.assert_status(200);
    r.assert_content_type(STOW_JSON_CT);
    let v = parse_json(&r.body).expect("stow json");
    let refs = v
        .obj_get("00081199")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("ReferencedSOPSequence");
    assert_eq!(refs.len(), 1);
}

/// An instance whose Study Instance UID differs from the URL study is a
/// request-level conflict: 409, and nothing is written (10.5.3-1 lists 409
/// for a Study Instance UID mismatch).
#[test]
fn stow_study_scoped_uid_mismatch_is_409() {
    let srv = TestServer::start();
    let other_study = "1.2.826.0.1.3680043.10.1234.9999";
    let path = format!("/dicomweb/studies/{other_study}");
    let r = srv.post(&path, "application/dicom", &sop_a());
    assert_eq!(r.status, 409, "UID mismatch must be 409, got {}", r.status);

    // The mismatch must not have stored anything: the instance is not
    // retrievable from its own study.
    let wado = format!("/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances");
    let search = srv.get(&wado);
    search.assert_status(200);
    let found = parse_json(&search.body).expect("search json");
    let found = found.as_array().expect("search returns an array");
    assert!(
        found.is_empty(),
        "a mismatched instance must not be written, found {}",
        found.len()
    );
}

/// Re-storing the same instance into its own study stays idempotent: 200, no
/// 409 - the scope check passes and `decide_insert_result` returns Idempotent.
#[test]
fn stow_study_scoped_idempotent_re_store_is_200() {
    let srv = TestServer::start();
    let path = format!("/dicomweb/studies/{STUDY_A}");
    let r1 = srv.post(&path, "application/dicom", &sop_a());
    r1.assert_status(200);
    let r2 = srv.post(&path, "application/dicom", &sop_a());
    r2.assert_status(200);
    assert_eq!(
        r2.body, r1.body,
        "an idempotent re-store must answer byte-identically"
    );
}

/// A multipart request whose first part belongs to the target study but whose
/// second part belongs to a foreign study is a whole-request 409, and the
/// foreign instance is never written.
#[test]
fn stow_study_scoped_multipart_with_a_foreign_part_is_409() {
    let srv = TestServer::start();
    let foreign_study = "1.2.826.0.1.3680043.10.1234.7777";
    let foreign = build_instance(
        "1.2.826.0.1.3680043.10.1234.1401",
        "STOWPAT01",
        foreign_study,
        SERIES_A,
        0x43,
    );
    let path = format!("/dicomweb/studies/{STUDY_A}");
    let body = multipart_body(BOUNDARY, &[&sop_a(), &foreign]);
    let r = srv.post(&path, &multipart_ct(), &body);
    assert_eq!(
        r.status, 409,
        "a foreign part must fail the request: {}",
        r.status
    );

    // Nothing from the foreign study may be retrievable.
    let wado = format!("/dicomweb/studies/{foreign_study}/series/{SERIES_A}/instances");
    let search = srv.get(&wado);
    search.assert_status(200);
    let found = parse_json(&search.body).expect("search json");
    let found = found.as_array().expect("search returns an array");
    assert!(
        found.is_empty(),
        "the foreign instance must not have been written, found {}",
        found.len()
    );
}

/// Scenario 3b: single application/dicom with a transfer-syntax parameter.
///
/// PS3.18 8.7.3.1 gives the media type an optional `transfer-syntax`
/// parameter, and 10.5.1.4 lets a single-part STOW-RS body name its encoding
/// that way. The transfer syntax actually travels in the DICOM File Meta
/// (0002,0010), so the parameter is redundant - which is exactly why it must
/// not change the answer. Before the fix the handler compared the whole header
/// and fell through to the multipart branch, answering
/// `400 "Expected multipart/related Content-Type"` to a conformant request.
///
/// The body and status are compared against the BARE form rather than against
/// a literal, so the two spellings cannot drift into answering differently.
#[test]
fn stow_single_instance_with_a_transfer_syntax_parameter() {
    let srv = TestServer::start();
    let bare = srv.post("/dicomweb/studies", "application/dicom", &sop_a());
    bare.assert_status(200);

    let parameterized = srv.post(
        "/dicomweb/studies",
        "application/dicom; transfer-syntax=1.2.840.10008.1.2.1",
        &sop_a(),
    );
    parameterized.assert_status(200);
    parameterized.assert_content_type(STOW_JSON_CT);
    assert_eq!(
        parameterized.body, bare.body,
        "the parameter is a modifier of the media type, not a different request"
    );
}

/// The negative: a structured-syntax subtype is NOT application/dicom.
///
/// `application/dicom+json` carries a JSON body, so it must take the same path
/// the non-DICOM types take - the rejection branch, and its unsupported-media-
/// type 415. If the base-media-type match were a prefix test instead of an
/// equality, this is the request that would break, and the failure would be a
/// JSON body read as a DICOM instance.
#[test]
fn a_structured_syntax_subtype_is_not_treated_as_application_dicom() {
    let srv = TestServer::start();
    let r = srv.post(
        "/dicomweb/studies",
        "application/dicom+json",
        br#"{"00080018":{"Value":["1.2.3"]}}"#,
    );
    // PS3.18 10.5.3-1: `application/dicom+json` is neither the single-part
    // `application/dicom` nor `multipart/related`, so it is an unsupported
    // media type (415) rather than a malformed request (400).
    r.assert_status(415);
    let body_text = String::from_utf8_lossy(&r.body).to_string();
    assert!(
        body_text.contains("multipart/related"),
        "the refusal must name the expected media type: {body_text}"
    );
}

/// Scenario 4: a mix of one valid + one invalid part -> 202 + FailedSOPSequence.
/// (All-parts-fail would be a 400 Bad Request; 202 is the PARTIAL-success path.)
#[test]
fn stow_invalid_dicom_202() {
    let srv = TestServer::start();
    let garbage: &[u8] = b"this is not a dicom file, just random bytes";
    let body = multipart_body(BOUNDARY, &[&sop_a(), garbage]);
    let r = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    r.assert_status(202);
    r.assert_content_type(STOW_JSON_CT);
    let v = parse_json(&r.body).expect("stow json");
    // One accepted (ReferencedSOPSequence) + one failed (FailedSOPSequence).
    assert_eq!(
        v.obj_get("00081199")
            .and_then(|x| x.obj_get("Value"))
            .and_then(|x| x.as_array())
            .map(|a| a.len()),
        Some(1),
        "one instance should be accepted"
    );
    let fails = v
        .obj_get("00081198")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("FailedSOPSequence");
    assert_eq!(fails.len(), 1);
    assert!(
        r.header("warning").is_some_and(|w| w.contains("1 failure")),
        "expected Warning header, got {:?}",
        r.header("warning")
    );
}

/// Scenario 5: non-DICOM (metadata) part -> skipped, not in response.
#[test]
fn stow_metadata_part_skipped() {
    let srv = TestServer::start();
    let boundary = "----clarus_stow_boundary_meta";
    let a = sop_a();
    let meta: &[u8] = br#"{"contentType":"application/json"}"#;
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(&a);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/json\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(meta);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let ct = format!("multipart/related; type=\"application/dicom\"; boundary={boundary}");

    let r = srv.post("/dicomweb/studies", &ct, &body);
    r.assert_status(200);
    let v = parse_json(&r.body).expect("stow json");
    let refs = v
        .obj_get("00081199")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("ReferencedSOPSequence");
    assert_eq!(refs.len(), 1, "metadata part must be skipped");
    assert!(
        v.obj_get("00081198").is_none(),
        "no FailedSOPSequence expected"
    );
}

/// Scenario 6a (multipart): DICOM part over the size limit -> Failed entry (202,
/// because a valid part is also present -> partial success).
#[test]
fn stow_multipart_oversize_failed() {
    let srv = TestServer::start_with(|c| {
        c.max_instance_size_mb = 1; // 1 MiB limit
    });
    let mut big = sop_b();
    let huge: Vec<u8> = vec![0u8; 2 * 1024 * 1024];
    append_element(&mut big, 0x7FE0, 0x0010, b"OB", &huge);
    let big = big;
    let body = multipart_body(BOUNDARY, &[&sop_a(), &big]);
    let r = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    r.assert_status(202);
    r.assert_content_type(STOW_JSON_CT);
    let v = parse_json(&r.body).expect("stow json");
    // One accepted, one failed (oversize).
    assert_eq!(
        v.obj_get("00081199")
            .and_then(|x| x.obj_get("Value"))
            .and_then(|x| x.as_array())
            .map(|a| a.len()),
        Some(1)
    );
    let fails = v
        .obj_get("00081198")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("FailedSOPSequence");
    assert_eq!(fails.len(), 1);
    assert!(
        r.header("warning").is_some_and(|w| w.contains("1 failure")),
        "expected Warning header, got {:?}",
        r.header("warning")
    );
}

/// Scenario 6b (single): oversize application/dicom -> 413 with `application/json`.
#[test]
fn stow_single_oversize_413() {
    let srv = TestServer::start_with(|c| {
        c.max_instance_size_mb = 1;
    });
    let mut big = sop_b();
    let huge: Vec<u8> = vec![0u8; 2 * 1024 * 1024];
    append_element(&mut big, 0x7FE0, 0x0010, b"OB", &huge);
    let r = srv.post("/dicomweb/studies", "application/dicom", &big);
    r.assert_status(413);
    // Single-instance 413 comes from `to_bytes` error -> plain application/json.
    r.assert_content_type("application/json");
    let v = parse_json(&r.body).expect("error json");
    assert_eq!(
        v.obj_get("error").and_then(|x| x.as_str()),
        Some("Payload Too Large")
    );
}

/// Scenario 7: same SOPInstanceUID, different PatientID -> Conflict.
#[test]
fn stow_sop_conflict() {
    let srv = TestServer::start();
    let inst_a = build_instance(
        "1.2.826.0.1.3680043.10.1234.1305",
        "STOWPAT01",
        STUDY_A,
        SERIES_A,
        0x51,
    );
    let inst_b = build_instance(
        "1.2.826.0.1.3680043.10.1234.1305",
        "STOWPAT02",
        "1.2.826.0.1.3680043.10.1234.1102",
        "1.2.826.0.1.3680043.10.1234.1202",
        0x52,
    );
    let body1 = multipart_body(BOUNDARY, &[&inst_a]);
    let r1 = srv.post("/dicomweb/studies", &multipart_ct(), &body1);
    r1.assert_status(200);
    let body2 = multipart_body(BOUNDARY, &[&inst_b]);
    let r2 = srv.post("/dicomweb/studies", &multipart_ct(), &body2);
    r2.assert_status(409);
    r2.assert_content_type(STOW_JSON_CT);
    let v = parse_json(&r2.body).expect("stow json");
    let fails = v
        .obj_get("00081198")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("FailedSOPSequence");
    assert_eq!(fails.len(), 1);
    assert!(
        v.obj_get("00081198")
            .and_then(|x| x.obj_get("Value"))
            .and_then(|x| x.first_of_array())
            .and_then(|e| e.obj_get("00081197"))
            .and_then(|x| x.obj_get("Value"))
            .and_then(|x| x.first_of_array())
            .and_then(|x| x.as_u64())
            == Some(272),
        "failure reason must be 272"
    );
}

/// 0710: after a successful STOW the blob AND the referencing manifest both
/// exist on disk (the durability ORDER that the fsync barrier enforces: the
/// blob is materialized during instance ingest, the manifest is written and
/// synced at request end). The blob is addressed by its BLAKE3, so its
/// presence at `cas/<xx>/<hex>` is the durable-artifact proof.
#[test]
fn stow_leaves_blob_and_manifest_both_present() {
    let srv = TestServer::start();
    let inst = sop_a();
    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    assert_eq!(r.status, 200, "clean STOW must succeed");

    // Blob present at its content address (same geometry the server uses).
    let hex = blake3::hash(&inst).to_hex().to_string();
    let blob = srv
        .cas_root
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(format!("{hex}.dcm"));
    assert!(blob.is_file(), "the CAS blob must exist at {blob:?}");

    // Manifest present AND referencing exactly that blob hash.
    let path = clarus_core::stow::manifest_path(&srv.manifest_dir, STUDY_A);
    let m = clarus_core::manifest::read_manifest(&path).expect("manifest reads back");
    let rec = m
        .find_instance("1.2.826.0.1.3680043.10.1234.1301")
        .expect("instance recorded");
    assert_eq!(
        clarus_adapters::hex_encode(&rec.blake3),
        hex,
        "the manifest row addresses the blob that is on disk"
    );
}

/// 0710: a CAS blob write failure must FAIL the instance (never a silent 200);
/// the client then keeps its copy. Injection targets the BLOB materialization
/// specifically: the temp file streams normally, but a file is planted where
/// the CAS bucket DIRECTORY must be created, so `rename_temp_to_cas` cannot
/// publish the blob. The response must be a failure (not 200) and the study
/// manifest must NOT reference the instance.
#[test]
fn stow_blob_write_failure_is_never_a_silent_200() {
    let srv = TestServer::start();
    // Bucket dir for this instance's blob = cas/<xx>/<yy>.
    let digest = blake3::hash(&sop_a());
    let hex = digest.to_hex().to_string();
    let bucket = srv.cas_root.join(&hex[..2]).join(&hex[2..4]);
    std::fs::create_dir_all(&bucket).expect("cas root");
    // Replace the bucket dir with a FILE so `create_dir_all(bucket)` fails.
    std::fs::remove_dir(&bucket).expect("remove empty bucket dir");
    std::fs::write(&bucket, b"not a directory").expect("plant bucket blocker");

    let r = srv.post("/dicomweb/studies", "application/dicom", &sop_a());
    assert_ne!(r.status, 200, "a blob write failure must not answer 200");
    assert!(
        r.status >= 400,
        "the failure must be visible to the client, got {}",
        r.status
    );
    // No manifest may reference an instance whose blob was never published.
    let path = clarus_core::stow::manifest_path(&srv.manifest_dir, STUDY_A);
    if let Ok(m) = clarus_core::manifest::read_manifest(&path) {
        assert!(
            m.find_instance("1.2.826.0.1.3680043.10.1234.1301")
                .is_none(),
            "the failed instance must not be recorded in the manifest"
        );
    }
}

/// Scenario 8: an instance with EMPTY PatientID / PatientName is coerced by
/// FR-001 -> its ReferencedSOPSequence entry carries Warn Reason
/// (0008,1196) = 45056 (B000, Coercion of Data Elements). A clean instance
/// carries no (0008,1196).
#[test]
fn stow_coercion_emits_b000() {
    let srv = TestServer::start();
    let coerced_sop = "1.2.826.0.1.3680043.10.1234.1303";
    let coerced = build_coerced_instance(coerced_sop, STUDY_A, SERIES_A, 0x61);
    // A clean instance in the SAME request (no coercion) for contrast.
    let clean_sop = "1.2.826.0.1.3680043.10.1234.1304";
    let clean = build_instance(clean_sop, "STOWPAT01", STUDY_A, SERIES_A, 0x62);

    let body = multipart_body(BOUNDARY, &[&coerced, &clean]);
    let r = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    r.assert_status(202);
    // 202: stored with warnings (coerced Type 2 fields or an in-place update) - PS3.18 10.5.2.
    r.assert_content_type(STOW_JSON_CT);
    let v = parse_json(&r.body).expect("stow json");
    let refs = v
        .obj_get("00081199")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("ReferencedSOPSequence");
    assert_eq!(refs.len(), 2);

    // The coerced entry carries B000; the clean entry does not.
    let mut coerced_entry: Option<&JsonVal> = None;
    let mut clean_entry: Option<&JsonVal> = None;
    for e in refs {
        let si = e
            .obj_get("00081155")
            .and_then(|x| x.obj_get("Value"))
            .and_then(|x| x.as_str_array_first())
            .unwrap_or("");
        if si == coerced_sop {
            coerced_entry = Some(e);
        } else if si == clean_sop {
            clean_entry = Some(e);
        }
    }
    let coerced_entry = coerced_entry.expect("coerced referenced sop present");
    let clean_entry = clean_entry.expect("clean referenced sop present");

    assert_eq!(
        coerced_entry
            .obj_get("00081196")
            .and_then(|x| x.obj_get("Value")),
        Some(&JsonVal::Arr(vec![JsonVal::U64(45056)])),
        "coerced instance must carry Warn Reason 45056"
    );
    assert!(
        clean_entry.obj_get("00081196").is_none(),
        "clean instance must not carry (0008,1196)"
    );
}

/// Scenario 10: cold-tier
/// layout unity. STOW -> migrate hot file to COLD at the canonical TWO-LEVEL
/// path -> WADO 200. Regression guard for the cold-write-vs-WADO-read bug,
/// where the cold write used one level (`{xx}/{hash}.dcm`) but WADO reads via
/// `cas_path` two level (`{xx}/{yy}/{hash}.dcm`) -> 404.
#[test]
fn stow_cold_migrate_wado_200() {
    let srv = TestServer::start_cold();
    let hot_root = srv
        .cold_root
        .as_ref()
        .unwrap()
        .parent()
        .unwrap()
        .join("cas");
    let cold_root = srv.cold_root.as_ref().unwrap();

    // Numeric-only UIDs so WADO's UID validation accepts them on the URL.
    let study_uid = "1.2.826.0.1.3680043.10.1234.100";
    let series_uid = "1.2.826.0.1.3680043.10.1234.101";
    let sop_uid = "1.2.826.0.1.3680043.10.1234.102";

    // 1. STOW one instance with valid UIDs.
    let inst = build_instance(sop_uid, "STOWPAT01", study_uid, series_uid, 0x41);
    let body = multipart_body(BOUNDARY, &[&inst]);
    let r = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    r.assert_status(200);

    // 2. BLAKE3 of the stored bytes -> the CAS key (same as the STOW hot path).
    let hash = clarus_adapters::hex_encode(&clarus_server::shadow::blake3_of(&inst));
    let (hi, lo) = (&hash[..2], &hash[2..4]);
    let hot_file = hot_root.join(hi).join(lo).join(format!("{hash}.dcm"));
    let cold_file = cold_root.join(hi).join(lo).join(format!("{hash}.dcm"));
    assert!(hot_file.exists(), "hot CAS file must exist after STOW");

    // 3. Migrate hot -> cold at the canonical two-level path; drop the hot copy.
    let bytes = std::fs::read(&hot_file).expect("read hot file");
    if let Some(parent) = cold_file.parent() {
        std::fs::create_dir_all(parent).expect("cold bucket dirs");
    }
    std::fs::write(&cold_file, &bytes).expect("write cold file");
    std::fs::remove_file(&hot_file).expect("remove hot file");

    // 4. WADO retrieve the instance -> 200 (found in COLD via two-level path).
    let w = srv.get(&format!(
        "/dicomweb/studies/{study_uid}/series/{series_uid}/instances/{sop_uid}"
    ));
    w.assert_status(200);
    // The full DICOM body came back from cold, not a 404/empty object.
    assert!(
        w.body.len() >= inst.len(),
        "WADO body must be at least the stored instance, got {}",
        w.body.len()
    );
}

/// Scenario 9: - the restored request-level disk-space preflight
/// (`preflight_disk`) returns 507 + Retry-After BEFORE any body is parsed.
/// min_free_disk_mb is forced absurdly high so the guard always trips.
/// (The mutate closure runs AFTER the harness zeroes the preflight knobs, so
/// this is the one place a high floor is honored.)
#[test]
fn stow_disk_preflight_507_retry_after() {
    let srv = TestServer::start_with(|c| {
        c.min_free_disk_mb = u64::MAX;
    });
    let r = srv.post("/dicomweb/studies", "application/dicom", &sop_a());
    r.assert_status(507);
    // The preflight is a plain error response (JSON), not a STOW DICOM-JSON body.
    r.assert_content_type("application/json; charset=utf-8");
    let body_text = String::from_utf8_lossy(&r.body).to_string();
    assert!(
        body_text.contains("Insufficient Storage"),
        "expected Insufficient Storage body, got: {body_text}"
    );
    let retry = r.header("retry-after").expect("Retry-After header");
    assert_eq!(retry.trim(), "300", "Retry-After must be 300");
}

/// Regression pin: a StudyInstanceUID containing a LETTER is
/// rejected by the STOW UID grammar gate (validate_instance_uids) with 400 +
/// FailedSOPSequence (00081198) whose 00000902 reason contains
/// "Invalid DICOM UID". Pins the gate at the smoke level so the letter-UID
/// drift class cannot recur silently (all fixture UIDs in this file are
/// numeric-only since 28.08 for exactly this reason).
#[test]
fn stow_rejects_invalid_uid_400() {
    let srv = TestServer::start();
    let inst = build_instance(
        "1.2.826.0.1.3680043.10.1234.1306",
        "STOWPAT01",
        "1.2.826.0.1.3680043.10.1234.bad.study",
        SERIES_A,
        0x71,
    );
    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    r.assert_status(400);
    r.assert_content_type(STOW_JSON_CT);
    let v = parse_json(&r.body).expect("stow json");
    let fails = v
        .obj_get("00081198")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("FailedSOPSequence");
    assert_eq!(fails.len(), 1, "the letter-UID instance must be rejected");
    let reason = fails[0]
        .obj_get("00000902")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_str_array_first())
        .expect("00000902 reason");
    assert!(
        reason.contains("Invalid DICOM UID"),
        "reason must name the UID gate, got: {reason}"
    );
}

// -- W3 (2808): update-in-place scenarios 11-14 -------------------------

/// Read the stored instance's BLAKE3 back out of the study manifest (the
/// archive's source of truth for the CAS address).
fn manifest_blake3(manifest_dir: &std::path::Path, study_uid: &str, sop_uid: &str) -> [u8; 32] {
    let path = clarus_core::stow::manifest_path(manifest_dir, study_uid);
    let m = clarus_core::manifest::read_manifest(&path).expect("manifest reads back");
    m.find_instance(sop_uid)
        .expect("instance in manifest")
        .blake3
}

/// The canonical stored blob must hash to the manifest's instance record.
fn assert_manifest_points_at_blob(
    manifest_dir: &std::path::Path,
    study: &str,
    sop: &str,
    blob: &[u8],
) {
    assert_eq!(
        clarus_adapters::hex_encode(&manifest_blake3(manifest_dir, study, sop)),
        clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(blob)),
        "manifest instance record must point at the served blob"
    );
}

/// Scenario 11: re-STOW the same SOP with a corrected PatientName -> 200,
/// WADO serves the NEW canonical blob (original bytes + (0400,0561) with the
/// old name + reason CORRECT), QIDO shows the corrected name, and the
/// manifest re-points to the embedded blake3.
#[test]
fn stow_update_corrected_pn_replaces_blob() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1307";
    let old_inst = build_instance_named(sop, "STOWPAT01", "OLD^NAME", STUDY_A, SERIES_A, 0x71);

    let first = srv.post("/dicomweb/studies", "application/dicom", &old_inst);
    first.assert_status(200);

    // Pre-update WADO serves the ORIGINAL bytes (no coercion -> no embed).
    let w0 = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w0.assert_status(200);
    assert_eq!(
        w0.body, old_inst,
        "pre-update WADO serves the original bytes"
    );
    let original_len = old_inst.len();

    // Corrected PatientName, identical pixels.
    let new_inst = build_instance_named(sop, "STOWPAT01", "NEW^NAME", STUDY_A, SERIES_A, 0x71);
    let upd = srv.post("/dicomweb/studies", "application/dicom", &new_inst);
    // 202: stored with warnings (coerced Type 2 fields or an in-place update) - PS3.18 10.5.2.
    upd.assert_status(202);
    upd.assert_content_type(STOW_JSON_CT);
    // Updated -> ReferencedSOPSequence with the same shape as Stored.
    let v = parse_json(&upd.body).expect("stow json");
    let refs = v
        .obj_get("00081199")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("ReferencedSOPSequence");
    assert_eq!(refs.len(), 1, "the update references the replaced SOP");

    // WADO serves the canonical blob: original bytes + (0400,0561) appended.
    let w1 = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w1.assert_status(200);
    assert!(
        w1.body.len() > original_len,
        "updated blob must carry the appended 0561, got {} vs {}",
        w1.body.len(),
        original_len
    );
    let text = String::from_utf8_lossy(&w1.body);
    assert!(
        text.contains("CORRECT"),
        "0561 reason must be CORRECT (operator correction)"
    );
    assert!(text.contains("OLD^NAME"), "0561 must carry the OLD value");
    assert!(
        text.contains("CLARUS_SERVER"),
        "0561 ModifyingSystem must be CLARUS_SERVER"
    );

    // The manifest re-points to the embedded blob.
    assert_manifest_points_at_blob(&srv.manifest_dir, STUDY_A, sop, &w1.body);

    // QIDO returns the corrected name (manifest re-stamp -> RAM index).
    let q = srv.get("/dicomweb/studies");
    q.assert_status(200);
    let qtext = String::from_utf8_lossy(&q.body);
    assert!(
        qtext.contains("NEW^NAME"),
        "QIDO must show the corrected PatientName, got: {qtext}"
    );
    assert!(
        !qtext.contains("OLD^NAME"),
        "QIDO must not show the old PatientName"
    );
}

/// Scenario 12: byte-identical re-STOW stays Idempotent with ZERO CAS churn -
/// both for a plain instance and for a coerced one (the COERCE embed lives in
/// a separate audit copy, so a repeated upload maps to the same canonical
/// bytes and never rewrites the manifest-referenced blob).
#[test]
fn stow_update_identical_restow_stays_idempotent_zero_churn() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1308";
    let inst = build_instance(sop, "STOWPAT01", STUDY_A, SERIES_A, 0x71);

    srv.post("/dicomweb/studies", "application/dicom", &inst)
        .assert_status(200);
    let after_insert = srv.cas_file_count();

    let r1 = srv.post("/dicomweb/studies", "application/dicom", &inst);
    r1.assert_status(200);
    // 202: stored with warnings (coerced Type 2 fields or an in-place update) - PS3.18 10.5.2.
    let r2 = srv.post("/dicomweb/studies", "application/dicom", &inst);
    r2.assert_status(200);
    // Byte-identical Idempotent responses, byte-identical CAS tree.
    assert_eq!(r1.body, r2.body, "idempotent responses are byte-identical");
    assert_eq!(
        srv.cas_file_count(),
        after_insert,
        "identical re-STOW must not churn CAS"
    );

    // Embed determinism on the COERCE path: one original blob + one audit
    // copy; a repeated upload of the same file adds nothing. The coerced
    // instance gets its OWN study (generated AUTOPAT PatientID must not
    // collide with the STOWPAT01 instance already in STUDY_A).
    let coerced_sop = "1.2.826.0.1.3680043.10.1234.1309";
    let coerced = build_coerced_instance(
        coerced_sop,
        "1.2.826.0.1.3680043.10.1234.1104",
        "1.2.826.0.1.3680043.10.1234.1204",
        0x71,
        // 202: stored with warnings (coerced Type 2 fields or an in-place update) - PS3.18 10.5.2.
    );
    let c1 = srv.post("/dicomweb/studies", "application/dicom", &coerced);
    c1.assert_status(202);
    assert!(
        String::from_utf8_lossy(&c1.body).contains("45056"),
        "coerced insert carries the B000 warn reason"
    );
    let after_coerced = srv.cas_file_count();
    assert_eq!(
        after_coerced,
        after_insert + 1,
        // 202: stored with warnings (coerced Type 2 fields or an in-place update) - PS3.18 10.5.2.
        "3008: coerced insert = original blob only (placeholder coercion is journaled, no alibi blob)"
    );
    let c2 = srv.post("/dicomweb/studies", "application/dicom", &coerced);
    c2.assert_status(202);
    assert_eq!(
        srv.cas_file_count(),
        after_coerced,
        "repeated coerced upload must be idempotent (no blob churn)"
    );
}

// -- 3008 hidden-PHI sanitization ----------------------------------------

/// Build a hidden-PHI fixture: a coerced instance (empty patient fields)
/// whose OWN top-level (0400,0561) embed carries a non-empty PatientName (a
/// sloppy upstream anonymizer shape). The embed is the shared validation
/// builder output - exactly what the server itself appends on updates.
fn build_hidden_phi_instance(
    sop_instance_uid: &str,
    study_uid: &str,
    series_uid: &str,
    pixel_byte: u8,
) -> Vec<u8> {
    let mut d = build_coerced_instance(sop_instance_uid, study_uid, series_uid, pixel_byte);
    let items = [clarus_server::validation::OriginalAttributeItem {
        group: 0x0010,
        element: 0x0010,
        old_value: "HIDDEN^NAME".to_string(),
        reason: "CORRECT",
    }];
    d.extend_from_slice(
        &clarus_server::validation::build_original_attributes_sequence(
            &items,
            "STOW-RS",
            "CLARUS_SERVER:8024",
        ),
    );
    d
}

/// Little-endian (group, element) tag byte pattern search.
fn contains_tag(bytes: &[u8], group: u16, element: u16) -> bool {
    let pat = [group.to_le_bytes(), element.to_le_bytes()].concat();
    bytes.windows(4).any(|w| w == pat)
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Non-empty `hex tier` lines of a sidecar file (empty when absent).
fn sidecar_lines(path: &std::path::Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 3008 5c: a file with empty patient fields + its own 0561 containing a
/// non-empty PatientName -> the served bytes contain NO 0400,0550/0561; the
/// `{study}.audited` sidecar has one line; the endpoint shows tracked=1; a
/// repeat upload is Updated again and the sidecar still has ONE line (append
/// dedup).
#[test]
fn stow_hidden_phi_0561_sanitized_tracked_and_dedup() {
    let srv = TestServer::start();
    let study = "1.2.826.0.1.3680043.10.1234.1150";
    let series = "1.2.826.0.1.3680043.10.1234.1250";
    // 202: stored with warnings (coerced Type 2 fields or an in-place update) - PS3.18 10.5.2.
    let sop = "1.2.826.0.1.3680043.10.1234.1350";
    let hidden = build_hidden_phi_instance(sop, study, series, 0x50);

    let r = srv.post("/dicomweb/studies", "application/dicom", &hidden);
    r.assert_status(202);
    assert!(
        String::from_utf8_lossy(&r.body).contains("45056"),
        "coerced insert carries the B000 warn reason"
    );

    // WADO serves the SANITIZED bytes: no 0400,0550/0561 tags anywhere.
    let w = srv.get(&format!(
        "/dicomweb/studies/{study}/series/{series}/instances/{sop}"
    ));
    w.assert_status(200);
    assert!(!contains_tag(&w.body, 0x0400, 0x0550), "0550 excised");
    assert!(!contains_tag(&w.body, 0x0400, 0x0561), "0561 excised");
    assert!(!contains_bytes(&w.body, b"HIDDEN^NAME"), "hidden name gone");

    // Alibi: the ORIGINAL bytes recorded in {study}.audited (one line, the
    // incoming hash - never recomputed).
    let sidecar = srv.manifest_dir.join(format!("{study}.audited"));
    let lines = sidecar_lines(&sidecar);
    assert_eq!(lines.len(), 1, "one alibi line: {lines:?}");
    let alibi_hex = lines[0].split_once(' ').expect("hex tier").0;
    assert_eq!(
        alibi_hex,
        clarus_adapters::hex_encode(blake3::hash(&hidden).as_bytes()),
        "alibi line = incoming blake3"
    );

    // Endpoint: tracked=1 (the W4 view).
    clarus_server::audit_copies::refresh(&srv.manifest_dir);
    let ac = srv.get("/admin/v1/storage/audit-copies");
    ac.assert_status(200);
    let ac_body = String::from_utf8_lossy(&ac.body).to_string();
    assert!(
        ac_body.contains("\"total_tracked\":1"),
        "tracked=1: {ac_body}"
    );

    // 202: stored with warnings (coerced Type 2 fields or an in-place update) - PS3.18 10.5.2.
    // Repeat upload -> Updated again (incoming hash = original, manifest hash
    // = sanitized, never equal - accepted, like repeated CORRECT); the
    // .audited sidecar still has ONE line (append dedup) and the replaced
    // sanitized blob is orphan-accounted.
    let r2 = srv.post("/dicomweb/studies", "application/dicom", &hidden);
    r2.assert_status(202);
    assert_eq!(
        sidecar_lines(&sidecar).len(),
        1,
        "append dedup: still one alibi line"
    );
    let orphans = srv.manifest_dir.join(format!("{study}.orphans"));
    assert_eq!(
        sidecar_lines(&orphans).len(),
        1,
        "update orphans the replaced sanitized blob"
    );
}

/// 3008 5c companion: a placeholder-only file (no 0561) -> NO `.audited`
/// line and NO extra blob (only the original lands in CAS).
#[test]
fn stow_placeholder_coerce_no_alibi_blob() {
    let srv = TestServer::start();
    // 202: stored with warnings (coerced Type 2 fields or an in-place update) - PS3.18 10.5.2.
    let study = "1.2.826.0.1.3680043.10.1234.1151";
    let series = "1.2.826.0.1.3680043.10.1234.1251";
    let sop = "1.2.826.0.1.3680043.10.1234.1351";
    let before = srv.cas_file_count();
    let coerced = build_coerced_instance(sop, study, series, 0x51);
    let r = srv.post("/dicomweb/studies", "application/dicom", &coerced);
    r.assert_status(202);
    assert!(
        String::from_utf8_lossy(&r.body).contains("45056"),
        "coerced insert carries the B000 warn reason"
    );
    assert_eq!(
        srv.cas_file_count(),
        before + 1,
        "only the original blob (3008: no COERCE alibi blob)"
    );
    assert!(
        !srv.manifest_dir.join(format!("{study}.audited")).exists(),
        "placeholder-only coercion writes no .audited sidecar"
    );
}

/// 3008: the journal is the record. A coerced INSERT emits a U line with
/// reason "coerce_insert"; a hidden-PHI sanitization with reason
/// "hidden_phi_excised" (distinct from CORRECT/COERCE).
#[test]
fn stow_u_events_journal_coerce_and_hidden_phi_reasons() {
    let dir = tempfile::tempdir().expect("temp dir");
    let writer = clarus_server::access_log::init(dir.path(), 7, false);
    let srv = TestServer::start();

    let coerced = build_coerced_instance(
        "1.2.826.0.1.3680043.10.1234.1352",
        "1.2.826.0.1.3680043.10.1234.1152",
        "1.2.826.0.1.3680043.10.1234.1252",
        0x52,
    );
    srv.post("/dicomweb/studies", "application/dicom", &coerced)
        .assert_status(202);

    let hidden = build_hidden_phi_instance(
        "1.2.826.0.1.3680043.10.1234.1353",
        "1.2.826.0.1.3680043.10.1234.1153",
        "1.2.826.0.1.3680043.10.1234.1253",
        0x53,
    );
    srv.post("/dicomweb/studies", "application/dicom", &hidden)
        .assert_status(202);

    // Dropping the writer handle drains the ring (graceful shutdown).
    drop(writer);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut text = String::new();
    while std::time::Instant::now() < deadline {
        text.clear();
        if let Ok(entries) = std::fs::read_dir(dir.path()) {
            for e in entries.flatten() {
                if let Ok(t) = std::fs::read_to_string(e.path()) {
                    text.push_str(&t);
                }
            }
        }
        if text.contains("\"reason\":\"coerce_insert\"")
            && text.contains("\"reason\":\"hidden_phi_excised\"")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        text.contains("\"reason\":\"coerce_insert\""),
        "coerced insert journaled with coerce_insert: {text}"
    );
    assert!(
        text.contains("\"reason\":\"hidden_phi_excised\""),
        "hidden-PHI sanitization journaled with hidden_phi_excised: {text}"
    );
}

/// Scenario 13: pixel-only replacement (attrs unchanged) -> Updated with
/// changed_attrs = 0: the stored blob is the plain new bytes (NO 0561), WADO
/// serves the new pixels.
#[test]
fn stow_update_pixel_only_no_0561() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1310";
    let old_inst = build_instance_named(sop, "STOWPAT01", "FIXED^NAME", STUDY_A, SERIES_A, 0x71);

    srv.post("/dicomweb/studies", "application/dicom", &old_inst)
        .assert_status(200);

    let new_inst = build_instance_named(sop, "STOWPAT01", "FIXED^NAME", STUDY_A, SERIES_A, 0x72);
    let upd = srv.post("/dicomweb/studies", "application/dicom", &new_inst);
    upd.assert_status(202);

    let w = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w.assert_status(200);
    assert_eq!(
        w.body, new_inst,
        "pixel-only update stores the plain new bytes (no 0561)"
    );
    assert_manifest_points_at_blob(&srv.manifest_dir, STUDY_A, sop, &w.body);
}

/// Scenario 14: multipart update parity - scenario 11 via multipart: the
/// corrected-PN part replaces the stored blob exactly like the bytes path.
#[test]
fn stow_update_multipart_parity() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1311";
    let old_inst = build_instance_named(sop, "STOWPAT01", "OLD^NAME", STUDY_A, SERIES_A, 0x71);

    let body = multipart_body(BOUNDARY, &[&old_inst]);
    srv.post("/dicomweb/studies", &multipart_ct(), &body)
        .assert_status(200);

    let new_inst = build_instance_named(sop, "STOWPAT01", "NEW^NAME", STUDY_A, SERIES_A, 0x71);
    let body = multipart_body(BOUNDARY, &[&new_inst]);
    let upd = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    upd.assert_status(202);

    let w = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w.assert_status(200);
    assert!(w.body.len() > new_inst.len(), "updated blob carries 0561");
    let text = String::from_utf8_lossy(&w.body);
    assert!(
        text.contains("CORRECT"),
        "multipart update embeds CORRECT 0561"
    );
    assert!(text.contains("OLD^NAME"), "0561 carries the old name");
    assert_manifest_points_at_blob(&srv.manifest_dir, STUDY_A, sop, &w.body);
}

/// 3008 (0400,0563): an Updated blob embeds the CONFIGURED ModifyingSystem
/// value; the hardcoded CLARUS_SERVER is gone.
#[test]
fn stow_updated_blob_embeds_configured_modifying_system() {
    let srv = TestServer::start_with(|cfg| {
        cfg.modifying_system = Some("TEST-NODE-7".to_string());
    });
    let sop = "1.2.826.0.1.3680043.10.1234.1320";
    let old_inst = build_instance_named(sop, "STOWPAT01", "OLD^NAME", STUDY_A, SERIES_A, 0x71);
    srv.post("/dicomweb/studies", "application/dicom", &old_inst)
        .assert_status(200);
    let new_inst = build_instance_named(sop, "STOWPAT01", "NEW^NAME", STUDY_A, SERIES_A, 0x71);
    srv.post("/dicomweb/studies", "application/dicom", &new_inst)
        .assert_status(202);
    let w = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w.assert_status(200);
    let text = String::from_utf8_lossy(&w.body);
    assert!(
        text.contains("TEST-NODE-7"),
        "0563 = configured value: {text}"
    );
    assert!(!text.contains("CLARUS_SERVER"), "no hardcoded 0563");
}

/// 3008 default: the (0400,0563) value resolves to CLARUS_SERVER:{port}.
#[test]
fn stow_updated_blob_embeds_default_modifying_system() {
    let srv = TestServer::start_with(|cfg| {
        cfg.port = 8024;
    });
    let sop = "1.2.826.0.1.3680043.10.1234.1321";
    let old_inst = build_instance_named(sop, "STOWPAT01", "OLD^NAME", STUDY_A, SERIES_A, 0x71);
    srv.post("/dicomweb/studies", "application/dicom", &old_inst)
        .assert_status(200);
    let new_inst = build_instance_named(sop, "STOWPAT01", "NEW^NAME", STUDY_A, SERIES_A, 0x71);
    srv.post("/dicomweb/studies", "application/dicom", &new_inst)
        .assert_status(202);
    let w = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w.assert_status(200);
    let text = String::from_utf8_lossy(&w.body);
    assert!(
        text.contains("CLARUS_SERVER:8024"),
        "default 0563 = CLARUS_SERVER:{{port}}: {text}"
    );
}

/// Scenario 15 (01.09 erase gap): admin delete removes the study manifest but
/// the append-only `sop_owner` rows stay. Re-STOWs after the wipe must rebuild
/// the study piece by piece, not 409-Conflict and not a silent idempotent
/// no-op. Simulated by dropping the manifest file directly after two STOWs
/// (the exact post-delete disk state): the first re-STOW inserts fresh (owner
/// manifest missing), the second merges into the readable rebuilt manifest
/// (stale row, SOP absent).
#[test]
fn stow_after_deleted_study_reinserts_fresh() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1401";
    let sop2 = "1.2.826.0.1.3680043.10.1234.1402";
    let inst = build_instance(sop, "STOWPAT01", STUDY_A, SERIES_A, 0x71);
    let inst2 = build_instance(sop2, "STOWPAT01", STUDY_A, SERIES_A, 0x72);

    // Two STOWs: sop_owner rows + a study manifest with both instances.
    srv.post("/dicomweb/studies", "application/dicom", &inst)
        .assert_status(200);
    srv.post("/dicomweb/studies", "application/dicom", &inst2)
        .assert_status(200);

    // Admin-delete aftermath: the study manifest file is GONE, the
    // append-only sop_owner rows survive (field fact 01.09).
    let manifest_path = clarus_core::stow::manifest_path(&srv.manifest_dir, STUDY_A);
    std::fs::remove_file(&manifest_path).expect("simulate admin delete removing the manifest");

    // Re-STOW the first SOP: owner manifest MISSING -> fresh insert, not 409.
    let re = srv.post("/dicomweb/studies", "application/dicom", &inst);
    assert!(
        re.status == 200,
        "re-STOW after manifest removal must be 200, got {}: {}",
        re.status,
        String::from_utf8_lossy(&re.body)
    );
    let v = parse_json(&re.body).expect("stow json");
    let refs = v
        .obj_get("00081199")
        .and_then(|x| x.obj_get("Value"))
        .and_then(|x| x.as_array())
        .expect("ReferencedSOPSequence");
    assert_eq!(refs.len(), 1, "the re-inserted SOP is referenced");

    // Re-STOW the second SOP: the rebuilt manifest is READABLE but does not
    // contain the SOP (stale row) -> merged in, no silent no-op.
    let re2 = srv.post("/dicomweb/studies", "application/dicom", &inst2);
    assert!(
        re2.status == 200,
        "stale-row re-STOW into a rebuilt study must be 200, got {}: {}",
        re2.status,
        String::from_utf8_lossy(&re2.body)
    );

    // Both instances are addressable again.
    for s in [&sop, &sop2] {
        let w = srv.get(&format!(
            "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{s}"
        ));
        w.assert_status(200);
    }
    let q = srv.get("/dicomweb/studies");
    q.assert_status(200);
    assert!(
        String::from_utf8_lossy(&q.body).contains(STUDY_A),
        "QIDO lists the re-inserted study"
    );
    let m =
        clarus_core::manifest::read_manifest(&manifest_path).expect("rebuilt manifest reads back");
    let n: usize = m.series.iter().map(|s| s.instances.len()).sum();
    assert_eq!(n, 2, "both re-inserted instances are in the manifest");
}

// -- Streaming + temp-file cleanup (single-part application/dicom) --------

/// Count leftover `clarus_stow_*.dcm` scratch files in the CAS `_tmp` dir.
/// A streaming STOW must leave zero of them behind on any exit path.
fn stow_scratch_files(cas_root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let tmp = cas_root.join("_tmp");
    if let Ok(entries) = std::fs::read_dir(&tmp) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("clarus_stow_") && name.ends_with(".dcm") {
                out.push(e.path());
            }
        }
    }
    out
}

/// Send raw bytes on a fresh connection and return whatever the server says
/// before closing. `declared_len` may exceed `body.len()` to simulate a client
/// that aborts mid-stream (PS3.18 streaming upload) - the server MUST NOT leave
/// a temp file behind.
fn post_then_abort(srv: &TestServer, content_type: &str, declared_len: usize, body: &[u8]) {
    let mut stream = TcpStream::connect(srv.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let head = format!(
        "POST /dicomweb/studies HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: {content_type}\r\nContent-Length: {declared_len}\r\nConnection: close\r\n\r\n",
        srv.addr.port()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(body).expect("write partial body");
    // Close WITHOUT completing the declared Content-Length: the server sees
    // `UnexpectedEof` on the body reader (framing.rs Length path).
    drop(stream);
}

/// Streaming: a single-part `application/dicom` body is written to a temp file
/// and ingested via `InstanceSource::TempFile`, exactly like a multipart part.
/// The observable contract is unchanged (201 + the frozen JSON), and no scratch
/// file survives.
#[test]
fn stow_single_part_streams_to_temp_file() {
    let srv = TestServer::start();
    let before = srv.cas_file_count();
    let r = srv.post("/dicomweb/studies", "application/dicom", &sop_a());
    r.assert_status(200);
    r.assert_content_type(STOW_JSON_CT);
    assert_eq!(
        srv.cas_file_count(),
        before + 1,
        "single-part STOW stores exactly one CAS blob"
    );
    assert!(
        stow_scratch_files(&srv.cas_root).is_empty(),
        "no clarus_stow_* scratch file survives a successful STOW"
    );
}

/// A single-part upload larger than one READ_BUF (64 KiB) still round-trips: the
/// body is chunked through the temp file, so the result is byte-identical to the
/// buffered path it replaced. Serves as the "streaming did not corrupt data"
/// check for a body spanning many read chunks.
#[test]
fn stow_single_part_multi_chunk_roundtrip() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1320";
    let mut inst = build_instance(sop, "STOWPAT01", STUDY_A, SERIES_A, 0x61);
    // ~300 KiB of pixel data => several 64 KiB reads through the stream loop.
    let big: Vec<u8> = (0..300 * 1024).map(|i| (i % 251) as u8).collect();
    append_element(&mut inst, 0x7FE0, 0x0010, b"OB", &big);

    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    r.assert_status(200);

    let w = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w.assert_status(200);
    assert_eq!(w.body, inst, "multi-chunk STOW is byte-exact");
    assert!(
        stow_scratch_files(&srv.cas_root).is_empty(),
        "no scratch file after a multi-chunk STOW"
    );
}

/// Single-part client abort (declared length never fully sent): the handler
/// removes the partial temp file before returning, so a gigabyte WSI part does
/// not leak on disk until the next restart.
#[test]
fn stow_single_part_abort_removes_temp_file() {
    let srv = TestServer::start();
    let body = sop_a();
    // Declare 64 MiB but send only the head of the body, then close.
    post_then_abort(
        &srv,
        "application/dicom",
        64 * 1024 * 1024,
        &body[..body.len() / 2],
    );

    // The handler runs on its own thread; give the socket close a moment to
    // propagate and the error branch to unlink.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if stow_scratch_files(&srv.cas_root).is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        stow_scratch_files(&srv.cas_root).is_empty(),
        "aborted single-part STOW leaves no clarus_stow_* temp file"
    );
    assert_eq!(
        srv.cas_file_count(),
        0,
        "an aborted single-part STOW commits no CAS blob"
    );
}

/// Multipart parse-error (mid-part drop): the inner drain `Err` branch removes
/// the partial temp file before the 400, same guarantee as the single path.
#[test]
fn stow_multipart_abort_removes_temp_file() {
    let srv = TestServer::start();
    let ct = multipart_ct();
    let mut head = format!(
        "--{BOUNDARY}\r\nContent-Type: application/dicom\r\nContent-Length: {}\r\n\r\n",
        64 * 1024 * 1024
    )
    .into_bytes();
    let part = sop_a();
    head.extend_from_slice(&part[..part.len() / 2]);
    post_then_abort(&srv, &ct, head.len(), &head);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if stow_scratch_files(&srv.cas_root).is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        stow_scratch_files(&srv.cas_root).is_empty(),
        "aborted multipart STOW leaves no clarus_stow_* temp file"
    );
    assert_eq!(
        srv.cas_file_count(),
        0,
        "an aborted multipart STOW commits no CAS blob"
    );
}

// -- Frame-table parity: streaming file scan == in-memory bytes scan ------

/// Build a multi-frame ENCAPSULATED instance (JPEG Baseline declared syntax),
/// with one zero-length (sparse) tile, so the frame table is non-trivial.
fn multi_frame_instance(
    sop_instance_uid: &str,
    study_uid: &str,
    series_uid: &str,
    tiles: &[&[u8]],
) -> Vec<u8> {
    multi_frame_instance_ex(sop_instance_uid, study_uid, series_uid, tiles, false)
}

/// [`multi_frame_instance`] with an optional UNDEFINED-length PixelData
/// (0xFFFFFFFF) - the RMS-class encoding (PS3.5 Table A.4-1), which the server
/// must table just like the defined-length form.
fn multi_frame_instance_ex(
    sop_instance_uid: &str,
    study_uid: &str,
    series_uid: &str,
    tiles: &[&[u8]],
    undefined_pixeldata: bool,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(4096);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.50");
    append_element(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.77.1.6",
    ); // WSI
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_instance_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"WSI^FRAMES");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"WSIPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"SM");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &256u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &256u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(
        &mut d,
        0x0028,
        0x0008,
        b"IS",
        tiles.len().to_string().as_bytes(),
    );
    // Encapsulated PixelData: empty BOT item, then one item per tile, then the
    // sequence delimiter.
    let mut pixel = Vec::new();
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    for tile in tiles {
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        pixel.extend_from_slice(&(tile.len() as u32).to_le_bytes());
        pixel.extend_from_slice(tile);
    }
    pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    if undefined_pixeldata {
        // OB + 2 reserved bytes + 0xFFFFFFFF, then the value.
        d.extend_from_slice(&0x7FE0u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"OB");
        d.extend_from_slice(&[0, 0]);
        d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        d.extend_from_slice(&pixel);
    } else {
        append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixel);
    }
    d
}

/// Task A parity: the frame table STOW writes via the STREAMING file scan
/// (single-part path, now `frame_records_from_file`) is byte-identical to the
/// table the in-memory bytes scan produces for the SAME instance. This pins
/// that the read-back removal is behaviour-preserving, and the sparse tile
/// survives as a zero-length row.
#[test]
fn stow_streaming_frame_scan_matches_in_memory_scan() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1391";
    let t1: &[u8] = &[0xAA; 300];
    let t2: &[u8] = &[]; // sparse tile
    let t3: &[u8] = &[0xCC; 512];
    let inst = multi_frame_instance(sop, STUDY_A, SERIES_A, &[t1, t2, t3]);

    // Single-part application/dicom -> the streaming file scan builds the table.
    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    assert_eq!(r.status, 200, "multi-frame STOW must succeed");

    // The manifest table must equal the in-memory scan of the same bytes.
    // (The public slice scan is the reference; the STOW path used the
    // streaming file scan, so an equal table proves the two agree.)
    let expected = clarus_adapters::dicom_rs::frame::compute_frame_chunks(&inst);
    let path = clarus_core::stow::manifest_path(&srv.manifest_dir, STUDY_A);
    let m = clarus_core::manifest::read_manifest(&path).expect("manifest reads back");
    let got = m
        .find_instance(sop)
        .expect("instance present")
        .frames
        .clone();

    assert_eq!(
        got.len(),
        expected.len(),
        "streaming scan must table the same frame count as the bytes scan"
    );
    for (g, e) in got.iter().zip(expected.iter()) {
        assert_eq!(g.number, e.number, "frame number");
        assert_eq!(g.offset, e.offset, "frame offset");
        assert_eq!(g.compressed_len, e.len, "frame length");
    }
    assert_eq!(got.len(), 3, "one row per tile");
    assert_eq!(got[1].compressed_len, 0, "sparse tile is a zero-length row");

    // And the served frame bytes are exactly the source tiles (the table is
    // trustworthy, not merely present).
    for (i, tile) in [t1, t2, t3].iter().enumerate() {
        let n = i + 1;
        let resp = srv.get(&format!(
            "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}/frames/{n}"
        ));
        assert_eq!(resp.status, 200, "frame {n}");
        assert_eq!(
            resp.multipart_part_body(),
            *tile,
            "frame {n} bytes must equal the source tile"
        );
    }
}

/// RMS-class end to end: an instance whose PixelData is UNDEFINED length
/// (0xFFFFFFFF encapsulated, PS3.5 Table A.4-1) - the encoding the 13.09 GB
/// field file uses. STOW must write a NON-EMPTY frame table (the HARD RULE),
/// and /frames/{N} must serve the exact source tiles. Before the undefined-
/// length fix this instance got an empty table, so one tile request fell back
/// to `read_to_end` of the whole instance.
#[test]
fn stow_undefined_length_pixeldata_tables_frames() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1392";
    let t1: &[u8] = &[0x11; 256];
    let t2: &[u8] = &[0x22; 128];
    let inst = multi_frame_instance_ex(
        sop,
        STUDY_A,
        SERIES_A,
        &[t1, t2],
        true, // undefined-length PixelData
    );

    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    assert_eq!(
        r.status, 200,
        "undefined-length PixelData STOW must succeed"
    );

    // HARD RULE: the table is PRESENT, not empty.
    let path = clarus_core::stow::manifest_path(&srv.manifest_dir, STUDY_A);
    let m = clarus_core::manifest::read_manifest(&path).expect("manifest reads back");
    let got = m
        .find_instance(sop)
        .expect("instance present")
        .frames
        .clone();
    assert_eq!(
        got.len(),
        2,
        "an undefined-length multi-frame WSI MUST get a frame table (empty = whole-blob read)"
    );
    assert_eq!(got[0].compressed_len, 256);
    assert_eq!(got[1].compressed_len, 128);

    // The table is trustworthy: /frames/{N} serves the exact source tiles.
    for (i, tile) in [t1, t2].iter().enumerate() {
        let n = i + 1;
        let resp = srv.get(&format!(
            "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}/frames/{n}"
        ));
        assert_eq!(resp.status, 200, "frame {n}");
        assert_eq!(
            resp.multipart_part_body(),
            *tile,
            "frame {n} bytes must equal the source tile"
        );
    }
}

/// F5 end to end: an instance that holds MORE frames than
/// `max_frames_per_instance` is REJECTED (413), leaves NO CAS blob behind, and
/// writes NO manifest - it is never stored without a table.
#[test]
fn stow_over_frame_cap_is_rejected_no_residue() {
    // A tiny cap so a small fixture trips it.
    let srv = TestServer::start_with(|c| c.max_frames_per_instance = 4);
    let sop = "1.2.826.0.1.3680043.10.1234.1393";
    // 8 items > cap 4.
    let tiles: Vec<Vec<u8>> = (0..8u8).map(|b| vec![b; 32]).collect();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let inst = multi_frame_instance(sop, STUDY_A, SERIES_A, &refs);

    let before = srv.cas_file_count();
    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    // The per-instance reject is 413; STOW-RS aggregates a rejected instance
    // into the request-level failure response (400) with a FailedSOPSequence,
    // exactly as it does for an oversize or invalid instance.
    assert!(
        r.status == 413 || r.status == 400,
        "an over-cap instance must be rejected (413/400), got {}",
        r.status
    );

    // No CAS residue: the blob renamed into CAS during ingest is unlinked.
    assert_eq!(
        srv.cas_file_count(),
        before,
        "an over-cap instance must leave no CAS blob behind"
    );

    // No manifest entry for the rejected SOP.
    let path = clarus_core::stow::manifest_path(&srv.manifest_dir, STUDY_A);
    if let Ok(m) = clarus_core::manifest::read_manifest(&path) {
        assert!(
            m.find_instance(sop).is_none(),
            "a rejected instance must not appear in the manifest"
        );
    }
}

/// F5: an instance exactly AT the cap is accepted and tabled (the bound is
/// inclusive; the reject only fires ABOVE it).
#[test]
fn stow_at_frame_cap_is_accepted() {
    let srv = TestServer::start_with(|c| c.max_frames_per_instance = 4);
    let sop = "1.2.826.0.1.3680043.10.1234.1394";
    let tiles: Vec<Vec<u8>> = (0..4u8).map(|b| vec![b; 32]).collect();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let inst = multi_frame_instance(sop, STUDY_A, SERIES_A, &refs);

    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    assert_eq!(r.status, 200, "an at-cap instance is accepted");

    let path = clarus_core::stow::manifest_path(&srv.manifest_dir, STUDY_A);
    let m = clarus_core::manifest::read_manifest(&path).expect("manifest reads back");
    let got = m
        .find_instance(sop)
        .expect("instance present")
        .frames
        .clone();
    assert_eq!(got.len(), 4, "all 4 frames tabled at the cap");
}

/// F5 must hold on the UPDATE path too.
///
/// `materialize_update` used to build the table with the UNCAPPED reader, so
/// the cap could be bypassed in two requests: store a small instance under the
/// SOP, then re-send the same SOP with more frames than the cap. The update
/// wrote the new blob and tabled every frame, and the reject never fired. The
/// cap is now checked before the blob reaches CAS, so the second request is
/// refused and the FIRST instance is the one still in the archive.
#[test]
fn re_stowing_the_same_sop_over_the_frame_cap_is_rejected() {
    let srv = TestServer::start_with(|c| c.max_frames_per_instance = 4);
    let sop = "1.2.826.0.1.3680043.10.1234.1395";

    // First: a 2-frame instance, comfortably under the cap.
    let small: Vec<Vec<u8>> = (0..2u8).map(|b| vec![b; 32]).collect();
    let small_refs: Vec<&[u8]> = small.iter().map(|t| t.as_slice()).collect();
    let first = multi_frame_instance(sop, STUDY_A, SERIES_A, &small_refs);
    srv.post("/dicomweb/studies", "application/dicom", &first)
        .assert_status(200);
    let blake3_before = manifest_blake3(&srv.manifest_dir, STUDY_A, sop);

    // Then: the SAME SOP with 8 frames (> cap 4).
    let big: Vec<Vec<u8>> = (0..8u8).map(|b| vec![b; 32]).collect();
    let big_refs: Vec<&[u8]> = big.iter().map(|t| t.as_slice()).collect();
    let second = multi_frame_instance(sop, STUDY_A, SERIES_A, &big_refs);
    let r = srv.post("/dicomweb/studies", "application/dicom", &second);
    assert!(
        r.status == 413 || r.status == 400,
        "an over-cap UPDATE must be rejected (413/400), got {}",
        r.status
    );

    // The original instance survives untouched: same blob, same 2 frames.
    let path = clarus_core::stow::manifest_path(&srv.manifest_dir, STUDY_A);
    let m = clarus_core::manifest::read_manifest(&path).expect("manifest reads back");
    let inst = m.find_instance(sop).expect("the first instance survives");
    assert_eq!(
        inst.frames.len(),
        2,
        "the rejected update must not have replaced the frame table"
    );
    assert_eq!(
        inst.blake3, blake3_before,
        "the rejected update must not have re-pointed the instance"
    );
}

// --- 0410 Q2: folded BLAKE3 over the STOW stream ------------------------

/// W4(a): a multipart part that ends in `\r\n` (Weasis framing) is stripped
/// before CAS, and the manifest digest is BLAKE3 of the FINAL (stripped) bytes
/// - proving the folded digest is reconciled after a strip truncation.
#[test]
fn blake3_fold_crlf_part_digest_matches_stripped_blob() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1401";
    let mut inst = build_instance(sop, "STOWPAT01", STUDY_A, SERIES_A, 0x41);
    // Append client-framing CRLF junk the strip must remove.
    inst.extend_from_slice(b"\r\n");

    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    r.assert_status(200);

    // The stored blob must NOT end in CRLF.
    let w = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w.assert_status(200);
    assert!(
        !w.body.ends_with(b"\r\n"),
        "the stored blob must be stripped of trailing CRLF"
    );
    // Manifest digest == BLAKE3(final stripped bytes), not the raw upload.
    assert_eq!(
        clarus_adapters::hex_encode(&manifest_blake3(&srv.manifest_dir, STUDY_A, sop)),
        clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(&w.body)),
        "manifest digest must equal BLAKE3 of the FINAL on-disk CAS bytes"
    );
}

/// W4(d): digest parity on a multi-KB part - the folded digest equals both the
/// independent `blake3_of_file` and the in-memory `cas_hash_one` of the same
/// bytes (no byte changed; the fold is transparent).
#[test]
fn blake3_fold_digest_parity_multi_kb() {
    let srv = TestServer::start();
    let sop = "1.2.826.0.1.3680043.10.1234.1402";
    // A larger pixel buffer so the part spans many read chunks.
    let mut inst = Vec::with_capacity(400_000);
    inst.extend_from_slice(&[0u8; 128]);
    inst.extend_from_slice(b"DICM");
    append_element(&mut inst, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(
        &mut inst,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.2",
    );
    append_element(&mut inst, 0x0008, 0x0018, b"UI", sop.as_bytes());
    append_element(&mut inst, 0x0010, 0x0010, b"PN", b"STOW^SMOKE");
    append_element(&mut inst, 0x0010, 0x0020, b"LO", b"STOWPAT01");
    append_element(&mut inst, 0x0020, 0x000d, b"UI", STUDY_A.as_bytes());
    append_element(&mut inst, 0x0020, 0x000e, b"UI", SERIES_A.as_bytes());
    append_element(&mut inst, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut inst, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut inst, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut inst, 0x0028, 0x0010, b"US", &64u16.to_le_bytes());
    append_element(&mut inst, 0x0028, 0x0011, b"US", &64u16.to_le_bytes());
    append_element(&mut inst, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut inst, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    let pixels: Vec<u8> = (0..64 * 64).map(|i| (i % 251) as u8).collect();
    append_element(&mut inst, 0x7FE0, 0x0010, b"OB", &pixels);

    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    r.assert_status(200);

    let w = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances/{sop}"
    ));
    w.assert_status(200);
    assert_eq!(w.body, inst, "a conformant part is stored byte-for-byte");
    assert_eq!(
        clarus_adapters::hex_encode(&manifest_blake3(&srv.manifest_dir, STUDY_A, sop)),
        clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(&inst)),
        "folded digest == cas_hash_one of the same bytes"
    );
}

// -- IOCM rate counters ---------------------------------------------------

/// Read one counter's value from the rendered `/metrics` text.
///
/// The assertion is on the operator-visible number, not on the atomic: a
/// counter that never reaches the render does not exist as far as Prometheus
/// is concerned.
fn metric_value(name: &str) -> u64 {
    let rendered = clarus_server::handlers::metrics_render::render_metrics();
    let prefix = format!("{name} ");
    rendered
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or_else(|| panic!("no sample for {name} in /metrics"))
}

// The counters are process-global, so these assertions are written as deltas.
// Each test binds its own server and runs no other STOW concurrently.

/// Serialises tests that assert on the process-global STOW counters.
///
/// The counters are process-wide and every other STOW test in this binary
/// increments them concurrently, so a value delta across two HTTP requests is
/// not attributable to either request. What IS observable per request is that
/// the counter is monotonic and that an idempotent re-STOW does not move it
/// MORE than concurrent noise would - which cannot be asserted reliably.
///
/// The exact per-request accounting is therefore proved by the unit tests on
/// `record_instances_stored` / `record_rejected_instances` (which measure the
/// increment they are given) plus the wiring tests in
/// `expose_rejected_smoke.rs` (which prove both counters reach `/metrics`).
/// This file proves the ROUTE calls the helper: a stored instance advances the
/// counter at all, and a rejection note advances the rejection counter.
static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn stow_advances_the_stored_instances_counter() {
    let _guard = COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let srv = TestServer::start();
    let body = multipart_body(BOUNDARY, &[&sop_a(), &sop_b()]);

    let before = metric_value("clarus_instances_stored_total");
    let r = srv.post("/dicomweb/studies", &multipart_ct(), &body);
    r.assert_status(200);
    let after = metric_value("clarus_instances_stored_total");
    assert!(
        after > before,
        "storing new instances must advance the denominator ({before} -> {after})"
    );
}

#[test]
fn a_rejection_note_advances_the_rejected_counter_once() {
    let _guard = COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let srv = TestServer::start();
    // SEG (66.4) is in the AI-Result set, so the Quality scope permits hiding.
    let seg_class = "1.2.840.10008.5.1.4.1.1.66.4";
    let inst_one = build_seg_instance("1.2.826.0.1.3680043.10.1234.1801");
    let inst_two = build_seg_instance("1.2.826.0.1.3680043.10.1234.1802");
    let r = srv.post(
        "/dicomweb/studies",
        &multipart_ct(),
        &multipart_body(BOUNDARY, &[&inst_one, &inst_two]),
    );
    r.assert_status(200);

    let before = metric_value("clarus_rejected_instances_total");
    let note = build_quality_rejection_note(&[
        (seg_class, "1.2.826.0.1.3680043.10.1234.1801"),
        (seg_class, "1.2.826.0.1.3680043.10.1234.1802"),
    ]);
    let r = srv.post("/dicomweb/studies", "application/dicom", &note);
    r.assert_status(200);
    let after_note = metric_value("clarus_rejected_instances_total");
    assert!(
        after_note > before,
        "a note naming stored instances must advance the counter ({before} -> {after_note})"
    );

    // The note actually sequestered the instances: they are gone from the
    // ordinary view. This is the behavioural half of the same wiring.
    let qido = srv.get(&format!(
        "/dicomweb/studies/{STUDY_A}/series/{SERIES_A}/instances"
    ));
    qido.assert_status(200);
    let body = String::from_utf8_lossy(&qido.body).to_string();
    assert!(
        !body.contains("1.2.826.0.1.3680043.10.1234.1801"),
        "a rejected instance must be hidden from an ordinary search: {body}"
    );
}

/// A single-part `application/dicom` STOW is one part, and the IOCM
/// re-acceptance rule (IOCM 4.66.4.2.3.1, reasons 113037/113038) applies to
/// it exactly as it does to a multipart part. Skipping the guard on the bytes
/// path let a client re-store an instance the standard says may never be
/// re-accepted.
#[test]
fn a_single_part_stow_cannot_re_accept_a_patient_safety_rejection() {
    // Hiding a diagnostic image is the deployment's consent decision, so the
    // reason only takes effect with the switch on. Off by default, and the
    // re-acceptance rule is unreachable without a hidden instance.
    let srv = TestServer::start_with(|c| c.anyclass_enable = true);
    let seg_class = "1.2.840.10008.5.1.4.1.1.66.4";
    let sop = "1.2.826.0.1.3680043.10.1234.1851";
    let inst = build_seg_instance(sop);
    srv.post("/dicomweb/studies", "application/dicom", &inst)
        .assert_status(200);

    let note = build_rejection_note("113037", &[(seg_class, sop)]);
    srv.post("/dicomweb/studies", "application/dicom", &note)
        .assert_status(200);

    // Re-store the same bytes through the SINGLE-part path. One part, one
    // failure: `choose_status` maps all-failed to 400 with the per-instance
    // entry in FailedSOPSequence (0008,1198).
    let r = srv.post("/dicomweb/studies", "application/dicom", &inst);
    r.assert_status(400);
    let body = String::from_utf8_lossy(&r.body).to_string();
    assert!(
        body.contains("00081198"),
        "the refusal must be reported per instance: {body}"
    );
    assert!(
        body.contains("272"),
        "PS3.18 Table I.2-2 processing failure (272) is the code: {body}"
    );
    assert!(
        body.contains("113037"),
        "the error comment must name the reason: {body}"
    );
    assert!(
        !body.contains("00081199"),
        "the refused instance must not also be reported as stored: {body}"
    );
}

/// Build a SEG-class instance: an AI-Result class the Quality scope permits.
fn build_seg_instance(sop_uid: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.66.4",
    );
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"QA^SEG");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"STOWPAT01");
    append_element(&mut d, 0x0020, 0x000D, b"UI", STUDY_A.as_bytes());
    append_element(&mut d, 0x0020, 0x000E, b"UI", SERIES_A.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"SEG");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &[0x3Cu8; 64]);
    d
}

/// Build a Key Object Selection rejection note for the Quality reason.
///
/// The nesting is PS3.3 A.35.4: (0040,A375) -> (0008,1115) -> (0008,1199)
/// -> (0008,1150)/(0008,1155).
fn build_quality_rejection_note(refs: &[(&str, &str)]) -> Vec<u8> {
    build_rejection_note("113001", refs)
}

/// Same note shape with an explicit Document Title code, so the IOCM reasons
/// that forbid re-acceptance (113037/113038) can be exercised.
fn build_rejection_note(code_value: &str, refs: &[(&str, &str)]) -> Vec<u8> {
    // (0008,1199) Referenced SOP Sequence, one item per referenced instance.
    let mut sop_items = Vec::new();
    for (class_uid, instance_uid) in refs {
        let mut item = Vec::new();
        item.extend_from_slice(&0x0008u16.to_le_bytes());
        item.extend_from_slice(&0x1150u16.to_le_bytes());
        item.extend_from_slice(b"UI");
        item.extend_from_slice(&(class_uid.len() as u16).to_le_bytes());
        item.extend_from_slice(class_uid.as_bytes());
        if !class_uid.len().is_multiple_of(2) {
            item.push(0);
        }
        item.extend_from_slice(&0x0008u16.to_le_bytes());
        item.extend_from_slice(&0x1155u16.to_le_bytes());
        item.extend_from_slice(b"UI");
        item.extend_from_slice(&(instance_uid.len() as u16).to_le_bytes());
        item.extend_from_slice(instance_uid.as_bytes());
        if !instance_uid.len().is_multiple_of(2) {
            item.push(0);
        }
        sop_items.extend_from_slice(&0xFFFEu16.to_le_bytes());
        sop_items.extend_from_slice(&0xE000u16.to_le_bytes());
        sop_items.extend_from_slice(&(item.len() as u32).to_le_bytes());
        sop_items.extend_from_slice(&item);
    }

    let mut referenced = Vec::new();
    referenced.extend_from_slice(&0x0008u16.to_le_bytes());
    referenced.extend_from_slice(&0x1199u16.to_le_bytes());
    referenced.extend_from_slice(b"SQ");
    referenced.extend_from_slice(&[0u8, 0u8]);
    referenced.extend_from_slice(&(sop_items.len() as u32).to_le_bytes());
    referenced.extend_from_slice(&sop_items);

    let mut series_item = Vec::new();
    series_item.extend_from_slice(&0xFFFEu16.to_le_bytes());
    series_item.extend_from_slice(&0xE000u16.to_le_bytes());
    series_item.extend_from_slice(&(referenced.len() as u32).to_le_bytes());
    series_item.extend_from_slice(&referenced);

    let mut evidence = Vec::new();
    evidence.extend_from_slice(&0x0008u16.to_le_bytes());
    evidence.extend_from_slice(&0x1115u16.to_le_bytes());
    evidence.extend_from_slice(b"SQ");
    evidence.extend_from_slice(&[0u8, 0u8]);
    evidence.extend_from_slice(&(series_item.len() as u32).to_le_bytes());
    evidence.extend_from_slice(&series_item);
    // (0020,000D) StudyInstanceUID: the owning study of the referenced series.
    evidence.extend_from_slice(&0x0020u16.to_le_bytes());
    evidence.extend_from_slice(&0x000Du16.to_le_bytes());
    evidence.extend_from_slice(b"UI");
    evidence.extend_from_slice(&(STUDY_A.len() as u16).to_le_bytes());
    evidence.extend_from_slice(STUDY_A.as_bytes());
    if !evidence.len().is_multiple_of(2) {
        evidence.push(0);
    }

    let mut evidence_item = Vec::new();
    evidence_item.extend_from_slice(&0xFFFEu16.to_le_bytes());
    evidence_item.extend_from_slice(&0xE000u16.to_le_bytes());
    evidence_item.extend_from_slice(&(evidence.len() as u32).to_le_bytes());
    evidence_item.extend_from_slice(&evidence);

    // (0040,A043) ConceptNameCodeSequence: the coded Document Title.
    let mut code_item = Vec::new();
    code_item.extend_from_slice(&0x0008u16.to_le_bytes());
    code_item.extend_from_slice(&0x0100u16.to_le_bytes());
    code_item.extend_from_slice(b"SH");
    code_item.extend_from_slice(&(code_value.len() as u16).to_le_bytes());
    code_item.extend_from_slice(code_value.as_bytes());
    code_item.extend_from_slice(&0x0008u16.to_le_bytes());
    code_item.extend_from_slice(&0x0102u16.to_le_bytes());
    code_item.extend_from_slice(b"SH");
    code_item.extend_from_slice(&3u16.to_le_bytes());
    code_item.extend_from_slice(b"DCM");
    if !code_item.len().is_multiple_of(2) {
        code_item.push(0x20);
    }

    let mut concept_item = Vec::new();
    concept_item.extend_from_slice(&0xFFFEu16.to_le_bytes());
    concept_item.extend_from_slice(&0xE000u16.to_le_bytes());
    concept_item.extend_from_slice(&(code_item.len() as u32).to_le_bytes());
    concept_item.extend_from_slice(&code_item);

    let mut d = Vec::with_capacity(2048);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    // Key Object Selection Document.
    append_element(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.88.59",
    );
    append_element(
        &mut d,
        0x0008,
        0x0018,
        b"UI",
        b"1.2.826.0.1.3680043.10.1234.1901",
    );
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"QA^NOTE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"STOWPAT01");
    append_element(&mut d, 0x0020, 0x000D, b"UI", STUDY_A.as_bytes());
    append_element(&mut d, 0x0020, 0x000E, b"UI", SERIES_A.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"KO");
    // (0040,A043) ConceptNameCodeSequence, explicit VR SQ.
    d.extend_from_slice(&0x0040u16.to_le_bytes());
    d.extend_from_slice(&0xA043u16.to_le_bytes());
    d.extend_from_slice(b"SQ");
    d.extend_from_slice(&[0u8, 0u8]);
    d.extend_from_slice(&(concept_item.len() as u32).to_le_bytes());
    d.extend_from_slice(&concept_item);
    // (0040,A375) CurrentRequestedProcedureEvidenceSequence.
    d.extend_from_slice(&0x0040u16.to_le_bytes());
    d.extend_from_slice(&0xA375u16.to_le_bytes());
    d.extend_from_slice(b"SQ");
    d.extend_from_slice(&[0u8, 0u8]);
    d.extend_from_slice(&(evidence_item.len() as u32).to_le_bytes());
    d.extend_from_slice(&evidence_item);
    d
}
