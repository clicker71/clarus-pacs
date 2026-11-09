// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Freeze the DICOMweb Storage Commitment wire behaviour (PS3.18 Section 13).
//!
//! Boots the server over a real TCP socket and exercises Commit (13.4) and
//! Check Commit Result (13.5). The assertions pin the four places an
//! implementation built from a pre-publication draft tends to drift, all four
//! called out in the architect's report:
//!
//! 1. the Transaction UID lives in the resource PATH, never in the payload
//!    (13.4.1.1);
//! 2. 202 is never returned - Commit is synchronously Done (P1);
//! 3. a 200 may report every referenced instance as failed (13.4.2 Note);
//! 4. Check Commit Result takes no query parameters (13.5.1.2).
//!
//! The instance-presence half of the check reads the study manifest, so the
//! stored case is set up by writing a manifest through the ordinary STOW
//! machinery rather than by hand-crafting CBOR.

#![forbid(unsafe_code)]

use clarus_server::config::Config;
use clarus_server::handlers::json_common::{JsonVal, parse_json};
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Fixture identities ---------------------------------------------------

const STORED_SOP_INSTANCE: &str = "1.2.826.0.1.3680043.10.2309.1001";
const MISSING_SOP_INSTANCE: &str = "1.2.826.0.1.3680043.10.2309.1002";
const SOP_CLASS_CT: &str = "1.2.840.10008.5.1.4.1.1.2";
const STUDY_UID: &str = "1.2.826.0.1.3680043.10.2309.1";

const TXN_OK: &str = "1.2.826.0.1.3680043.10.2309.9001";
const TXN_MIXED: &str = "1.2.826.0.1.3680043.10.2309.9002";
const TXN_ALL_FAILED: &str = "1.2.826.0.1.3680043.10.2309.9003";
const TXN_STUDY: &str = "1.2.826.0.1.3680043.10.2309.9004";

const JSON_CT: &str = "application/dicom+json";

// -- Test server harness (mirror of ups_smoke.rs) -------------------------

struct TestServer {
    addr: SocketAddr,
    _temp: tempfile::TempDir,
    _state: Arc<AppState>,
}

impl TestServer {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");

        let btree_root = temp.path().join("btree");
        std::fs::create_dir_all(&btree_root).expect("btree dir");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let mut config = Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        // Keep the retention sweep off for the wire scenarios; the sweep has
        // its own unit tests in clarus-core::commitment::store.
        config.commitment.result_retention_days = 0;

