// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! freeze: the two new UPS-RS endpoints.
//!
//! PS3.18:
//!   - 11.3 GET  /dicomweb/workitems/{uid}   - retrieve one workitem.
//!   - 11.4 POST /dicomweb/workitems?search  - search with a DICOMweb JSON body.
//!
//! Golden fixtures for these endpoints are added here; the existing ups_smoke
//! scenarios are untouched. The server is (CBOR workitem store).
//!
//! The golden for `GET /workitems/{uid}` is a single DICOMweb JSON object
//! (not an array); the `POST ?search` result must be byte-identical to the
//! equivalent `GET /workitems` query result.

#![forbid(unsafe_code)]

use clarus_server::handlers::json_common::{JsonVal, parse_json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const WORKITEM_A: &str = "1.2.826.0.1.3680043.10.1234.8101";

struct TestServer {
    addr: SocketAddr,
    _temp: tempfile::TempDir,
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

        // the server boots a real backend for the rest of the
        // storage tier; the UPS path uses the CBOR store at workitems_dir.

        let mut config = clarus_server::config::Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        let state = clarus_server::state::AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(temp.path().join("cas"))
                .expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: clarus_server::sse_fanout::DualEventSink::new(),
            start_time: std::time::Instant::now(),
            metrics_ring: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            log_buffer: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::VecDeque::new(),
            )),
            maintenance: std::sync::Arc::new(std::sync::RwLock::new(
                clarus_server::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: clarus_server::shadow::ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let state = std::sync::Arc::new(state);
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let sd = shutdown.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, state, sd, timeouts, u32::MAX);
        });

        Self { addr, _temp: temp }
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path, &[("Accept", "application/dicom+json")], None)
    }

    fn post_json(&self, path: &str, body: &[u8]) -> Response {
        self.request(
            "POST",
            path,
            &[("Content-Type", "application/json")],
            Some(body),
        )
    }

    /// Create a workitem the way PS3.18 11.4.1.1 defines it.
    ///
    /// Table 11.4.1-1 puts the Workitem UID in a QUERY parameter on the
    /// worklist resource, not in a path segment: `POST /workitems/{uid}` is
    /// the Update resource (11.6), so a create sent there would be answered
    /// as an update of a workitem that does not exist yet.
    fn create(&self, uid: &str, body: &[u8]) -> Response {
        self.post_json(&format!("/dicomweb/workitems?workitem={uid}"), body)
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
    fn status(&self) -> u16 {
        self.status
    }
    fn json(&self) -> JsonVal {
        parse_json(&self.body).expect("json body")
    }
    /// The body as text, for the assertions that are about an attribute
    /// being PRESENT (or absent) rather than about its parsed value.
    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A create body carrying the four type-1 attributes of PS3.4 Table
/// CC.2.5-3, the station as the (0040,4025) code sequence the standard
/// defines, and the study UID inside (0040,4021) Input Information Sequence,
/// which is the only place a workitem can name the study it operates on.
fn workitem_body(uid: &str, label: &str) -> String {
    format!(
        r#"{{"00404021":{{"vr":"SQ","Value":[{{"0020000D":{{"vr":"UI","Value":["{uid}"]}}}}]}},"00741000":{{"vr":"CS","Value":["SCHEDULED"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"LO","Value":["{label}"]}},"00404005":{{"vr":"DT","Value":["20260101120000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00404025":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["STATION01"]}},"00080102":{{"vr":"SH","Value":["99CLARUS"]}},"00080104":{{"vr":"LO","Value":["AI station"]}}}}]}},"00100010":{{"vr":"PN","Value":[{{"Alphabetic":"UPS^PATIENT"}}]}},"00100020":{{"vr":"LO","Value":["UPSPAT01"]}}}}"#
    )
}

#[test]
fn get_workitem_single_retrieve() {
    let srv = TestServer::start();
    srv.create(WORKITEM_A, workitem_body(WORKITEM_A, "CT-CHEST").as_bytes())
        .status();

    // 11.3: GET /dicomweb/workitems/{uid} -> 200 single object.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"));
    assert_eq!(r.status(), 200, "GET workitem by uid should be 200");
    let ct = r.header("content-type").unwrap_or("");
    assert!(
        ct.contains("application/dicom+json"),
        "dicom+json ct, got {ct}"
    );
    let j = r.json();
    assert_eq!(
        j.obj_get("00080018")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some(WORKITEM_A),
        "the workitem UID is the identity slot"
    );
    assert_eq!(
        j.obj_get("00741000")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("SCHEDULED")
    );
    assert!(matches!(j, JsonVal::Obj(_)), "single object, not array");
}

#[test]
fn get_workitem_unknown_404() {
    let srv = TestServer::start();
    let r = srv.get("/dicomweb/workitems/1.2.3.4.9999");
    assert_eq!(r.status(), 404, "unknown workitem should 404");
}

#[test]
fn post_search_matches_get_search_byte_exact() {
    let srv = TestServer::start();
    srv.create(WORKITEM_A, workitem_body(WORKITEM_A, "CT-CHEST").as_bytes())
        .status();

    // GET search by the Scheduled Station Name Code Sequence key.
    let get_res = srv.get("/dicomweb/workitems?00404025=STATION01");
    assert_eq!(get_res.status(), 200);
    let get_arr = get_res.json();
    assert!(
        get_arr.as_array().map(|a| a.len()).unwrap_or(0) == 1,
        "GET search finds it"
    );

    // 11.4: POST /workitems?search with JSON body -> must equal GET result.
    let body =
        br#"{"00404025":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["STATION01"]}}]}}"#;
    let post_res = srv.post_json("/dicomweb/workitems?search", body);
    assert_eq!(post_res.status(), 200, "POST ?search should 200");
    let post_arr = post_res.json();
    assert_eq!(
        post_arr, get_arr,
        "POST ?search byte-identical to GET search"
    );

    // The found workitem is WORKITEM_A. The study it operates on is named
    // through the Referenced Request Sequence (0040,A370) - the bare
    // top-level (0020,000D) is gone (Task 3), so the study is read from the
    // sequence. The fixture uses WORKITEM_A as the study UID.
    let found = post_arr.as_array().unwrap();
    assert!(
        found[0].obj_get("0020000D").is_none(),
        "no bare top-level study tag"
    );
    assert_eq!(
        found[0]
            .obj_get("0040A370")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.first_of_array())
            .and_then(|item| item.obj_get("0020000D"))
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some(WORKITEM_A)
    );

    // And the search array JSON equals the single-item GET object contents.
    assert_eq!(
        found[0],
        srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}")).json()
    );
}

