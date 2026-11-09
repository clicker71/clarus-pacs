// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP smoke test for QIDO-RS + health.
//!
//! Freezes the QIDO-RS wire behaviour and proves `qido_pure.rs` matches:
//! the same store, the same requests, the same status + named headers + body.
//!
//! The server is started on an EPHEMERAL port (127.0.0.1:0) over a real TCP
//! socket, so this exercises the full HTTP stack - not in-process handler
//! calls. Responses for deterministic queries are compared byte-exact against
//! golden fixtures (`tests/fixtures/qido/*`). Header ORDER is NOT compared
//! (HTTP does not guarantee it - that would be flaky).
//!
//! After the freeze, these fixtures are asserted (never regenerated). The
//! `__GENERATE__` marker only triggers a one-shot write during the initial
//! freeze so the goldens lock in the pre-extraction bytes.

#![forbid(unsafe_code)]

mod common;

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

/// Deterministic `Host` header sent by the smoke client so QIDO RetrieveURL
/// (0008,1190) is byte-stable across runs (the server port varies otherwise).
const TEST_HOST: &str = "qido.test";

// -- Fixture helpers -----------------------------------------------------

/// Assert the response body is byte-exact with a committed golden fixture in
/// `tests/fixtures/qido/<name>`. Fixtures are written once at freeze time from
/// the OLD behaviour; this never modifies them - regressions are drift errors.
fn check_fixture(name: &str, actual: &[u8]) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("qido")
        .join(name);
    if std::env::var("CLARUS_FIXTURE_REGEN").is_ok() {
        std::fs::write(&path, actual)
            .unwrap_or_else(|e| panic!("regen write {}: {e}", path.display()));
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("fixture missing: {}", path.display()));
    assert_eq!(
        String::from_utf8_lossy(actual),
        expected,
        "fixture drift in {name}"
    );
}

// -- Test server harness -------------------------------------------------

/// A running test server on an ephemeral port plus its live store handles.
struct TestServer {
    addr: SocketAddr,
    _temp: tempfile::TempDir,
}

impl TestServer {
    /// Boot the full router on an ephemeral port with a fresh temp store.
    fn start() -> Self {
        Self::start_with_limit(250)
    }

    /// Same as [`Self::start`], with an explicit `[query] default_limit`.
    fn start_with_limit(default_limit: u64) -> Self {
        let temp = tempfile::tempdir().expect("temp dir");

        // storage_dir must exist for the health check.
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
        config.query.default_limit = default_limit;
        // The disk-free preflight is irrelevant to QIDO wire freezing and it
        // reads the HOST volume that backs the tempdir (which varies by
        // machine). Zero it so a full dev disk cannot fail the seed with 507
        // Insufficient Storage.
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

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
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

    /// Seed the canonical 3-study / 4-series / 5-instance store. The previous
    /// `seed()` wrote the hierarchy directly via `SqliteBackend`. Now we STOW
    /// real DICOM through `/dicomweb/studies` so the B-tree tier (manifests)
    /// answers QIDO - the same corpus, byte-identical to the SQLite seed.
    fn seed_via_stow(&self) {
        let ct = "1.2.840.10008.5.1.4.1.1.2";
        let mr = "1.2.840.10008.5.1.4.1.1.4";

        // Study 1 - patient SMITH^JOHN/P001/19800101/M, CT `Chest CT`.
        {
            let d = build_instance(
                "SMITH^JOHN",
                "P001",
                "19800101",
                "M",
                "1.1.1.1.1.1.1.1.100",
                "20210101",
                "120000",
                "ACC-100",
                "CT",
                "DOE^JANE",
                "Chest CT",
                "S100",
                "2.1.1.1.1.1.1.1.100",
                "1",
                "Axial",
                "20210101",
                "120000",
                "CT1",
                "CHEST",
                "ACME",
                "CITY HOSPITAL",
                "RADIOLOGY",
                "OPS^ANNA",
                "CHEST_PROTO",
                "REQ^PAUL",
                "CT CHEST WO",
                "3.1.1.1.1.1.1.1.100.1",
                ct,
                "1",
            );
            let r = self.post_single("/dicomweb/studies", &d);
            r.assert_status(200);
            let d = build_instance(
                "SMITH^JOHN",
                "P001",
                "19800101",
                "M",
                "1.1.1.1.1.1.1.1.100",
                "20210101",
                "120000",
                "ACC-100",
                "CT",
                "DOE^JANE",
                "Chest CT",
                "S100",
                "2.1.1.1.1.1.1.1.100",
                "1",
                "Axial",
                "20210101",
                "120000",
                "CT1",
                "CHEST",
                "ACME",
                "CITY HOSPITAL",
                "RADIOLOGY",
                "OPS^ANNA",
                "CHEST_PROTO",
                "REQ^PAUL",
                "CT CHEST WO",
                "3.1.1.1.1.1.1.1.100.2",
                ct,
                "2",
            );
            let r = self.post_single("/dicomweb/studies", &d);
            r.assert_status(200);
            let d = build_instance(
                "SMITH^JOHN",
                "P001",
                "19800101",
                "M",
                "1.1.1.1.1.1.1.1.100",
                "20210101",
                "120000",
                "ACC-100",
                "CT",
                "DOE^JANE",
                "Chest CT",
                "S100",
                "2.1.2.1.1.1.1.1.100",
                "2",
                "Coronal",
                "20210101",
                "120001",
                "CT1",
                "CHEST",
                "ACME",
                "CITY HOSPITAL",
                "RADIOLOGY",
                "OPS^ANNA",
                "CHEST_PROTO",
                "REQ^PAUL",
                "CT CHEST WO",
                "3.1.2.1.1.1.1.1.100.1",
                ct,
                "1",
            );
            let r = self.post_single("/dicomweb/studies", &d);
            r.assert_status(200);
        }

        // Study 2 - patient BROWN^EMMA/P002/19900101/F, MR `Brain MR`.
        {
            let d = build_instance(
                "BROWN^EMMA",
                "P002",
                "19900101",
                "F",
                "1.1.2.1.1.1.1.1.200",
                "20220202",
                "090000",
                "ACC-200",
                "MR",
                "WHITE^PAUL",
                "Brain MR",
                "S200",
                "2.2.1.1.1.1.1.1.200",
                "1",
                "T1",
                "20220202",
                "090000",
                "MR1",
                "BRAIN",
                "SIEMENS",
                "CITY HOSPITAL",
                "NEURO",
                "OPS^BOB",
                "BRAIN_PROTO",
                "REQ^PAUL",
                "MR BRAIN WO",
                "3.2.1.1.1.1.1.1.200.1",
                mr,
                "1",
            );
            let r = self.post_single("/dicomweb/studies", &d);
            r.assert_status(200);
        }

        // Study 3 - patient DAVIS^LIAM/P003/19750101/O, CT `Abdomen CT`.
        {
            let d = build_instance(
                "DAVIS^LIAM",
                "P003",
                "19750101",
                "O",
                "1.1.3.1.1.1.1.1.300",
                "20230303",
                "140000",
                "ACC-300",
                "CT",
                "GREEN^NORA",
                "Abdomen CT",
                "S300",
                "2.3.1.1.1.1.1.1.300",
                "1",
                "Axial",
                "20230303",
                "140000",
                "CT1",
                "ABDOMEN",
                "ACME",
                "CITY HOSPITAL",
                "RADIOLOGY",
                "OPS^ANNA",
                "ABDO_PROTO",
                "REQ^MARY",
                "CT ABDOMEN WO",
                "3.3.1.1.1.1.1.1.300.1",
                ct,
                "1",
            );
            let r = self.post_single("/dicomweb/studies", &d);
            r.assert_status(200);
        }
    }

    /// POST a raw `application/dicom` body to `/dicomweb/studies` (STOW). The
    /// bytes-path STOW writes both the SQLite hierarchy AND the B-tree CBOR study
    /// manifest, so after seeding the B-tree tier answers QIDO.
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

    /// GET a path, returning (status, lowercased-header map, body bytes).
    fn get(&self, path: &str, accept: Option<&str>) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: {}\r\n", TEST_HOST);
        if let Some(a) = accept {
            req.push_str(&format!("Accept: {a}\r\n"));
        }
        req.push_str("Connection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("write req");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        parse_http(&buf)
    }

    /// HEAD a path (curl -I semantics): the GET head, no body.
    fn head(&self, path: &str, accept: Option<&str>) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut req = format!("HEAD {path} HTTP/1.1\r\nHost: {}\r\n", TEST_HOST);
        if let Some(a) = accept {
            req.push_str(&format!("Accept: {a}\r\n"));
        }
        req.push_str("Connection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("write req");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        parse_http(&buf)
    }
}

/// Parse the raw response bytes into a `Response`. Handles both
/// Content-Length and chunked bodies (axum streams), and tolerates an empty
/// body (e.g. a bare STOW response).
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

/// A parsed HTTP response.
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

// -- DICOM builder (Explicit VR LE, uncompressed) ------------------------

/// Append a length-prefixed DICOM element with even-length padding. Text VRs
/// pad with 0x20 (space), others with 0x00; UI is padded to even length.
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

/// Build a full Explicit VR LE DICOM instance carrying the complete identity
/// tag set (128 zero bytes + `DICM` + TransferSyntaxUID + all identity tags).
#[allow(clippy::too_many_arguments)]
fn build_instance(
    patient_name: &str,
    patient_id: &str,
    birth_date: &str,
    sex: &str,
    study_uid: &str,
    study_date: &str,
    study_time: &str,
    accession: &str,
    modality: &str,
    referring: &str,
    study_desc: &str,
    study_id: &str,
    series_uid: &str,
    series_number: &str,
    series_desc: &str,
    series_date: &str,
    series_time: &str,
    station: &str,
    body_part: &str,
    manufacturer: &str,
    institution: &str,
    inst_department: &str,
    operators: &str,
    protocol: &str,
    requesting: &str,
    req_desc: &str,
    sop_uid: &str,
    sop_class_uid: &str,
    instance_number: &str,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1"); // Explicit VR LE
    append_element(&mut d, 0x0008, 0x0016, b"UI", sop_class_uid.as_bytes()); // SOP Class
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes()); // SOP Instance
    append_element(&mut d, 0x0008, 0x0020, b"DA", study_date.as_bytes());
    append_element(&mut d, 0x0008, 0x0030, b"TM", study_time.as_bytes());
    append_element(&mut d, 0x0008, 0x0050, b"SH", accession.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", modality.as_bytes());
    append_element(&mut d, 0x0008, 0x0090, b"PN", referring.as_bytes());
    append_element(&mut d, 0x0008, 0x1030, b"LO", study_desc.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", patient_name.as_bytes());
    append_element(&mut d, 0x0010, 0x0020, b"LO", patient_id.as_bytes());
    append_element(&mut d, 0x0010, 0x0030, b"DA", birth_date.as_bytes());
    append_element(&mut d, 0x0010, 0x0040, b"CS", sex.as_bytes());
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x0010, b"SH", study_id.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x0011, b"IS", series_number.as_bytes());
    append_element(&mut d, 0x0020, 0x0013, b"IS", instance_number.as_bytes());
    append_element(&mut d, 0x0008, 0x0021, b"DA", series_date.as_bytes());
    append_element(&mut d, 0x0008, 0x0031, b"TM", series_time.as_bytes());
    append_element(&mut d, 0x0008, 0x1010, b"SH", station.as_bytes());
    append_element(&mut d, 0x0008, 0x103e, b"LO", series_desc.as_bytes());
    append_element(&mut d, 0x0018, 0x0015, b"CS", body_part.as_bytes());
    append_element(&mut d, 0x0008, 0x0070, b"LO", manufacturer.as_bytes());
    append_element(&mut d, 0x0008, 0x0080, b"LO", institution.as_bytes());
    append_element(&mut d, 0x0008, 0x1040, b"LO", inst_department.as_bytes());
    append_element(&mut d, 0x0008, 0x1070, b"PN", operators.as_bytes());
    append_element(&mut d, 0x0018, 0x1030, b"LO", protocol.as_bytes());
    append_element(&mut d, 0x0032, 0x1032, b"PN", requesting.as_bytes());
    append_element(&mut d, 0x0032, 0x1060, b"LO", req_desc.as_bytes());
    d
}

// -- Smoke tests ---------------------------------------------------------

const JSON_CT: &str = "application/dicom+json; charset=utf-8";

/// 12.09: scoped instance enumeration is NOT capped by `[query] default_limit`
/// (the MicroDicom 250-cap root cause: the viewer enumerates a series'
/// instances for download and took the search cap as the full list), while
/// open-ended searches keep the cap.
#[test]
fn smoke_s6_scoped_instances_uncapped_by_default_limit() {
    let server = TestServer::start_with_limit(1);
    server.seed_via_stow();

    // Search protection intact: the corpus shrinks to 1 study.
    let r = server.get("/dicomweb/studies", None);
    r.assert_status(200);
    let arr = parse_json(&r.body).expect("studies json");
    assert_eq!(arr.as_array().expect("array").len(), 1);

    // Study-scoped series enumeration: all 2, despite default_limit = 1.
    let r = server.get("/dicomweb/studies/1.1.1.1.1.1.1.1.100/series", None);
    r.assert_status(200);
    let arr = parse_json(&r.body).expect("series json");
    assert_eq!(arr.as_array().expect("array").len(), 2);

    // Series-scoped instances: all 2, despite default_limit = 1.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series/2.1.1.1.1.1.1.1.100/instances",
        None,
    );
    r.assert_status(200);
    let arr = parse_json(&r.body).expect("instances json");
    assert_eq!(arr.as_array().expect("array").len(), 2);