        let state = AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(temp.path().join("cas"))
                .expect("open cas"),
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

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(state);
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let sd = shutdown.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        let serving = Arc::clone(&state);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, serving, sd, timeouts, u32::MAX);
        });

        Self {
            addr,
            _temp: temp,
            _state: state,
        }
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        self.request("GET", path, headers, None)
    }

    fn post(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Response {
        self.request("POST", path, headers, Some(body))
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

    /// Commit with the standard headers (Accept: application/dicom+json).
    fn commit(&self, txn: &str, body: &[u8]) -> Response {
        self.post(
            &format!("/dicomweb/commitment-requests/{txn}"),
            &[("Content-Type", JSON_CT), ("Accept", JSON_CT)],
            body,
        )
    }

    /// Check Commit Result with the Default Accept.
    fn check(&self, txn: &str) -> Response {
        self.get(
            &format!("/dicomweb/commitment-requests/{txn}"),
            &[("Accept", JSON_CT)],
        )
    }
}

fn parse_http(buf: &[u8]) -> Response {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let body = buf[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    let mut headers: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers
                .entry(k.trim().to_ascii_lowercase())
                .or_default()
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
        assert_eq!(
            self.status,
            expected,
            "status should be {expected}; body: {}",
            String::from_utf8_lossy(&self.body)
        );
    }

    fn json(&self) -> JsonVal {
        parse_json(&self.body)
            .unwrap_or_else(|| panic!("json body: {}", String::from_utf8_lossy(&self.body)))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

// -- Request payload builders ---------------------------------------------

/// Instance-form request body: Referenced SOP Sequence (Annex J.1).
fn instance_request(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut body = String::from("{\"00081199\":{\"vr\":\"SQ\",\"Value\":[");
    for (i, (class, instance)) in pairs.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!(
            "{{\"00081150\":{{\"vr\":\"UI\",\"Value\":[\"{class}\"]}},\
             \"00081155\":{{\"vr\":\"UI\",\"Value\":[\"{instance}\"]}}}}"
        ));
    }
    body.push_str("]}}");
    body.into_bytes()
}

/// Study-form request body: 0008,1110 -> 0008,1115 -> 0008,1112 -> 0008,114A.
fn study_request(study_uid: &str, series_uid: &str, pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut inner = String::new();
    for (i, (class, instance)) in pairs.iter().enumerate() {
        if i > 0 {
            inner.push(',');
        }
        inner.push_str(&format!(
            "{{\"00081150\":{{\"vr\":\"UI\",\"Value\":[\"{class}\"]}},\
             \"0008114A\":{{\"vr\":\"SQ\",\"Value\":[\
             {{\"00081155\":{{\"vr\":\"UI\",\"Value\":[\"{instance}\"]}}}}]}}}}"
        ));
    }
    format!(
        "{{\"00081110\":{{\"vr\":\"SQ\",\"Value\":[{{\
           \"0020000D\":{{\"vr\":\"UI\",\"Value\":[\"{study_uid}\"]}},\
           \"00081115\":{{\"vr\":\"SQ\",\"Value\":[{{\
             \"0020000E\":{{\"vr\":\"UI\",\"Value\":[\"{series_uid}\"]}},\
             \"00081112\":{{\"vr\":\"SQ\",\"Value\":[{inner}]}}}}]}}}}]}}}}"
    )
    .into_bytes()
}

/// A minimal DICOM instance as multipart/related STOW parts.
fn stow_body(study_uid: &str, series_uid: &str, sop_instance_uid: &str) -> (String, Vec<u8>) {
    let boundary = "clarus2309boundary";
    let dicom = minimal_instance(study_uid, series_uid, sop_instance_uid);
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/dicom\r\n\r\n");
    body.extend_from_slice(&dicom);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (
        format!("multipart/related; type=\"application/dicom\"; boundary={boundary}"),
        body,
    )
}

/// Append one Explicit VR Little Endian element (with the correct short/long
/// form and the VR-specific padding), as `stow_smoke.rs` does.
fn append_element(buf: &mut Vec<u8>, group: u16, elem: u16, vr: &[u8; 2], value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&elem.to_le_bytes());
    buf.extend_from_slice(vr);
    let len = value.len();
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
        return;
    }
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

/// Build a minimal Explicit VR LE DICOM instance with a fixed identity and
/// non-empty pixel data.
///
/// Only the tags the manifest writer reads are present; the geometry is set so
/// the instance passes the store-side validation the ordinary STOW path runs.
fn minimal_instance(study_uid: &str, series_uid: &str, sop_instance_uid: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", SOP_CLASS_CT.as_bytes());
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_instance_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"COMMIT^SMOKE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"PT2309");
    append_element(&mut d, 0x0020, 0x000D, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000E, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    let pixels = [0x5Au8; 64];
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
}

/// Read `obj[TAG].Value[0]` as a string - the DICOM JSON single-value shape.
///
/// The `Value` hop matters: a DICOM JSON attribute is
/// `{"vr":..,"Value":[..]}`, so reading the array directly off the tag yields
/// the attribute object, not the value.
fn attr(obj: &JsonVal, tag: &str) -> Option<String> {
    obj.obj_get(tag)?
        .obj_get("Value")?
        .as_str_array_first()
        .map(str::to_string)
}

/// Read `obj[TAG].Value` as the item array of a sequence.
fn seq_items<'a>(obj: &'a JsonVal, tag: &str) -> Option<&'a Vec<JsonVal>> {
    obj.obj_get(tag)?.obj_get("Value")?.as_array()
}