#[test]
fn post_search_requires_search_marker() {
    let srv = TestServer::start();
    let body =
        br#"{"00404025":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["STATION01"]}}]}}"#;
    // POST /workitems (no ?search) -> 400.
    let r = srv.post_json("/dicomweb/workitems", body);
    assert_eq!(r.status(), 400, "POST workitems without ?search should 400");
}

// -- Sprint A / A4: the AIW-I station key (0040,4025) --------------------

/// A create body that schedules a workitem with a station-CODE assignment
/// but no (0040,0001), which is the shape an AIW-I requester sends: the
/// profile's station table keys on the code sequence.
fn workitem_body_with_station_code(uid: &str, code_value: &str) -> String {
    format!(
        r#"{{"00404021":{{"vr":"SQ","Value":[{{"0020000D":{{"vr":"UI","Value":["{uid}"]}}}}]}},"00741000":{{"vr":"CS","Value":["SCHEDULED"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"LO","Value":["CT-CHEST"]}},"00404005":{{"vr":"DT","Value":["20260101120000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00404025":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["{code_value}"]}},"00080102":{{"vr":"SH","Value":["99CLARUS"]}},"00080104":{{"vr":"LO","Value":["AI station"]}}}}]}}}}"#
    )
}

#[test]
fn the_aiwi_station_key_alias_matches_the_same_workitem() {
    // PS3.4 Table CC.2.5-3 carries the station ONCE - as the Code Value of
    // (0040,4025) Scheduled Station Name Code Sequence - and gives the vendor
    // (0040,0001) no row at all. AIW-I 4.81.4.1.2.1 Table 4.81.4.1.2.1-3 then
    // makes that Code Value the AE Title a performer claims, so one search
    // key reaches every workitem a station was offered.
    let srv = TestServer::start();
    let uid = WORKITEM_A;
    srv.create(
        uid,
        workitem_body_with_station_code(uid, "STATION01").as_bytes(),
    )
    .status();

    let by_code = srv.get("/dicomweb/workitems?00404025=STATION01");
    assert_eq!(by_code.status(), 200, "the code-key search is accepted");
    let rows = by_code.json();
    assert_eq!(
        rows.as_array().map(Vec::len).unwrap_or(0),
        1,
        "the (0040,4025) Code Value reaches the workitem that carries it: {}",
        by_code.body_text()
    );

    // All three accepted spellings of that one key agree; the dotted-hex form
    // is what dcm4chee writes and the keyword path is what PS3.18 8.3.4
    // defines.
    for query in [
        "00404025.00080100=STATION01",
        "ScheduledStationNameCodeSequence.CodeValue=STATION01",
    ] {
        let r = srv.get(&format!("/dicomweb/workitems?{query}"));
        assert_eq!(r.status(), 200, "{query} is accepted");
        assert_eq!(
            r.json().as_array().map(Vec::len).unwrap_or(0),
            1,
            "{query} reaches the same workitem: {}",
            r.body_text()
        );
    }

    // The vendor key is GONE: a search that still sends it is refused rather
    // than answered, so a client cannot keep relying on a spelling the
    // standard does not define.
    let removed = srv.get("/dicomweb/workitems?00400001=STATION01");
    assert_eq!(
        removed.status(),
        400,
        "the vendor AE-title key is no longer a matching key: {}",
        removed.body_text()
    );
}