    // Study-scoped instances: all 3, despite default_limit = 1.
    let r = server.get("/dicomweb/studies/1.1.1.1.1.1.1.1.100/instances", None);
    r.assert_status(200);
    let arr = parse_json(&r.body).expect("study instances json");
    assert_eq!(arr.as_array().expect("array").len(), 3);

    // Patients list follows the SAME knob (12.09): absent limit -> capped at
    // default_limit = 1.
    let r = server.get("/dicomweb/patients", None);
    r.assert_status(200);
    let arr = parse_json(&r.body).expect("patients json");
    assert_eq!(arr.as_array().expect("array").len(), 1);

    // Explicit client limit is honored without clamping: the corpus has at
    // least 2 patients.
    let r = server.get("/dicomweb/patients?limit=250", None);
    r.assert_status(200);
    let arr = parse_json(&r.body).expect("patients json");
    assert!(arr.as_array().expect("array").len() >= 2);

    // Explicit client limit on scoped enumeration is still honored.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series/2.1.1.1.1.1.1.1.100/instances?limit=1",
        None,
    );
    r.assert_status(200);
    let arr = parse_json(&r.body).expect("limited instances json");
    assert_eq!(arr.as_array().expect("array").len(), 1);
}

/// m18 (PS3.18 8.3.4.4.1): the patients page must report the matches it did
/// NOT return.
///
/// `remaining = matches - offset - returned`, and `matches` is what the query
/// matched BEFORE the limit. The count was taken after `truncate`, so
/// `matches == returned` always held, `remaining` was always 0, and the
/// Warning header that tells a client to ask for the next page was never
/// emitted - a silently short answer that looks complete.
#[test]
fn a_limited_patients_page_reports_the_matches_it_withheld() {
    let server = TestServer::start();
    server.seed_via_stow();

    // The corpus holds at least two patients, so a page of one withholds at
    // least one match.
    let r = server.get("/dicomweb/patients?limit=1", None);
    r.assert_status(200);
    let arr = parse_json(&r.body).expect("patients json");
    assert_eq!(
        arr.as_array().expect("array").len(),
        1,
        "the page honours the limit"
    );
    let warning = r
        .header("warning")
        .expect("a truncated page must carry the 299 Warning");
    assert!(
        warning.starts_with("299 "),
        "8.3.4.4.1 Warning is code 299: {warning}"
    );
    assert!(
        warning.contains("additional results that can be requested"),
        "the fixed 8.3.4.4.1 wording: {warning}"
    );
    // And it names how many were withheld, not zero.
    assert!(
        !warning.contains("There are 0 additional"),
        "a withheld match must not be reported as zero: {warning}"
    );
}