// -- Scenarios ------------------------------------------------------------

/// Store one instance through the ordinary STOW route so the manifest exists.
fn store_one(server: &TestServer) {
    let series_uid = "1.2.826.0.1.3680043.10.2309.10.1";
    let (ct, body) = stow_body(STUDY_UID, series_uid, STORED_SOP_INSTANCE);
    let r = server.post(
        "/dicomweb/studies",
        &[("Content-Type", &ct), ("Accept", JSON_CT)],
        &body,
    );
    assert!(
        r.status == 200 || r.status == 202,
        "STOW fixture must be accepted; got {}: {}",
        r.status,
        r.text()
    );
}

#[test]
fn commit_returns_200_and_lists_the_committed_instance() {
    let server = TestServer::start();
    store_one(&server);

    let body = instance_request(&[(SOP_CLASS_CT, STORED_SOP_INSTANCE)]);
    let r = server.commit(TXN_OK, &body);
    r.assert_status(200);
    assert_eq!(
        r.header("content-type"),
        Some("application/dicom+json; charset=utf-8")
    );

    let json = r.json();
    let committed = seq_items(&json, "00081199")
        .expect("Referenced SOP Sequence must be present for a committed instance");
    assert_eq!(committed.len(), 1);
    assert_eq!(
        attr(&committed[0], "00081155").as_deref(),
        Some(STORED_SOP_INSTANCE)
    );
    assert!(
        json.obj_get("00081198").is_none(),
        "a fully committed answer carries no Failed SOP Sequence"
    );
}

#[test]
fn commit_reports_a_missing_instance_as_0112_in_the_failed_sequence() {
    let server = TestServer::start();
    store_one(&server);

    let body = instance_request(&[
        (SOP_CLASS_CT, STORED_SOP_INSTANCE),
        (SOP_CLASS_CT, MISSING_SOP_INSTANCE),
    ]);
    let r = server.commit(TXN_MIXED, &body);
    r.assert_status(200);

    let json = r.json();
    let committed = seq_items(&json, "00081199").expect("committed sequence");
    assert_eq!(committed.len(), 1);

    let failed = seq_items(&json, "00081198").expect("failed sequence");
    assert_eq!(failed.len(), 1);
    assert_eq!(
        attr(&failed[0], "00081155").as_deref(),
        Some(MISSING_SOP_INSTANCE)
    );
    // PS3.3 C.14.1.1: 0112H "No such SOP Instance".
    assert_eq!(
        attr(&failed[0], "00081197").as_deref(),
        Some("0112"),
        "a missing instance is 0112H No such SOP Instance"
    );
}

#[test]
fn commit_answers_200_even_when_every_instance_failed() {
    // 13.4.2 Note: "The Storage Commitment response may indicate that storage
    // commitment failed for some or even all of the referenced SOP Instances."
    // The per-instance failure is an ordinary 200 payload, not an error.
    let server = TestServer::start();

    let body = instance_request(&[(SOP_CLASS_CT, MISSING_SOP_INSTANCE)]);
    let r = server.commit(TXN_ALL_FAILED, &body);
    r.assert_status(200);

    let json = r.json();
    assert!(
        json.obj_get("00081199").is_none(),
        "nothing was committed, so there is no Referenced SOP Sequence"
    );
    let failed = seq_items(&json, "00081198").expect("failed sequence");
    assert_eq!(failed.len(), 1);
}

#[test]
fn a_duplicate_transaction_uid_is_409_and_leaves_the_first_result_intact() {
    let server = TestServer::start();
    store_one(&server);

    let first = instance_request(&[(SOP_CLASS_CT, STORED_SOP_INSTANCE)]);
    server.commit(TXN_OK, &first).assert_status(200);

    // A second Commit under the same UID must be refused, and the recorded
    // answer must not be replaced.
    let second = instance_request(&[(SOP_CLASS_CT, MISSING_SOP_INSTANCE)]);
    let r = server.commit(TXN_OK, &second);
    r.assert_status(409);

    let after = server.check(TXN_OK);
    after.assert_status(200);
    let json = after.json();
    let committed =
        seq_items(&json, "00081199").expect("the first result must survive the refused duplicate");
    assert_eq!(
        attr(&committed[0], "00081155").as_deref(),
        Some(STORED_SOP_INSTANCE)
    );
}