#[test]
fn the_standard_station_key_wins_when_a_search_carries_both_forms() {
    // Two workitems with DIFFERENT stations, and a search that names one of
    // them through the standard key. The answer is that workitem and only it.
    let srv = TestServer::start();
    let first = "1.2.826.0.1.3680043.10.2001";
    let second = "1.2.826.0.1.3680043.10.2002";
    srv.create(
        first,
        workitem_body_with_station_code(first, "STATION01").as_bytes(),
    )
    .status();
    srv.create(
        second,
        workitem_body_with_station_code(second, "STATION02").as_bytes(),
    )
    .status();

    let both = srv.get(
        "/dicomweb/workitems?00404025=STATION01&\
         ScheduledStationNameCodeSequence.CodeValue=STATION01",
    );
    let rows = both.json();
    let rows = rows.as_array().cloned().unwrap_or_default();
    assert_eq!(
        rows.len(),
        1,
        "both spellings of the same key narrow to one workitem: {}",
        both.body_text()
    );
    assert_eq!(
        rows[0]
            .obj_get("00080018")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some(first),
        "and the match is the workitem whose station is STATION01"
    );
}

#[test]
fn the_station_code_alias_works_through_the_post_search_body() {
    // Same alias, on the POST ?search path - which the server routes onto
    // the identical query struct, so a divergence here would be a
    // divergence in the parser rather than in the matcher.
    let srv = TestServer::start();
    let uid = WORKITEM_A;
    srv.create(
        uid,
        workitem_body_with_station_code(uid, "STATION01").as_bytes(),
    )
    .status();

    let body =
        br#"{"00404025":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["STATION01"]}}]}}"#;
    let r = srv.post_json("/dicomweb/workitems?search", body);
    assert_eq!(r.status(), 200, "POST ?search accepts the code key");
    assert_eq!(
        r.json().as_array().map(Vec::len).unwrap_or(0),
        1,
        "{}",
        r.body_text()
    );
}

// -- Sprint A / A5, A5b: the internal requestor's identity -------

/// A create body with the patient identity and the scheduled workitem code
/// the internal requestor now sends.
fn workitem_body_with_identity(uid: &str, issuer: &str, code: &str) -> String {
    format!(
        r#"{{"00404021":{{"vr":"SQ","Value":[{{"0020000D":{{"vr":"UI","Value":["{uid}"]}}}}]}},"00741000":{{"vr":"CS","Value":["SCHEDULED"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"LO","Value":["CAD"]}},"00404005":{{"vr":"DT","Value":["20260101120000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00100020":{{"vr":"LO","Value":["PAT-42"]}},"00100021":{{"vr":"LO","Value":["{issuer}"]}},"00404018":{{"vr":"SQ","Value":[{{"00080102":{{"vr":"SH","Value":["99CLARUS"]}},"00080104":{{"vr":"LO","Value":["AI detection"]}},"00080100":{{"vr":"SH","Value":["{code}"]}}}}]}}}}"#
    )
}