/// PS3.18 8.3.4.4.1: `offset` skips matches, so a paged walk visits every
/// patient exactly once. Without it every page answered with the first
/// `limit` rows and a client looping on `remaining` never advanced.
#[test]
fn a_patients_offset_skips_the_rows_before_it() {
    let server = TestServer::start();
    server.seed_via_stow();

    let all = server.get("/dicomweb/patients?limit=250", None);
    all.assert_status(200);
    let all = parse_json(&all.body).expect("patients json");
    let all = all.as_array().expect("array").clone();
    assert!(
        all.len() >= 2,
        "corpus needs two patients for a meaningful offset: {}",
        all.len()
    );

    let second = server.get("/dicomweb/patients?offset=1&limit=1", None);
    second.assert_status(200);
    let second = parse_json(&second.body).expect("patients json");
    let second = second.as_array().expect("array");
    assert_eq!(second.len(), 1, "the page honours the limit");

    // `patient_id` is a per-page rank, not a stored identity, so compare the
    // stable DICOM identity (0010,0020).
    let pid = |row: &clarus_server::handlers::json_common::JsonVal| -> String {
        row.obj_get("00100020")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let all_ids = all.iter().map(&pid).collect::<Vec<_>>();
    let page_ids = second.iter().map(pid).collect::<Vec<_>>();
    assert_eq!(
        page_ids,
        vec![all_ids[1].clone()],
        "offset=1 must return the SECOND patient, not the first"
    );
}

/// A huge `?limit=` is a VALID uint (PS3.18 8.3.4-1), so it is not a 400 - but
/// the response buffer must not be sized from it. Sizing the pre-allocation by
/// the client's number made `limit=10^18` ask for ~5e20 bytes (abort) and
/// `u64::MAX` overflow the capacity arithmetic (panic): one request took the
/// whole server down instead of returning the row it holds.
#[test]
fn a_huge_limit_is_answered_instead_of_aborting_the_server() {
    let server = TestServer::start();
    server.seed_via_stow();

    for limit in ["1000000000000000000", "18446744073709551615"] {
        let path = format!("/dicomweb/studies?limit={limit}");
        let r = server.get(&path, Some(JSON_CT));
        r.assert_status(200);
        // The seeded study is still returned: the fix changes the buffer's
        // size, not the answer.
        let body = String::from_utf8_lossy(&r.body).to_string();
        assert!(
            body.starts_with('[') && body.len() > 2,
            "limit={limit} must still return the matching study: {body}"
        );
    }
}

#[test]
fn head_answers_like_get_with_empty_body_and_easter_eggs() {
    let server = TestServer::start();
    server.seed_via_stow();

    let get = server.get("/dicomweb/studies?limit=2", Some(JSON_CT));
    get.assert_status(200);

    let head = server.head("/dicomweb/studies?limit=2", Some(JSON_CT));
    head.assert_status(200);
    // The SQL-era curl -I contract: the easter eggs ride on HEAD too.
    assert_eq!(head.header("x-pragma"), Some("once"));
    assert_eq!(head.header("x-clarus"), Some("definitum semel"));
    assert_eq!(head.header("warning"), get.header("warning"));
    assert!(head.body.is_empty(), "HEAD must send no body");
    assert_eq!(
        head.header("content-length"),
        get.header("content-length"),
        "HEAD advertises the GET body size"
    );

    // SQL era: the eggs were on EVERY response, /health included.
    let health = server.head("/health", None);
    health.assert_status(200);
    assert_eq!(health.header("x-pragma"), Some("once"));
    assert_eq!(health.header("x-clarus"), Some("definitum semel"));
    assert!(health.body.is_empty(), "HEAD /health sends no body");
}

#[test]
fn easter_eggs_ride_on_every_response_class() {
    // Regression pin for the SQL-era contract that the no-axum rewrite
    // silently degraded: the easter eggs are stamped on EVERY response,
    // not just on QIDO 200s. Each class below uses a different response
    // writer, so losing one of them fails this test.
    let server = TestServer::start();
    server.seed_via_stow();

    // Viewer root (the original `curl -I /` poster child).
    let r = server.get("/", None);
    r.assert_status(200);
    assert_eq!(r.header("x-pragma"), Some("once"));
    assert_eq!(r.header("x-clarus"), Some("definitum semel"));

    // Capabilities (write_resp path).
    let r = server.get("/dicomweb/", None);
    r.assert_status(200);
    assert_eq!(r.header("x-pragma"), Some("once"));
    assert_eq!(r.header("x-clarus"), Some("definitum semel"));

    // Health (write_resp, plain text).
    let r = server.get("/health", None);
    r.assert_status(200);
    assert_eq!(r.header("x-pragma"), Some("once"));
    assert_eq!(r.header("x-clarus"), Some("definitum semel"));

    // Error path (send_error -> send_body).
    let r = server.get("/no/such/route", None);
    r.assert_status(404);
    assert_eq!(r.header("x-pragma"), Some("once"));
    assert_eq!(r.header("x-clarus"), Some("definitum semel"));

    // WADO single instance (sendfile head path).
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series/2.1.1.1.1.1.1.1.100/instances/3.1.1.1.1.1.1.1.100.1",
        None,
    );
    r.assert_status(200);
    assert_eq!(r.header("x-pragma"), Some("once"));
    assert_eq!(r.header("x-clarus"), Some("definitum semel"));

    // Chunked multipart series (chunked_start_with path).
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series/2.1.1.1.1.1.1.1.100",
        Some("multipart/related; type=application/dicom"),
    );
    r.assert_status(200);
    assert_eq!(r.header("x-pragma"), Some("once"));
    assert_eq!(r.header("x-clarus"), Some("definitum semel"));
}

#[test]
fn smoke_qido_health_and_search() {
    let server = TestServer::start();
    server.seed_via_stow();

    // 1. GET /health -> 200 "OK".
    let r = server.get("/health", None);
    r.assert_status(200);
    assert_eq!(String::from_utf8_lossy(&r.body), "OK");

    // 9. GET /dicomweb/studies/UNKNOWN/series -> 200 [].
    let r = server.get("/dicomweb/studies/1.2.3.4.5.6.7.8.9/series", None);
    r.assert_status(200);
    assert_eq!(String::from_utf8_lossy(&r.body), "[]");

    // 2. GET /dicomweb/studies?limit=2 -> 200 + Warning 299 + byte-exact body.
    // The corpus holds three studies, so a page of two leaves one match: the
    // Warning carries the fixed PS3.18 8.3.4.4.1 wording and names the request
    // Host (`TEST_HOST`), not a hardcoded `localhost`.
    let r = server.get("/dicomweb/studies?limit=2", None);
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    assert_eq!(r.header("x-pragma"), Some("once"));
    assert_eq!(r.header("x-clarus"), Some("definitum semel"));
    assert_eq!(
        r.header("warning"),
        Some("299 qido.test: There are 1 additional results that can be requested")
    );
    check_fixture("studies_limit2.json", &r.body);

    // 3. Repeated includefield values merged with commas.
    let r = server.get(
        "/dicomweb/studies?includefield=00100010&includefield=00080020&PatientName=*",
        None,
    );
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    check_fixture("studies_includefield.json", &r.body);

    // 4. GET /dicomweb/studies/{S1}/series -> 200, 2 series, byte-exact.
    let r = server.get("/dicomweb/studies/1.1.1.1.1.1.1.1.100/series", None);
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    check_fixture("series_by_study.json", &r.body);
    let arr = parse_json(&r.body).expect("series json");
    assert_eq!(arr.as_array().expect("array").len(), 2);

    // 5. GET /dicomweb/studies/{S1}/series/{Ser1}/instances -> 200, byte-exact.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series/2.1.1.1.1.1.1.1.100/instances",
        None,
    );
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    check_fixture("instances_by_series.json", &r.body);
    let arr = parse_json(&r.body).expect("instances json");
    assert_eq!(arr.as_array().expect("array").len(), 2);

    // 6. GET /dicomweb/studies/{S1}/instances -> 200, byte-exact.
    let r = server.get("/dicomweb/studies/1.1.1.1.1.1.1.1.100/instances", None);
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    check_fixture("study_instances.json", &r.body);

    // 7. GET /dicomweb/patients?limit=250 -> 200.
    let r = server.get("/dicomweb/patients?limit=250", None);
    r.assert_status(200);
    r.assert_content_type(JSON_CT);

    // 8. GET /dicomweb/studies with Accept: application/dicom+xml and
    // includefield -> 200, XML content type, and the three mandatory (R)
    // study attributes of PS3.18 Table 10.6.3-3 (2026c). The smoke corpus
    // studies all carry a Modality (CT/MR), so 00080061 (ModalitiesInStudy)
    // is guaranteed non-empty here; 00201206/00201208 are derived counts and
    // are always emitted.
    let r = server.get(
        "/dicomweb/studies?PatientName=*&includefield=00081030",
        Some("application/dicom+xml"),
    );
    r.assert_status(200);
    r.assert_content_type("application/dicom+xml");
    let xml = String::from_utf8_lossy(&r.body);
    assert!(
        xml.contains("<DicomAttribute tag=\"00201206\""),
        "NumberOfStudyRelatedSeries must be emitted"
    );
    assert!(
        xml.contains("<DicomAttribute tag=\"00201208\""),
        "NumberOfStudyRelatedInstances must be emitted"
    );
    assert!(
        xml.contains("<DicomAttribute tag=\"00080061\""),
        "ModalitiesInStudy must be emitted (corpus has CT/MR)"
    );

    // 9. Instance-level keys of Table 10.6.1-5: SOPClassUID and
    // InstanceNumber must answer 200 on /dicomweb/instances (both were 400
    // pre-fix). Corpus facts: 5 instances total; 4 carry SOPClassUID
    // 1.2.840.10008.5.1.4.1.1.2 (CT); exactly one carries InstanceNumber 2
    // ("3.1.1.1.1.1.1.1.100.2").
    let r = server.get(
        "/dicomweb/instances?00080016=1.2.840.10008.5.1.4.1.1.2",
        None,
    );
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    let arr = parse_json(&r.body).expect("instances json");
    assert_eq!(arr.as_array().expect("array").len(), 4);

    let r = server.get("/dicomweb/instances?00200013=2", None);
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    let arr = parse_json(&r.body).expect("instances json");
    let arr = arr.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    let got = String::from_utf8_lossy(&r.body);
    assert!(
        got.contains("3.1.1.1.1.1.1.1.100.2"),
        "InstanceNumber=2 must match the one corpus instance"
    );

    // 10. Regression net: the exact OHIF query shape over the wire
    // (PatientName=* + three includefield tags, one of them non-mandatory).
    // Key-set asserted on BOTH formats - no byte golden (a regression net,
    // not a snapshot).
    let r = server.get(
        "/dicomweb/studies?PatientName=*&includefield=00081030,00080060,00080090",
        None,
    );
    r.assert_status(200);
    let keys = common::json_key_set(&r.body);
    assert!(
        keys.contains(&0x0008_1030),
        "JSON: OHIF requested tag 00081030 must be emitted"
    );
    assert!(
        keys.contains(&0x0020_000D),
        "JSON: OHIF mandatory tag 0020000D must survive"
    );

    let r = server.get(
        "/dicomweb/studies?PatientName=*&includefield=00081030,00080060,00080090",
        Some("application/dicom+xml"),
    );
    r.assert_status(200);
    let tags = common::xml_tag_set(&r.body);
    assert!(
        tags.contains(&0x0008_1030),
        "XML: OHIF requested tag 00081030 must be emitted"
    );
    assert!(
        tags.contains(&0x0020_000D),
        "XML: OHIF mandatory tag 0020000D must survive"
    );
}