#[test]
fn check_returns_the_stored_result_and_404_for_an_unknown_transaction() {
    let server = TestServer::start();
    store_one(&server);
    let body = instance_request(&[(SOP_CLASS_CT, STORED_SOP_INSTANCE)]);
    server.commit(TXN_OK, &body).assert_status(200);

    let r = server.check(TXN_OK);
    r.assert_status(200);
    assert_eq!(
        r.header("content-type"),
        Some("application/dicom+json; charset=utf-8")
    );
    assert!(r.json().obj_get("00081199").is_some());

    // An unknown Transaction UID has no result: 404, not 410 (no tombstone).
    server.check(TXN_MIXED).assert_status(404);
}

#[test]
fn a_query_parameter_is_refused_on_both_transactions() {
    // 13.4.1.2 and 13.5.1.2: neither request has query parameters.
    let server = TestServer::start();
    let body = instance_request(&[(SOP_CLASS_CT, MISSING_SOP_INSTANCE)]);

    let r = server.post(
        &format!("/dicomweb/commitment-requests/{TXN_OK}?foo=bar"),
        &[("Content-Type", JSON_CT), ("Accept", JSON_CT)],
        &body,
    );
    r.assert_status(400);

    server
        .get(
            &format!("/dicomweb/commitment-requests/{TXN_OK}?foo=bar"),
            &[("Accept", JSON_CT)],
        )
        .assert_status(400);
}

#[test]
fn a_duplicate_instance_in_one_request_is_400() {
    // PS3.4 J.3.2.1.1.3: a SOP Instance may be referenced only once.
    let server = TestServer::start();
    let body = instance_request(&[
        (SOP_CLASS_CT, STORED_SOP_INSTANCE),
        (SOP_CLASS_CT, STORED_SOP_INSTANCE),
    ]);
    server.commit(TXN_OK, &body).assert_status(400);
}

#[test]
fn a_payload_carrying_a_dimese_only_attribute_is_refused() {
    // 0088,0130 Storage Media File-Set ID belongs to the DIMSE Action
    // Information (PS3.4 Table J.3-1) and is not part of Annex J.1.
    let server = TestServer::start();
    let body = b"{\"00081199\":{\"vr\":\"SQ\",\"Value\":[{\"00081150\":{\"vr\":\"UI\",\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"]},\"00081155\":{\"vr\":\"UI\",\"Value\":[\"1.2.3\"]}}]},\"00880130\":{\"vr\":\"SH\",\"Value\":[\"FSET\"]}}";
    server.commit(TXN_OK, body).assert_status(400);
}

#[test]
fn an_empty_or_malformed_body_is_400() {
    let server = TestServer::start();
    server.commit(TXN_OK, b"").assert_status(400);
    server.commit(TXN_OK, b"{\"00081199\"").assert_status(400);
    server.commit(TXN_OK, b"{}").assert_status(400);
}

#[test]
fn an_xml_accept_yields_the_dicom_xml_form() {
    let server = TestServer::start();
    store_one(&server);
    let body = instance_request(&[(SOP_CLASS_CT, STORED_SOP_INSTANCE)]);
    let r = server.post(
        &format!("/dicomweb/commitment-requests/{TXN_OK}"),
        &[
            ("Content-Type", JSON_CT),
            ("Accept", "application/dicom+xml"),
        ],
        &body,
    );
    r.assert_status(200);
    assert_eq!(r.header("content-type"), Some("application/dicom+xml"));
    let xml = r.text();
    assert!(xml.contains("<NativeDicomModel"), "xml body: {xml}");
    assert!(xml.contains("00081199"), "xml body: {xml}");
}