#[test]
fn the_requestor_identity_survives_the_store_and_the_wire() {
    // A5/A5b (T3). The server does not interpret (0010,0021) or (0040,4018):
    // it stores what the requester sent and hands it back, because AIW-I
    // 4.80.4.1.2.1 makes the ISSUER the requestor's obligation and
    // 4.84.4.1.2.1 has the performer echo the CODE. A server that dropped
    // either on the way through would make both impossible.
    let srv = TestServer::start();
    let uid = WORKITEM_A;
    srv.create(
        uid,
        workitem_body_with_identity(uid, "SITE-A", "CAD").as_bytes(),
    )
    .status();

    let r = srv.get(&format!("/dicomweb/workitems/{uid}"));
    assert_eq!(r.status(), 200);
    let j = r.json();
    assert_eq!(
        j.obj_get("00100021")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("SITE-A"),
        "the issuer is stored and returned verbatim: {}",
        r.body_text()
    );
    let code = j
        .obj_get("00404018")
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.first_of_array())
        .expect("(0040,4018) is present");
    assert_eq!(
        code.obj_get("00080100")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("CAD")
    );
    assert_eq!(
        code.obj_get("00080102")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("99CLARUS")
    );
    assert_eq!(
        code.obj_get("00080104")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("AI detection")
    );
}