// -- Validation error bodies (golden, part E) -----------------------------

#[test]
fn smoke_qido_validation_errors() {
    let server = TestServer::start();
    server.seed_via_stow();

    // StudyDate invalid (matches `^\d{8}(-\d{8})?$`). Validation errors are
    // rendered as `application/json`, not the DICOMweb JSON content type.
    let r = server.get("/dicomweb/studies?StudyDate=2019", None);
    r.assert_status(400);
    r.assert_content_type("application/json");
    check_fixture("error_study_date.json", &r.body);

    // PatientName too long (65 ASCII chars).
    let r = server.get("/dicomweb/studies?PatientName=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", None);
    r.assert_status(400);
    check_fixture("error_patientname_too_long.json", &r.body);

    // ReferringPhysicianName too long.
    let r = server.get("/dicomweb/studies?ReferringPhysicianName=BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB", None);
    r.assert_status(400);
    check_fixture("error_referring_too_long.json", &r.body);

    // PatientBirthDate invalid.
    let r = server.get("/dicomweb/studies?PatientBirthDate=2019", None);
    r.assert_status(400);
    check_fixture("error_birth_date.json", &r.body);

    // PatientSex invalid.
    let r = server.get("/dicomweb/studies?PatientSex=X", None);
    r.assert_status(400);
    check_fixture("error_sex.json", &r.body);

    // StudyTime invalid.
    let r = server.get("/dicomweb/studies?StudyTime=123", None);
    r.assert_status(400);
    check_fixture("error_study_time.json", &r.body);

    // Invalid DICOM UID.
    let r = server.get("/dicomweb/studies?StudyInstanceUID=abc!", None);
    r.assert_status(400);
    check_fixture("error_uid.json", &r.body);
}

// -- PS3.18 8.3.4.4: pagination (limit/offset) conformance ----------------

/// PS3.18 8.3.4 Table 8.3.4-1 declares `limit` and `offset` as `uint`. A
/// present value that is not an unsigned integer - negative or non-numeric -
/// is an INVALID query parameter and the answer is 400 (PS3.18 8.5), not a
/// silent fallback to "no limit". Before the fix each of these returned 200
/// with the full set.
#[test]
fn smoke_qido_limit_offset_must_be_uint() {
    let server = TestServer::start();
    server.seed_via_stow();

    for (path, needle) in [
        (
            "/dicomweb/studies?limit=-1",
            r#"limit must be a non-negative integer (got \"-1\")"#,
        ),
        (
            "/dicomweb/studies?offset=-1",
            r#"offset must be a non-negative integer (got \"-1\")"#,
        ),
        (
            "/dicomweb/studies?limit=abc",
            r#"limit must be a non-negative integer (got \"abc\")"#,
        ),
        (
            "/dicomweb/studies?offset=abc",
            r#"offset must be a non-negative integer (got \"abc\")"#,
        ),
        // A fractional value is not a uint either.
        (
            "/dicomweb/studies?limit=1.5",
            r#"limit must be a non-negative integer (got \"1.5\")"#,
        ),
    ] {
        let r = server.get(path, None);
        r.assert_status(400);
        r.assert_content_type("application/json");
        let body = String::from_utf8_lossy(&r.body);
        assert!(body.contains(needle), "{path}: {body}");
    }
}

/// PS3.18 8.3.4.4.1: `results = min(max(0, matches - offset), maxResults,
/// limit)`. An explicit `limit=0` (a valid uint) forces `results = 0`, so the
/// answer is 200 with the empty JSON array `[]` - NOT the full set. A previous
/// implementation treated 0 as "unlimited" and returned every study.
///
/// `remaining` is "the number of matches that were not yet returned", so with
/// three matches, zero returned and no offset it is 3 - and the Warning rule
/// (`remaining > 0`) still applies to an empty page.
#[test]
fn smoke_qido_limit_zero_is_empty_array() {
    let server = TestServer::start();
    server.seed_via_stow();

    let r = server.get("/dicomweb/studies?limit=0", None);
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    assert_eq!(String::from_utf8_lossy(&r.body), "[]");
    assert_eq!(
        r.header("warning"),
        Some("299 qido.test: There are 3 additional results that can be requested")
    );
}

/// PS3.18 8.3.4.4.1: an `offset` at or past the match count yields
/// `results = 0` -> 200 with an empty array (and no Warning: `remaining` is 0).
#[test]
fn smoke_qido_offset_past_end_is_empty_array() {
    let server = TestServer::start();
    server.seed_via_stow();

    let r = server.get("/dicomweb/studies?offset=999", None);
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    assert_eq!(String::from_utf8_lossy(&r.body), "[]");
    assert_eq!(r.header("warning"), None);
}

/// PS3.18 8.3.4.4.1: when `remaining > 0` the response SHALL carry the fixed
/// `Warning: 299 <service>: There are <remaining> additional results that can
/// be requested`, where `<service>` is the host:port the request was sent to.
/// The corpus has three studies, so `limit=1` leaves two, and the Warning must
/// name this request's Host (`TEST_HOST`) with the exact count.
#[test]
fn smoke_qido_limit_one_warns_with_remaining_count() {
    let server = TestServer::start();
    server.seed_via_stow();

    let r = server.get("/dicomweb/studies?limit=1", None);
    r.assert_status(200);
    r.assert_content_type(JSON_CT);
    let arr = parse_json(&r.body).expect("studies json");
    assert_eq!(arr.as_array().expect("array").len(), 1, "one row per page");
    assert_eq!(
        r.header("warning"),
        Some("299 qido.test: There are 2 additional results that can be requested")
    );

    // The last page has nothing beyond it: no Warning.
    let r = server.get("/dicomweb/studies?limit=3", None);
    r.assert_status(200);
    assert_eq!(r.header("warning"), None);
}