#[test]
fn a_multipart_accept_yields_a_one_part_multipart_body() {
    // Table 13.1.3-1 marks both multipart/related forms Required at service
    // level, so the handler must answer with a one-part body rather than
    // reject the header.
    let server = TestServer::start();
    store_one(&server);
    let body = instance_request(&[(SOP_CLASS_CT, STORED_SOP_INSTANCE)]);
    let r = server.post(
        &format!("/dicomweb/commitment-requests/{TXN_OK}"),
        &[
            ("Content-Type", JSON_CT),
            (
                "Accept",
                "multipart/related; type=\"application/dicom+json\"",
            ),
        ],
        &body,
    );
    r.assert_status(200);
    let ct = r.header("content-type").expect("content type");
    assert!(ct.starts_with("multipart/related"), "content type: {ct}");
    let boundary = ct
        .split("boundary=")
        .nth(1)
        .expect("boundary in content type")
        .trim_matches('"')
        .to_string();
    let text = r.text();
    assert!(
        text.starts_with(&format!("--{boundary}\r\n")),
        "body must open with the part delimiter: {text}"
    );
    assert!(
        text.ends_with(&format!("\r\n--{boundary}--\r\n")),
        "body must close with the delimiter: {text}"
    );
    assert!(
        text.contains("Content-Type: application/dicom+json"),
        "part header: {text}"
    );
}

#[test]
fn the_study_form_answers_with_the_study_shape() {
    let server = TestServer::start();
    store_one(&server);

    let series_uid = "1.2.826.0.1.3680043.10.2309.10.1";
    let body = study_request(
        STUDY_UID,
        series_uid,
        &[(SOP_CLASS_CT, STORED_SOP_INSTANCE)],
    );
    let r = server.commit(TXN_STUDY, &body);
    r.assert_status(200);

    let json = r.json();
    // The result form mirrors the request form: a study-shaped request is
    // answered with Referenced Study Sequence, not Referenced SOP Sequence.
    let studies = seq_items(&json, "00081110").expect("Referenced Study Sequence");
    assert_eq!(studies.len(), 1);
    assert_eq!(attr(&studies[0], "0020000D").as_deref(), Some(STUDY_UID));
    assert!(
        json.obj_get("0008119B").is_none(),
        "a fully committed study carries no Failed Study Sequence"
    );
}

#[test]
fn a_study_shaped_request_reports_a_missing_instance_under_the_failed_study() {
    let server = TestServer::start();
    store_one(&server);

    let series_uid = "1.2.826.0.1.3680043.10.2309.10.1";
    let body = study_request(
        STUDY_UID,
        series_uid,
        &[
            (SOP_CLASS_CT, STORED_SOP_INSTANCE),
            (SOP_CLASS_CT, MISSING_SOP_INSTANCE),
        ],
    );
    let r = server.commit(TXN_STUDY, &body);
    r.assert_status(200);

    let json = r.json();
    let failed_studies = seq_items(&json, "0008119B").expect("Failed Study Sequence");
    assert_eq!(failed_studies.len(), 1);
    assert_eq!(
        attr(&failed_studies[0], "0020000D").as_deref(),
        Some(STUDY_UID)
    );
    let failed = seq_items(&failed_studies[0], "00081198").expect("nested Failed SOP Sequence");
    assert_eq!(attr(&failed[0], "00081197").as_deref(), Some("0112"));
}

#[test]
fn the_transaction_uid_is_in_the_path_never_a_payload_attribute() {
    // 13.4.1.1: the UID is encoded in the resource path, not the response. If
    // it ever appeared in the body, a client could not tell the two apart.
    let server = TestServer::start();
    let body = instance_request(&[(SOP_CLASS_CT, MISSING_SOP_INSTANCE)]);
    let r = server.commit(TXN_OK, &body);
    r.assert_status(200);
    assert!(
        !r.text().contains(TXN_OK),
        "the Transaction UID must not appear in the payload: {}",
        r.text()
    );
}