#[test]
fn an_empty_issuer_is_returned_as_a_zero_length_element() {
    // AIW-I 4.80.4.1.2.1: a Patient ID is always accompanied by an issuer,
    // and the standard's way of saying "no issuer at this site" is a
    // zero-length value - NOT an absent element. Both would read as "no
    // issuer" to a lenient reader, but only one of them is conformant.
    let srv = TestServer::start();
    let uid = WORKITEM_A;
    srv.create(uid, workitem_body_with_identity(uid, "", "CAD").as_bytes())
        .status();

    let body = srv.get(&format!("/dicomweb/workitems/{uid}")).body_text();
    // PS3.18 F.2.5: zero length is stated by OMITTING `Value`. The element
    // itself stays, which is what makes it a zero-length attribute rather
    // than an absent one - but it carries no empty-string value.
    assert!(
        body.contains(r#""00100021":{"vr":"LO"}"#),
        "the issuer element is present and zero-length: {body}"
    );
    assert!(
        !body.contains(r#""00100021":{"Value":[""]"#),
        "zero length must not be written as one empty value: {body}"
    );
}

#[test]
fn a_workitem_with_no_patient_id_carries_no_issuer() {
    // The converse: an issuer for a patient the workitem does not name
    // would be an attribute about nothing. The rule is "an issuer for every
    // Patient ID present", so no Patient ID means no issuer element at all.
    let srv = TestServer::start();
    let uid = WORKITEM_A;
    let body = format!(
        r#"{{"00404021":{{"vr":"SQ","Value":[{{"0020000D":{{"vr":"UI","Value":["{uid}"]}}}}]}},"00741000":{{"vr":"CS","Value":["SCHEDULED"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"LO","Value":["CT-CHEST"]}},"00404005":{{"vr":"DT","Value":["20260101120000"]}},"00404041":{{"vr":"CS","Value":["READY"]}}}}"#
    );
    srv.create(uid, body.as_bytes()).status();

    let body = srv.get(&format!("/dicomweb/workitems/{uid}")).body_text();
    assert!(
        !body.contains("00100021"),
        "no Patient ID means no Issuer of Patient ID: {body}"
    );
    assert!(!body.contains("00100020"), "and no Patient ID either");
}

// -- 0110: the two UPS matching keys of PS3.4 Table CC.2.5-3 -------------

/// A create body that names its study in (0040,4021) and carries an
/// Accession Number in (0008,0050), which is the pair the two new matching
/// keys address.
///
/// The workitem UID is not here: PS3.18 Table 11.4.1-1 puts it in the request
/// QUERY string, which is where `TestServer::create` writes it.
fn workitem_body_with_accession(study_uid: &str, accession: &str) -> String {
    format!(
        r#"{{"00404021":{{"vr":"SQ","Value":[{{"0020000D":{{"vr":"UI","Value":["{study_uid}"]}}}}]}},"00080050":{{"vr":"SH","Value":["{accession}"]}},"00741000":{{"vr":"CS","Value":["SCHEDULED"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"LO","Value":["CT-CHEST"]}},"00404005":{{"vr":"DT","Value":["20260101120000"]}},"00404041":{{"vr":"CS","Value":["READY"]}}}}"#
    )
}

/// Count the workitems a search returned.
fn hits(srv: &TestServer, query: &str) -> usize {
    let r = srv.get(&format!("/dicomweb/workitems?{query}"));
    assert_eq!(r.status(), 200, "search {query} must be 200");
    r.json().as_array().map(|a| a.len()).unwrap_or(0)
}

/// The Accession Number key (0040,A370 > 0008,0050) selects the workitem
/// that carries the value, and only it.
#[test]
fn search_by_accession_number_selects_the_matching_workitem() {
    let srv = TestServer::start();
    let study_a = "1.2.826.0.1.3680043.10.1234.9201";
    let study_b = "1.2.826.0.1.3680043.10.1234.9202";

    srv.create(
        "1.2.826.0.1.3680043.10.1234.8001",
        workitem_body_with_accession(study_a, "ACC-AAA").as_bytes(),
    )
    .status();
    srv.create(
        "1.2.826.0.1.3680043.10.1234.8002",
        workitem_body_with_accession(study_b, "ACC-BBB").as_bytes(),
    )
    .status();

    // The standard's sequence-qualified spelling.
    assert_eq!(
        hits(&srv, "0040A370.00080050=ACC-AAA"),
        1,
        "the standard's qualified key must select exactly the matching workitem"
    );
    // The bare attribute spelling reaches the same field.
    assert_eq!(
        hits(&srv, "00080050=ACC-AAA"),
        1,
        "the bare tag spelling must reach the same key"
    );
    assert_eq!(hits(&srv, "0040A370.00080050=ACC-BBB"), 1);
    // A value nobody carries matches nothing.
    assert_eq!(hits(&srv, "0040A370.00080050=ACC-NONE"), 0);
    // And the search really is narrow: the other workitem is not swept in.
    let body = srv
        .get("/dicomweb/workitems?0040A370.00080050=ACC-AAA")
        .body_text();
    assert!(
        body.contains("8001"),
        "the matching workitem is returned: {body}"
    );
    assert!(
        !body.contains("8002"),
        "the non-matching workitem must not be returned: {body}"
    );
}

/// RED FACT. A search whose keys are the PS3.6 KEYWORDS (what PS3.18 8.3.4
/// defines and a conformant user agent sends) filters exactly like the hex
/// spellings, and does NOT return the unfiltered worklist.
///
/// Before the aliases a keyword key fell through to the parser's `_` arm and
/// set nothing, so `?PatientID=X` answered with EVERY workitem - a silent
/// privacy leak, not an error.
#[test]
fn search_by_keyword_keys_filters_like_the_hex_spellings() {
    let srv = TestServer::start();
    let study_a = "1.2.826.0.1.3680043.10.1234.9401";
    let study_b = "1.2.826.0.1.3680043.10.1234.9402";

    // Two workitems whose patient + accession differ, so a key that filtered
    // nothing would return both.
    srv.create(
        "1.2.826.0.1.3680043.10.1234.8001",
        workitem_body_with_accession(study_a, "ACC-KW1").as_bytes(),
    )
    .status();
    srv.create(
        "1.2.826.0.1.3680043.10.1234.8002",
        workitem_body_with_accession(study_b, "ACC-KW2").as_bytes(),
    )
    .status();

    // The keyword spellings of the two keys the nroduit rule uses.
    assert_eq!(
        hits(&srv, "AccessionNumber=ACC-KW1"),
        1,
        "the keyword accession key must narrow the search"
    );
    assert_eq!(
        hits(&srv, "ReferencedRequestSequence.AccessionNumber=ACC-KW1"),
        1,
        "and so must the sequence-qualified keyword form"
    );
    assert_eq!(
        hits(&srv, &format!("StudyInstanceUID={study_a}")),
        1,
        "the keyword study key narrows to one study"
    );

    // Keyword and hex give the SAME answer, which is the property that makes
    // the alias safe to add.
    let keyword = srv
        .get("/dicomweb/workitems?ReferencedRequestSequence.AccessionNumber=ACC-KW1")
        .body_text();
    let hex = srv
        .get("/dicomweb/workitems?0040A370.00080050=ACC-KW1")
        .body_text();
    assert_eq!(
        keyword, hex,
        "keyword and hex results must be byte-identical"
    );

    // The leak this test exists for: a keyword search must NOT return the
    // other workitem.
    assert!(keyword.contains("8001"), "the match is returned: {keyword}");
    assert!(
        !keyword.contains("8002"),
        "the non-match must not be swept in: {keyword}"
    );

    // A wrong-case keyword is not the standard's spelling: PS3.18 8.3.4 fixes
    // the keys, so the request is refused rather than answered. Answering with
    // the unfiltered list would hide the misspelling behind a broad result.
    let wrong_case = srv.get("/dicomweb/workitems?accessionnumber=ACC-KW1");
    assert_eq!(
        wrong_case.status(),
        400,
        "wrong case is a malformed key, not a broadening filter"
    );
    assert!(
        wrong_case.body_text().contains("accessionnumber"),
        "the refusal names the key: {}",
        wrong_case.body_text()
    );
}

/// RED FACT. A search carrying an unknown or misspelled query key is refused
/// with 400, the way dcm4chee-arc refuses it ("Invalid Query Parameter").
///
/// The parser used to ignore such a key, so `?PatientID=X` (right key) and
/// `?patientid=X` (wrong case) were answered differently only by accident: the
/// first narrowed, the second returned EVERY workitem with a 200. A caller
/// cannot tell those apart, which makes a typo a silent disclosure.
#[test]
fn search_with_an_unknown_query_key_is_refused() {
    let srv = TestServer::start();
    let study_a = "1.2.826.0.1.3680043.10.1234.9501";
    let study_b = "1.2.826.0.1.3680043.10.1234.9502";

    srv.create(
        "1.2.826.0.1.3680043.10.1234.8201",
        workitem_body_with_accession(study_a, "ACC-RJ1").as_bytes(),
    )
    .status();
    srv.create(
        "1.2.826.0.1.3680043.10.1234.8202",
        workitem_body_with_accession(study_b, "ACC-RJ2").as_bytes(),
    )
    .status();

    // The key that would have been dropped, in three shapes: an unknown name, a
    // wrong-case real keyword, and a wrong-case sequence path. All three are
    // malformed matching keys, not filters.
    //
    // The control parameters of PS3.18 Table 8.3.4-1 are deliberately NOT in
    // this list: the table marks them M (mandatory) for the origin server, so
    // `includefield=all` is a legal request (see the assertion below).
    for query in [
        "Foo=1",
        "patientid=ACC-RJ1",
        "ReferencedRequestSequence.Accessionnumber=ACC-RJ1",
    ] {
        let r = srv.get(&format!("/dicomweb/workitems?{query}"));
        assert_eq!(r.status(), 400, "{query} must be refused");
        let body = r.body_text();
        assert!(
            !body.contains("8201") && !body.contains("8202"),
            "{query} must not answer with workitems: {body}"
        );
    }

    // The refusal names the key it did not understand, so the client can find
    // its own typo in a query set it may have built programmatically.
    let r = srv.get("/dicomweb/workitems?Foo=1");
    assert!(
        r.body_text().contains("Foo"),
        "the message names the unknown key: {}",
        r.body_text()
    );

    // A good key beside a bad one is still malformed: the bad key must not ride
    // along with a working filter.
    assert_eq!(
        srv.get("/dicomweb/workitems?AccessionNumber=ACC-RJ1&Foo=1")
            .status(),
        400,
        "one unknown key refuses the whole request"
    );

    // The paging keys keep working, alone and beside a matching key.
    assert_eq!(hits(&srv, "limit=1"), 1, "limit still pages");
    assert_eq!(hits(&srv, "offset=1"), 1, "offset still pages");
    // PS3.18 Table 8.3.4-1 marks includefield and fuzzymatching M for the
    // origin server, so a conformant user agent's search is answered, not
    // refused - and the filter beside them still narrows.
    assert_eq!(
        hits(&srv, "includefield=all"),
        2,
        "includefield is mandatory for the origin server"
    );
    assert_eq!(
        srv.get("/dicomweb/workitems?AccessionNumber=ACC-RJ1&includefield=all&fuzzymatching=false")
            .status(),
        200,
        "the control parameters ride beside a real filter"
    );
    assert_eq!(
        hits(&srv, "AccessionNumber=ACC-RJ1&limit=1"),
        1,
        "a matching key beside the paging keys still filters"
    );
    // And an empty query is still a valid unfiltered search.
    assert_eq!(
        hits(&srv, ""),
        2,
        "a search with no keys returns everything"
    );
}

/// The Study Instance UID key (0040,4021 > 0020,000D) selects the workitems
/// whose INPUT study is that UID. This is the key Weasis filters on.
#[test]
fn search_by_study_uid_selects_that_studys_workitems() {
    let srv = TestServer::start();
    let study_a = "1.2.826.0.1.3680043.10.1234.9301";
    let study_b = "1.2.826.0.1.3680043.10.1234.9302";

    for (uid, study) in [
        ("1.2.826.0.1.3680043.10.1234.8101", study_a),
        ("1.2.826.0.1.3680043.10.1234.8102", study_a),
        ("1.2.826.0.1.3680043.10.1234.8103", study_b),
    ] {
        srv.create(uid, workitem_body_with_accession(study, "ACC-X").as_bytes())
            .status();
    }

    let body = srv
        .get(&format!("/dicomweb/workitems?0020000D={study_a}"))
        .body_text();
    assert!(
        body.contains("8101") && body.contains("8102"),
        "both of study A's: {body}"
    );
    assert!(!body.contains("8103"), "study B's must not appear: {body}");
}

/// An absent or empty Accession Number key matches everything, which is the
/// DICOMweb convention every other key on this path follows.
#[test]
fn an_empty_accession_number_key_matches_everything() {
    let srv = TestServer::start();
    let study = "1.2.826.0.1.3680043.10.1234.9401";
    srv.create(
        "1.2.826.0.1.3680043.10.1234.8201",
        workitem_body_with_accession(study, "ACC-1").as_bytes(),
    )
    .status();
    srv.create(
        "1.2.826.0.1.3680043.10.1234.8202",
        workitem_body_with_accession(study, "ACC-2").as_bytes(),
    )
    .status();

    assert_eq!(hits(&srv, ""), 2, "no key at all matches everything");
    assert_eq!(
        hits(&srv, "0040A370.00080050="),
        2,
        "a zero-length value matches everything, like every other key"
    );
}

/// The POST `?search` body path carries the same key, so a DICOMweb client
/// that searches by body is not a second-class caller.
#[test]
fn the_accession_key_works_through_the_post_search_body() {
    let srv = TestServer::start();
    let uid = "1.2.826.0.1.3680043.10.1234.8301";
    let study = "1.2.826.0.1.3680043.10.1234.9501";
    srv.create(
        uid,
        workitem_body_with_accession(study, "ACC-POST").as_bytes(),
    )
    .status();

    // The bare tag, because a JSON object key cannot express the sequence
    // path the same way a query parameter name does.
    let body = br#"{"00080050":{"vr":"SH","Value":["ACC-POST"]}}"#;
    let r = srv.post_json("/dicomweb/workitems?search", body);
    assert_eq!(r.status(), 200, "POST ?search by accession must be 200");
    let arr = r.json();
    assert_eq!(
        arr.as_array().map(|a| a.len()).unwrap_or(0),
        1,
        "the POST body key must select the workitem too"
    );

    // And it agrees with the GET form byte-for-byte, like every other key.
    let get_arr = srv.get("/dicomweb/workitems?00080050=ACC-POST").json();
    assert_eq!(arr, get_arr, "POST and GET search agree");
}