// -- Unknown search keys are ignored, not a 400 ---------------------------
/// Unknown-only query parameter -> 200 (all studies), not 400. DICOM lets
/// clients over-specify; a key we cannot express is ignored (and logged).
#[test]
fn smoke_qido_unknown_key_ignored() {
    let server = TestServer::start();
    server.seed_via_stow();
    let r = server.get("/dicomweb/studies?Foo=bar", None);
    r.assert_status(200);
}

/// A KNOWN key alongside an unknown one still filters (the unknown is ignored
/// and the known one drives the match) -> 200, not 400.
#[test]
fn smoke_qido_known_key_with_unknown_sibling() {
    let server = TestServer::start();
    server.seed_via_stow();
    let r = server.get("/dicomweb/studies?Modality=CT&Foo=bar", None);
    r.assert_status(200);
}

// -- Stage A (08.09): StationName / BodyPartExamined series keys ---------

/// StationName (0008,1010) and BodyPartExamined (0018,0015) are series-level
/// matching keys (vendor extensions, dcm4chee/Orthanc parity). The corpus:
/// study 1 has two CT series (station CT1, body CHEST), study 2 one MR
/// series (station MR1, body BRAIN), study 3 one CT series (CT1/ABDOMEN).
#[test]
fn smoke_station_name_body_part_series_keys() {
    let server = TestServer::start();
    server.seed_via_stow();

    // Study-scoped series: StationName=CT1 -> the study's two CT series.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?StationName=CT1",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);

    // StationName wildcard over the same study: no MR series there.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?StationName=MR*",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 0);

    // The MR study's series matches MR1.
    let r = server.get(
        "/dicomweb/studies/1.1.2.1.1.1.1.1.200/series?StationName=MR1",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 1);

    // BodyPartExamined with a single-char wildcard: BRAI? -> the MR series.
    let r = server.get(
        "/dicomweb/studies/1.1.2.1.1.1.1.1.200/series?BodyPartExamined=BRAI?",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 1);

    // Global all-series: BodyPartExamined=ABDOMEN -> exactly study 3's series.
    let r = server.get("/dicomweb/series?BodyPartExamined=ABDOMEN", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 1);

    // Hex-alias form is accepted like every other key.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?00081010=CT1",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);

    // Oversize SH value: NOT a validation error (the SeriesDescription
    // template carries no length cap and no new error path is invented) -
    // accepted, matches nothing in the corpus.
    let long = "X".repeat(300);
    let r = server.get(
        &format!("/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?StationName={long}"),
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 0);

    // Wrong-level discipline mirrors the existing template key: a series key
    // on the study resource is a 400, exactly like SeriesDescription.
    let r = server.get("/dicomweb/studies?StationName=CT1", None);
    r.assert_status(400);
    let r = server.get("/dicomweb/studies?BodyPartExamined=CHEST", None);
    r.assert_status(400);
    let r = server.get("/dicomweb/studies?SeriesDescription=Axial", None);
    r.assert_status(400);

    // Study's-Instances: the two keys are Series IE -> accepted-and-ignored
    // (200 with the study's instances), like SeriesDescription today.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/instances?StationName=CT1",
        None,
    );
    r.assert_status(200);
    assert!(json_len(&r.body) >= 1);
}

// -- Stage A (08.09): PatientBirthDate range at study + patients levels ---

/// PatientBirthDate accepts `YYYYMMDD` and `YYYYMMDD-YYYYMMDD` (and `*`),
/// validated by the StudyDate machinery, and matches the stored UInt with
/// inclusive bounds at the study level.
#[test]
fn smoke_birth_date_range_study_level() {
    let server = TestServer::start();
    server.seed_via_stow();

    // Exact form: only study 1 (SMITH, 19800101).
    let r = server.get("/dicomweb/studies?PatientBirthDate=19800101", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 1);
    let body = String::from_utf8_lossy(&r.body).into_owned();
    assert!(body.contains("P001"));

    // Range with inclusive bounds: 1970-1985 covers 1980 (study 1) and
    // 1975 (study 3) but not 1990 (study 2).
    let r = server.get("/dicomweb/studies?PatientBirthDate=19700101-19851231", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);

    // Bounds themselves are inclusive.
    let r = server.get("/dicomweb/studies?PatientBirthDate=19750101-19800101", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);

    // Wildcard form matches all three studies.
    let r = server.get("/dicomweb/studies?PatientBirthDate=*", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 3);

    // Malformed forms are 400s with the StudyDate-style message.
    let r = server.get("/dicomweb/studies?PatientBirthDate=1980", None);
    r.assert_status(400);
    let r = server.get("/dicomweb/studies?PatientBirthDate=19800101-1990", None);
    r.assert_status(400);
    let r = server.get(
        "/dicomweb/studies?PatientBirthDate=19800101-19901231-2020",
        None,
    );
    r.assert_status(400);

    // Cross-level discipline unchanged: birthdate stays accepted-and-ignored
    // on the global series resource (a Patient IE key).
    let all = server.get("/dicomweb/series", None);
    all.assert_status(200);
    let n_all = json_len(&all.body);
    let r = server.get("/dicomweb/series?PatientBirthDate=19700101-19851231", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), n_all);
}

/// The patients mini-viewer accepts the same birthdate range forms; a
/// malformed value is a 400 with the same message shape as the study level.
#[test]
fn smoke_birth_date_range_patients_level() {
    let server = TestServer::start();
    server.seed_via_stow();

    // Exact form -> only P001.
    let r = server.get("/dicomweb/patients?PatientBirthDate=19800101", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 1);
    let body = String::from_utf8_lossy(&r.body).into_owned();
    assert!(body.contains("P001") && !body.contains("P002"));

    // Range -> P001 (1980) + P003 (1975), not P002 (1990).
    let r = server.get(
        "/dicomweb/patients?PatientBirthDate=19700101-19851231",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);

    // Hex alias and wildcard are accepted.
    let r = server.get("/dicomweb/patients?00100030=19900101", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 1);
    let r = server.get("/dicomweb/patients?PatientBirthDate=*", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 3);

    // Malformed -> 400 (validated before any scan).
    let r = server.get("/dicomweb/patients?PatientBirthDate=1980", None);
    r.assert_status(400);
    let body = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        body.contains("PatientBirthDate must match pattern YYYYMMDD[-YYYYMMDD]"),
        "unexpected error body: {body}"
    );
}

// -- Stage B (08.09): the seven captured attributes as QIDO keys ----------

/// The corpus carries InstitutionName/Manufacturer/... per study:
/// studies 1+3 = ACME/CITY HOSPITAL/RADIOLOGY/CHEST_PROTO + ABDO_PROTO,
/// study 2 = SIEMENS/CITY HOSPITAL/NEURO/BRAIN_PROTO; operators differ.
#[test]
fn smoke_stage_b_search_keys() {
    let server = TestServer::start();
    server.seed_via_stow();

    // Study-level keys.
    let r = server.get("/dicomweb/studies?InstitutionName=CITY*", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 3);
    let r = server.get("/dicomweb/studies?RequestingPhysician=REQ^PAUL", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);
    let r = server.get("/dicomweb/studies?RequestingPhysician=REQ^P*", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);
    let r = server.get("/dicomweb/studies?RequestedProcedureDescription=CT*", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);
    // Hex-alias form.
    let r = server.get("/dicomweb/studies?00080080=CITY%20HOSPITAL", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 3);

    // Series-level keys.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?Manufacturer=ACME",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);
    let r = server.get(
        "/dicomweb/studies/1.1.2.1.1.1.1.1.200/series?ProtocolName=BRAIN*",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 1);
    let r = server.get("/dicomweb/series?Manufacturer=SIEMENS", None);
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 1);
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?OperatorsName=OPS^ANNA",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 2);

    // Level discipline mirrors the template keys.
    let r = server.get("/dicomweb/studies?Manufacturer=ACME", None);
    r.assert_status(400);
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?InstitutionName=CITY*",
        None,
    );
    r.assert_status(400);
    // Study's-Instances: series keys accepted-and-ignored (200).
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/instances?Manufacturer=ACME",
        None,
    );
    r.assert_status(200);
    assert!(json_len(&r.body) >= 1);
    // Global instances: study keys accepted-and-ignored.
    let r = server.get("/dicomweb/instances?InstitutionName=CITY*", None);
    r.assert_status(200);
    assert!(json_len(&r.body) >= 1);

    // PN length caps (ReferringPhysicianName template).
    let too_long = "A".repeat(65);
    let r = server.get(
        &format!("/dicomweb/studies?RequestingPhysician={too_long}"),
        None,
    );
    r.assert_status(400);
    let r = server.get(
        &format!("/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?OperatorsName={too_long}"),
        None,
    );
    r.assert_status(400);
}