#[test]
fn a_collection_path_without_a_transaction_uid_is_not_a_transaction() {
    // 13.3: both transactions live on /commitment-requests/{TransactionUID}.
    // The bare collection path names no transaction, so it is not a resource
    // at all: GET has no route there (404) and POST is not allowed on it.
    let server = TestServer::start();
    server
        .get("/dicomweb/commitment-requests", &[("Accept", JSON_CT)])
        .assert_status(404);
    let body = instance_request(&[(SOP_CLASS_CT, MISSING_SOP_INSTANCE)]);
    let r = server.post(
        "/dicomweb/commitment-requests",
        &[("Content-Type", JSON_CT), ("Accept", JSON_CT)],
        &body,
    );
    assert!(
        r.status == 404 || r.status == 405,
        "a collection POST must not be treated as a transaction; got {}: {}",
        r.status,
        r.text()
    );
}

#[test]
fn a_result_survives_a_store_read_back() {
    // The result is written through the same tmp+rename discipline as the
    // study manifest, so reading it back from a fresh store handle must work.
    let server = TestServer::start();
    store_one(&server);
    let body = instance_request(&[(SOP_CLASS_CT, STORED_SOP_INSTANCE)]);
    server.commit(TXN_OK, &body).assert_status(200);

    let store =
        clarus_core::commitment::CommitmentStore::new(server._state.commitments_dir.clone());
    let record = store
        .find(TXN_OK)
        .expect("read store")
        .expect("the committed result must be durable");
    assert_eq!(record.transaction_uid, TXN_OK);
    assert_eq!(record.instances.len(), 1);
    assert!(record.all_committed());
}

#[test]
fn the_capabilities_document_advertises_commitment_rs() {
    let server = TestServer::start();
    let r = server.get("/dicomweb/", &[]);
    r.assert_status(200);
    // The Capabilities Description is a WADL document (PS3.18 Annex H), so the
    // Storage Commitment resources appear as the `commitment-requests` path
    // and its `{TransactionUID}` child carrying both transactions (13.3).
    let body = r.text();
    assert!(
        body.contains(r#"<resource path="commitment-requests">"#),
        "Storage Commitment resource missing: {body}"
    );
    assert!(
        body.contains(r#"<resource path="{TransactionUID}">"#),
        "commitment TransactionUID resource missing: {body}"
    );
    assert!(
        body.contains(r#"<method name="POST" id="Commit"/>"#),
        "Commit method missing: {body}"
    );
    assert!(
        body.contains(r#"<method name="GET" id="Check Commit Result"/>"#),
        "Check Commit Result method missing: {body}"
    );
}

#[test]
fn a_swept_result_makes_check_answer_404() {
    // P2/P4: the retention sweep deletes the result, and Check then answers
    // 404 - Table 13.5.3-1 allows 404 for "the origin server may have deleted
    // the applicable result", and no 410 tombstone is kept.
    let server = TestServer::start();
    let body = instance_request(&[(SOP_CLASS_CT, MISSING_SOP_INSTANCE)]);
    server.commit(TXN_OK, &body).assert_status(200);
    server.check(TXN_OK).assert_status(200);

    let store =
        clarus_core::commitment::CommitmentStore::new(server._state.commitments_dir.clone());
    // A retention window of one day, evaluated far in the future, expires it.
    let removed =
        clarus_core::commitment::store::sweep_expired(&store, 1, 4_000_000_000).expect("sweep");
    assert_eq!(removed, vec![TXN_OK.to_string()]);

    server.check(TXN_OK).assert_status(404);
}

#[test]
fn a_retention_of_zero_keeps_results_available() {
    let server = TestServer::start();
    let body = instance_request(&[(SOP_CLASS_CT, MISSING_SOP_INSTANCE)]);
    server.commit(TXN_OK, &body).assert_status(200);

    let store =
        clarus_core::commitment::CommitmentStore::new(server._state.commitments_dir.clone());
    let removed =
        clarus_core::commitment::store::sweep_expired(&store, 0, 4_000_000_000).expect("sweep");
    assert!(removed.is_empty(), "0 means keep forever");

    server.check(TXN_OK).assert_status(200);
}