/// The new attributes are NOT part of the mandatory response baseline: they
/// appear via `includefield` only, in JSON and XML.
#[test]
fn smoke_stage_b_attrs_includefield_only() {
    let server = TestServer::start();
    server.seed_via_stow();

    // The new attrs are NOT part of the mandatory baseline: a filtered query
    // (includefield ADDS to the mandatory set) must not emit them.
    let r = server.get(
        "/dicomweb/studies?PatientName=SMITH*&includefield=00080090",
        None,
    );
    r.assert_status(200);
    let keys = common::json_key_set(&r.body);
    assert!(
        keys.contains(&0x0008_0090),
        "the requested tag must be present"
    );
    assert!(
        !keys.contains(&0x0008_0080),
        "includefield must not leak the new attr into the baseline"
    );

    // includefield adds it.
    let r = server.get(
        "/dicomweb/studies?PatientName=SMITH*&includefield=00080080",
        None,
    );
    r.assert_status(200);
    let keys = common::json_key_set(&r.body);
    assert!(
        keys.contains(&0x0008_0080),
        "includefield must emit 00080080"
    );

    // Keyword form resolves too.
    let r = server.get(
        "/dicomweb/studies?PatientName=SMITH*&includefield=InstitutionName",
        None,
    );
    r.assert_status(200);
    let keys = common::json_key_set(&r.body);
    assert!(
        keys.contains(&0x0008_0080),
        "keyword includefield must emit 00080080"
    );

    // Series baseline has no 00080070; a filtered query must not leak it.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?includefield=00080060",
        None,
    );
    r.assert_status(200);
    assert!(!common::json_key_set(&r.body).contains(&0x0008_0070));
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?includefield=00080070",
        None,
    );
    r.assert_status(200);
    assert!(common::json_key_set(&r.body).contains(&0x0008_0070));
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?includefield=00080070",
        Some("application/dicom+xml"),
    );
    r.assert_status(200);
    assert!(common::xml_tag_set(&r.body).contains(&0x0008_0070));

    // The PN series key emits the PS3.18 object form in JSON.
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?includefield=00081070",
        None,
    );
    r.assert_status(200);
    let body = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        body.contains("\"Alphabetic\":\"OPS^ANNA\""),
        "OperatorsName must use the PN object form, got: {body}"
    );
}

// -- Relational cross-level keys on global/scoped routes ------------------

// DICOM PS3.18 Table 10.6.1.2-1 / the section-10.6.1.2.1 relational model
// marks Patient IE keys legal on every GLOBAL (all-<T>) resource, and Series
// IE keys legal on the study-scoped instances + all-instances resources.
// The relational-scope relaxation turns those foreign-key 400s into
// ACCEPT-AND-IGNORE: the key
// is parsed and validated, never a 400, and is NOT applied as a filter (the
// search uses only the route's own level keys). The scoped series resource
// keeps its strict policy (a Study/Patient key there re-scopes a wrong study).

fn json_len(body: &[u8]) -> usize {
    parse_json(body)
        .expect("response is DICOMweb JSON array")
        .as_array()
        .expect("array")
        .len()
}

/// Global All-Series: Patient IE keys legal (accepted-ignored), same result
/// set as the unfiltered scan -> 200, not the old 400.
#[test]
fn smoke_s5_global_series_accepts_patient_ie() {
    let server = TestServer::start();
    server.seed_via_stow();

    let all = server.get("/dicomweb/series", None);
    all.assert_status(200);
    let n_all = json_len(&all.body);

    for path in [
        "/dicomweb/series?PatientName=SMITH",
        "/dicomweb/series?PatientID=P001",
        "/dicomweb/series?PatientSex=M",
        "/dicomweb/series?PatientBirthDate=19800101",
    ] {
        let r = server.get(path, None);
        r.assert_status(200);
        assert_eq!(
            json_len(&r.body),
            n_all,
            "Patient IE key on global series is accepted-and-ignored: {path}"
        );
    }
}

/// Global All-Series: a NON-patient Study IE key is still foreign -> 400.
#[test]
fn smoke_s5_global_series_rejects_nonpatient_study_key() {
    let server = TestServer::start();
    server.seed_via_stow();
    let r = server.get("/dicomweb/series?StudyDate=20210101", None);
    r.assert_status(400);
}

/// Global All-Instances: Study IE AND Series IE keys legal (accepted-ignored),
/// same result set as the unfiltered scan -> 200, not the old 400.
#[test]
fn smoke_s5_global_instances_accepts_study_and_series_ie() {
    let server = TestServer::start();
    server.seed_via_stow();

    let all = server.get("/dicomweb/instances", None);
    all.assert_status(200);
    let n_all = json_len(&all.body);

    for path in [
        "/dicomweb/instances?StudyDate=20210101",
        "/dicomweb/instances?PatientName=SMITH",
        "/dicomweb/instances?SeriesNumber=7",
        "/dicomweb/instances?SeriesInstanceUID=2.1.1.1.1.1.1.1.100",
    ] {
        let r = server.get(path, None);
        r.assert_status(200);
        assert_eq!(
            json_len(&r.body),
            n_all,
            "Study/Series IE key on global instances is accepted-and-ignored: {path}"
        );
    }
}

/// Scoped Study's-Instances: Series IE keys legal (accepted-ignored) -> 200,
/// not the old 400. The study scope comes from the path.
#[test]
fn smoke_s5_scoped_study_instances_accepts_series_ie() {
    let server = TestServer::start();
    server.seed_via_stow();

    let path = "/dicomweb/studies/1.1.1.1.1.1.1.1.100/instances?SeriesNumber=7";
    let r = server.get(path, None);
    r.assert_status(200);
    assert!(
        json_len(&r.body) >= 1,
        "study-instances must still list its instances past the accepted key"
    );
}

/// Scoped Study's-Series: strict policy unchanged - a Study/Patient IE key
/// there still re-scopes a wrong study -> 400.
#[test]
fn smoke_s5_scoped_study_series_keeps_patient_400() {
    let server = TestServer::start();
    server.seed_via_stow();
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series?PatientName=SMITH",
        None,
    );
    r.assert_status(400);
}

// -- Modality as a REAL Series key on the relational instance routes --------

// PS3.18 10.6.1.2.1 makes Series IE keys (Modality included) LEGAL on the
// RELATIONAL instance resources (`/studies/{study}/instances`, `/instances`).
// Modality is not accept-and-ignore there: it must narrow the returned
// instances to the matching series, because a viewer (Slim) expects only that
// modality back. It stays REJECTED on the hierarchical
// `/studies/{study}/series/{series}/instances`.

/// STOW a two-series study: series A is SM (2 instances), series B is PR
/// (1 instance). Returns the study UID.
fn seed_sm_pr_study(server: &TestServer) -> &'static str {
    const STUDY: &str = "3.3.3.3.3.3.3.3.300";
    let sm = "1.2.840.10008.5.1.4.1.1.77.1.6"; // VL Whole Slide Microscopy Image
    let pr = "1.2.840.10008.5.1.4.1.1.11.1"; // Grayscale Softcopy Presentation State
    for (series_uid, series_num, inst_num, modality, sop_class) in [
        ("4.4.4.4.4.4.4.4.301", "1", "1", "SM", sm),
        ("4.4.4.4.4.4.4.4.301", "1", "2", "SM", sm),
        ("4.4.4.4.4.4.4.4.302", "2", "1", "PR", pr),
    ] {
        let d = build_instance(
            "SLIDE^ONE",
            "P300",
            "19850101",
            "F",
            STUDY,
            "20240101",
            "090000",
            "ACC-300",
            modality,
            "REF^DOC",
            "WSI Study",
            "S300",
            series_uid,
            series_num,
            "Slide series",
            "20240101",
            "090000",
            "SM1",
            "WHOLEBODY",
            "ACME",
            "CITY HOSPITAL",
            "PATH",
            "OPS^ANNA",
            "SLIDE_PROTO",
            "REQ^PAUL",
            "WSI WO",
            &format!("{series_uid}.{inst_num}"),
            sop_class,
            inst_num,
        );
        let r = server.post_single("/dicomweb/studies", &d);
        r.assert_status(200);
    }
    STUDY
}

/// Scoped Study's-Instances: `Modality` is accepted (no 400) and FILTERS -
/// `?Modality=PR` returns only the PR instances, `?Modality=SM` only the SM
/// ones, and a non-matching modality returns empty.
#[test]
fn smoke_modality_scoped_instances_filters() {
    let server = TestServer::start();
    let study = seed_sm_pr_study(&server);

    let all = server.get(&format!("/dicomweb/studies/{study}/instances"), None);
    all.assert_status(200);
    assert_eq!(json_len(&all.body), 3, "3 instances across both series");

    let sm = server.get(
        &format!("/dicomweb/studies/{study}/instances?Modality=SM"),
        None,
    );
    sm.assert_status(200);
    assert_eq!(json_len(&sm.body), 2, "?Modality=SM -> the 2 SM instances");

    let pr = server.get(
        &format!("/dicomweb/studies/{study}/instances?Modality=PR"),
        None,
    );
    pr.assert_status(200);
    assert_eq!(
        json_len(&pr.body),
        1,
        "?Modality=PR -> the single PR instance"
    );

    let none = server.get(
        &format!("/dicomweb/studies/{study}/instances?Modality=XX"),
        None,
    );
    none.assert_status(200);
    assert_eq!(json_len(&none.body), 0, "unknown modality -> empty");

    // The comma list still applies (any element exact).
    let multi = server.get(
        &format!("/dicomweb/studies/{study}/instances?Modality=PR,SM"),
        None,
    );
    multi.assert_status(200);
    assert_eq!(json_len(&multi.body), 3, "comma list matches both series");
}

/// Global All-Instances: `Modality` is accepted (no 400) and filters.
#[test]
fn smoke_modality_global_instances_filters() {
    let server = TestServer::start();
    let study = seed_sm_pr_study(&server);

    let sm = server.get("/dicomweb/instances?Modality=SM", None);
    sm.assert_status(200);
    assert_eq!(
        json_len(&sm.body),
        2,
        "global ?Modality=SM -> the 2 SM instances"
    );

    let pr = server.get("/dicomweb/instances?Modality=PR", None);
    pr.assert_status(200);
    assert_eq!(
        json_len(&pr.body),
        1,
        "global ?Modality=PR -> the PR instance"
    );

    // Combined with StudyInstanceUID (the Slim shape) it still narrows.
    let combo = server.get(
        &format!("/dicomweb/instances?StudyInstanceUID={study}&Modality=SM"),
        None,
    );
    combo.assert_status(200);
    assert_eq!(
        json_len(&combo.body),
        2,
        "StudyInstanceUID + Modality compose"
    );
}

/// Hierarchical `/studies/{study}/series/{series}/instances` stays STRICT:
/// Modality is a Series IE key and is NOT legal on that resource -> 400.
#[test]
fn smoke_modality_hierarchical_instances_still_400() {
    let server = TestServer::start();
    server.seed_via_stow();
    let r = server.get(
        "/dicomweb/studies/1.1.1.1.1.1.1.1.100/series/2.1.1.1.1.1.1.1.100/instances?Modality=CT",
        None,
    );
    r.assert_status(400);
}

// -- StudyInstanceUID as a REAL Series key on the global routes -------------

// PS3.18 2026c Table 10.6.1-2 makes StudyInstanceUID a REQUIRED Series
// matching key. It is therefore NOT accept-and-ignore on global `/series`:
// the route resolves the named study's manifest and applies it as a filter.
// The same short-circuit fixes global `/instances`, where the key used to be
// accepted-and-ignored (so a study-narrowed request returned every instance
// in the archive).

/// Global All-Series: `StudyInstanceUID=<existing>` narrows to exactly that
/// study's series - the same set the scoped `/studies/{uid}/series` returns.
#[test]
fn smoke_studyuid_global_series_narrows_to_study() {
    let server = TestServer::start();
    server.seed_via_stow();

    let uid = "1.1.1.1.1.1.1.1.100";
    let all = server.get("/dicomweb/series", None);
    all.assert_status(200);
    let n_all = json_len(&all.body);

    let scoped = server.get(&format!("/dicomweb/studies/{uid}/series"), None);
    scoped.assert_status(200);
    let n_scoped = json_len(&scoped.body);

    let filtered = server.get(&format!("/dicomweb/series?StudyInstanceUID={uid}"), None);
    filtered.assert_status(200);
    assert_eq!(
        json_len(&filtered.body),
        n_scoped,
        "global /series?StudyInstanceUID must equal the scoped study's series"
    );
    assert!(
        n_scoped < n_all,
        "the study must be a proper subset of the archive (n_scoped={n_scoped}, n_all={n_all})"
    );
}

/// Global All-Series: an unknown StudyInstanceUID is an empty result (200),
/// matching the scoped route's unknown-study behaviour.
#[test]
fn smoke_studyuid_global_series_unknown_study_is_empty() {
    let server = TestServer::start();
    server.seed_via_stow();
    let r = server.get(
        "/dicomweb/series?StudyInstanceUID=9.9.9.9.9.9.9.9.999",
        None,
    );
    r.assert_status(200);
    assert_eq!(json_len(&r.body), 0, "unknown study -> empty array");
}

/// Global All-Series: a `StudyInstanceUID` that is not a UID-shaped value is
/// rejected 400 by the path-traversal guard before any manifest is opened.
#[test]
fn smoke_studyuid_global_series_rejects_traversal() {
    let server = TestServer::start();
    server.seed_via_stow();
    let r = server.get("/dicomweb/series?StudyInstanceUID=../etc", None);
    r.assert_status(400);
}

/// Global All-Series: `Modality` still narrows WITHIN the resolved study (the
/// Slim repro shape) - the study filter and the modality filter compose.
#[test]
fn smoke_studyuid_global_series_composes_with_modality() {
    let server = TestServer::start();
    server.seed_via_stow();

    // Study .100 is CT-only, so narrowing by its own modality keeps its series
    // and narrowing by a foreign modality (MR) drops them all.
    let uid = "1.1.1.1.1.1.1.1.100";
    let scoped = server.get(&format!("/dicomweb/studies/{uid}/series"), None);
    scoped.assert_status(200);
    let n_scoped = json_len(&scoped.body);

    let ct = server.get(
        &format!("/dicomweb/series?Modality=CT&StudyInstanceUID={uid}"),
        None,
    );
    ct.assert_status(200);
    assert_eq!(
        json_len(&ct.body),
        n_scoped,
        "CT within a CT study keeps every series"
    );

    let mr = server.get(
        &format!("/dicomweb/series?Modality=MR&StudyInstanceUID={uid}"),
        None,
    );
    mr.assert_status(200);
    assert_eq!(
        json_len(&mr.body),
        0,
        "a foreign modality within the resolved study narrows to nothing"
    );
}

/// Global All-Instances: `StudyInstanceUID=<existing>` narrows to that study's
/// instances - a strict subset of the unfiltered archive enumeration.
#[test]
fn smoke_studyuid_global_instances_narrows_to_study() {
    let server = TestServer::start();
    server.seed_via_stow();

    let uid = "1.1.1.1.1.1.1.1.100";
    let all = server.get("/dicomweb/instances", None);
    all.assert_status(200);
    let n_all = json_len(&all.body);

    let scoped = server.get(&format!("/dicomweb/studies/{uid}/instances"), None);
    scoped.assert_status(200);
    let n_scoped = json_len(&scoped.body);

    let filtered = server.get(&format!("/dicomweb/instances?StudyInstanceUID={uid}"), None);
    filtered.assert_status(200);
    assert_eq!(
        json_len(&filtered.body),
        n_scoped,
        "global /instances?StudyInstanceUID must equal the scoped study's instances"
    );
    assert!(
        n_scoped < n_all,
        "the study's instances must be a strict subset (n_scoped={n_scoped}, n_all={n_all})"
    );
}

/// Build a valid STOW instance (a CT study) and, when `resp` is `Some`, append
/// a ResponsiblePerson (0010,2297, PN) element so the accept captures it.
/// FR-vet stage 1 integration.
fn rp_instance(study_uid: &str, resp: Option<&str>) -> Vec<u8> {
    let mut d = build_instance(
        "PAT^PET",
        "PETREC",
        "20200101",
        "O",
        study_uid,
        "20260802",
        "103000",
        "ACC-RP",
        "CT",
        "WHO^REF",
        "RP study",
        "SRPST",
        &format!("{study_uid}.1"), // SeriesInstanceUID (numeric)
        "1",
        "Axial",
        "20260802",
        "103000",
        "CT1",
        "CHEST",
        "ACME",
        "CITY HOSPITAL",
        "RADIOLOGY",
        "OPS^ANNA",
        "CHEST_PROTO",
        "REQ^PAUL",
        "CT CHEST WO",
        &format!("{study_uid}.1.1"), // SOPInstanceUID (numeric)
        "1.2.840.10008.5.1.4.1.1.2",
        "1",
    );
    if let Some(rp) = resp {
        append_element(&mut d, 0x0010, 0x2297, b"PN", rp.as_bytes());
    }
    d
}

/// FR-vet stage 1 integration: a study STOWed with ResponsiblePerson (0010,2297)
/// is found by `?ResponsiblePerson=<exact>` and by the hex alias
/// `?00102297=...`; a study WITHOUT the attribute is never matched by the key.
#[test]
fn responsible_person_stow_and_qido_search() {
    let server = TestServer::start();

    // Study A carries ResponsiblePerson=IVANOV^IVAN; Study B carries none.
    let a = "9.9.9.9.9.9.9.9.1";
    let b = "9.9.9.9.9.9.9.9.2";
    let ra = server.post_single("/dicomweb/studies", &rp_instance(a, Some("IVANOV^IVAN")));
    ra.assert_status(200);
    let rb = server.post_single("/dicomweb/studies", &rp_instance(b, None));
    rb.assert_status(200);

    // Exact keyword finds exactly the responsible study.
    let r = server.get("/dicomweb/studies?ResponsiblePerson=IVANOV^IVAN", None);
    r.assert_status(200);
    let body = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        body.contains(a),
        "keyword search must find the responsible study"
    );
    assert!(
        !body.contains(b),
        "keyword search must exclude the no-RP study"
    );
    assert_eq!(json_len(&r.body), 1, "exactly one matching study");

    // Hex alias (case-insensitive key mapping) behaves identically.
    let r2 = server.get("/dicomweb/studies?00102297=IVANOV^IVAN", None);
    r2.assert_status(200);
    assert!(
        String::from_utf8_lossy(&r2.body).contains(a),
        "hex-alias search must find the responsible study"
    );

    // Fuzzy (1-edit) is inherited via name_predicate.
    let r3 = server.get(
        "/dicomweb/studies?ResponsiblePerson=IVANOF&fuzzymatching=true",
        None,
    );
    r3.assert_status(200);
    assert!(
        String::from_utf8_lossy(&r3.body).contains(a),
        "fuzzymatch must find the 1-edit responsible study"
    );

    // An unrelated responsible person matches nothing.
    let r4 = server.get("/dicomweb/studies?ResponsiblePerson=SIDOROV", None);
    r4.assert_status(200);
    assert_eq!(
        json_len(&r4.body),
        0,
        "no study matches an unknown responsible"
    );
}

/// Append a defined-length item header carrying `body`.
fn append_item(buf: &mut Vec<u8>, body: &[u8]) {
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
    buf.extend_from_slice(&0xE000u16.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

/// A CT instance carrying the two nested sequences the QIDO dotted keys reach
/// into: RequestAttributesSequence (0040,0275) with one item holding
/// (0040,0009) and (0040,1001), and OtherPatientIDsSequence (0010,1002) with
/// one item holding (0010,0020).
fn nested_key_instance(study_uid: &str, step_id: &str, proc_id: &str, other_id: &str) -> Vec<u8> {
    let mut d = build_instance(
        "PAT^PET",
        "PETSEL",
        "20200101",
        "O",
        study_uid,
        "20260802",
        "103000",
        "ACC-NESTED",
        "CT",
        "WHO^REF",
        "nested study",
        "SNEST",
        &format!("{study_uid}.1"),
        "1",
        "Axial",
        "20260802",
        "103000",
        "CT1",
        "CHEST",
        "ACME",
        "CITY HOSPITAL",
        "RADIOLOGY",
        "OPS^ANNA",
        "CHEST_PROTO",
        "REQ^PAUL",
        "CT CHEST WO",
        &format!("{study_uid}.1.1"),
        "1.2.840.10008.5.1.4.1.1.2",
        "1",
    );

    let mut item = Vec::new();
    append_element(&mut item, 0x0040, 0x0009, b"SH", step_id.as_bytes());
    append_element(&mut item, 0x0040, 0x1001, b"SH", proc_id.as_bytes());
    let mut seq = Vec::new();
    append_item(&mut seq, &item);
    append_element(&mut d, 0x0040, 0x0275, b"SQ", &seq);

    let mut id_item = Vec::new();
    append_element(&mut id_item, 0x0010, 0x0020, b"LO", other_id.as_bytes());
    let mut id_seq = Vec::new();
    append_item(&mut id_seq, &id_item);
    append_element(&mut d, 0x0010, 0x1002, b"SQ", &id_seq);

    d
}

/// PS3.18 Table 10.6.1-5 types `00400275.00400009` and `00400275.00401001` as
/// REQUIRED series matching attributes, so both are captured at STOW and filter
/// at search - each key is a real filter, and a workitem without the sequence
/// is never matched by either.
#[test]
fn request_attributes_sequence_keys_stow_and_filter() {
    let server = TestServer::start();
    let a = "9.9.9.9.9.9.8.1";
    let b = "9.9.9.9.9.9.8.2";

    server
        .post_single(
            "/dicomweb/studies",
            &nested_key_instance(a, "SPS-A", "RP-A", "OTHER-A"),
        )
        .assert_status(200);
    server
        .post_single(
            "/dicomweb/studies",
            &nested_key_instance(b, "SPS-B", "RP-B", "OTHER-B"),
        )
        .assert_status(200);

    // Each accepted spelling finds exactly its own series.
    for (query, expected) in [
        (
            "requestattributessequence.scheduledprocedurestepid=SPS-A",
            a,
        ),
        ("00400275.00400009=SPS-A", a),
        ("requestattributessequence.requestedprocedureid=RP-A", a),
        ("00400275.00401001=RP-A", a),
    ] {
        let r = server.get(&format!("/dicomweb/series?{query}"), None);
        r.assert_status(200);
        let body = String::from_utf8_lossy(&r.body).into_owned();
        assert!(
            body.contains(expected),
            "{query} must find its series: {body}"
        );
        assert!(
            !body.contains(b),
            "{query} must exclude the other series: {body}"
        );
    }

    // A value nobody carries matches nothing (the negative case).
    assert_eq!(
        json_len(
            &server
                .get("/dicomweb/series?00400275.00400009=NOPE", None)
                .body
        ),
        0,
        "an unmatched step ID selects nothing"
    );
    assert_eq!(
        json_len(
            &server
                .get("/dicomweb/series?00400275.00401001=NOPE", None)
                .body
        ),
        0,
        "an unmatched requested procedure selects nothing"
    );
}

/// `00101002.00100020` is a MULTI-VALUED study key: the study matches when any
/// of the identifiers the sequence lists is the queried one.
#[test]
fn other_patient_ids_sequence_key_stow_and_filter() {
    let server = TestServer::start();
    let a = "9.9.9.9.9.9.7.1";
    let b = "9.9.9.9.9.9.7.2";

    server
        .post_single(
            "/dicomweb/studies",
            &nested_key_instance(a, "SPS-A", "RP-A", "OTHER-A"),
        )
        .assert_status(200);
    server
        .post_single(
            "/dicomweb/studies",
            &nested_key_instance(b, "SPS-B", "RP-B", "OTHER-B"),
        )
        .assert_status(200);

    for query in [
        "otherpatientidssequence.patientid=OTHER-A",
        "00101002.00100020=OTHER-A",
    ] {
        let r = server.get(&format!("/dicomweb/studies?{query}"), None);
        r.assert_status(200);
        let body = String::from_utf8_lossy(&r.body).into_owned();
        assert!(body.contains(a), "{query} must find its study: {body}");
        assert!(!body.contains(b), "{query} must exclude the other: {body}");
        assert_eq!(json_len(&r.body), 1, "{query} matches exactly one study");
    }

    assert_eq!(
        json_len(
            &server
                .get("/dicomweb/studies?00101002.00100020=NOPE", None)
                .body
        ),
        0,
        "an unmatched alternate identifier selects nothing"
    );
}
