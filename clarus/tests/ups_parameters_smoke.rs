// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Freeze the Scheduled Processing Parameters carriage and the Coding Scheme
//! Designator check on the UPS-RS worklist.
//!
//! Three behaviours are pinned here:
//!
//! - (0008,0102) in the Scheduled Workitem Code Sequence is checked against
//!   the schemes the deployment declared (`[ups.coding_schemes]`), because a
//!   code whose scheme nobody knows cannot be matched by any performer
//!   (PS3.3 8.2).
//! - (0074,1210) Scheduled Processing Parameters Sequence is stored and
//!   returned unchanged: its items are Content Items (PS3.3 Table C.30.2-1)
//!   the server does not interpret, so the VALUE subtree is what has to
//!   survive the roundtrip.
//! - The Update transaction (PS3.18 11.6) writes the label, the priority,
//!   the parameter list and the requested-procedure reference, and refuses
//!   the nested shapes it cannot round-trip.
//!
//! Boots the real server over TCP so the status codes are the ones a client
//! would see.

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

// -- Test server harness -------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    _temp: tempfile::TempDir,
}

impl TestServer {
    /// Boot a server whose `[ups.coding_schemes]` lists exactly `schemes`.
    fn start_with_schemes(schemes: &[&str]) -> Self {
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
        config.ups.coding_schemes = schemes.iter().map(|s| (*s).to_string()).collect();

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
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, state, sd, timeouts, u32::MAX);
        });

        Self { addr, _temp: temp }
    }

    /// A server that has declared no scheme of its own - the pre-existing
    /// deployment shape, where only `99`-prefixed designators pass.
    fn start() -> Self {
        Self::start_with_schemes(&[])
    }

    fn create_json(&self, workitem_uid: &str, body: &[u8]) -> Response {
        self.post_json(
            &format!("/dicomweb/workitems?workitem={workitem_uid}"),
            body,
        )
    }

    fn get_json(&self, workitem_uid: &str) -> Response {
        self.get(&format!("/dicomweb/workitems/{workitem_uid}"), &[])
    }

    fn update_json(&self, workitem_uid: &str, transaction_uid: &str, body: &[u8]) -> Response {
        self.post_json(
            &format!("/dicomweb/workitems/{workitem_uid}?TransactionUid={transaction_uid}"),
            body,
        )
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        self.request("GET", path, headers, None)
    }

    fn post_json(&self, path: &str, body: &[u8]) -> Response {
        self.request(
            "POST",
            path,
            &[("Content-Type", "application/json")],
            Some(body),
        )
    }

    fn put_json(&self, path: &str, body: &[u8]) -> Response {
        self.request(
            "PUT",
            path,
            &[("Content-Type", "application/json")],
            Some(body),
        )
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
            "expected {expected}, body was {}",
            String::from_utf8_lossy(&self.body)
        );
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> JsonVal {
        parse_json(&self.body).expect("json body")
    }
}

// -- Body builders -------------------------------------------------------

const WORKITEM: &str = "1.2.826.0.1.3680043.10.1234.9101";
const CLAIM_TUID: &str = "2.25.3333333333333333333";

/// A create body with the type-1 set of PS3.4 Table CC.2.5-3 and a
/// Scheduled Workitem Code Sequence whose designator is `scheme`.
///
/// A designator of `""` sends the item with the designator element present
/// but empty, which is what a client that "has no scheme" looks like on the
/// wire.
fn workitem_body(scheme: &str) -> Vec<u8> {
    format!(
        r#"{{"00404005":{{"vr":"DT","Value":["20260917T103000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"SH","Value":["PARAMS"]}},"00404018":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["CV1"]}},"00080102":{{"vr":"SH","Value":["{scheme}"]}},"00080104":{{"vr":"LO","Value":["some code"]}}}}]}}}}"#
    )
    .into_bytes()
}

/// A create body carrying a two-item (0074,1210) Scheduled Processing
/// Parameters Sequence, each item a Content Item (PS3.3 Table C.30.2-1).
fn workitem_body_with_parameters() -> Vec<u8> {
    br#"{"00404005":{"vr":"DT","Value":["20260917T103000"]},"00404041":{"vr":"CS","Value":["READY"]},"00741200":{"vr":"CS","Value":["MEDIUM"]},"00741204":{"vr":"SH","Value":["PARAMS"]},"00741210":{"vr":"SQ","Value":[{"0040A043":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["113100"]},"00080102":{"vr":"SH","Value":["DCM"]},"00080104":{"vr":"LO","Value":["WindowCenter"]}}]},"0040A040":{"vr":"CS","Value":["TEXT"]},"0040A160":{"vr":"UT","Value":["400"]}},{"0040A043":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["113101"]},"00080102":{"vr":"SH","Value":["DCM"]},"00080104":{"vr":"LO","Value":["WindowWidth"]}}]},"0040A040":{"vr":"CS","Value":["NUMERIC"]},"0040A30A":{"vr":"DS","Value":["40.5"]}}]}}"#
        .to_vec()
}

fn claim_body(state: &str, transaction_uid: &str) -> Vec<u8> {
    format!(
        r#"{{"00741000":{{"vr":"CS","Value":["{state}"]}},"00081195":{{"vr":"UI","Value":["{transaction_uid}"]}}}}"#
    )
    .into_bytes()
}

/// Claim the workitem and return the Transaction UID every later Update has
/// to echo.
fn claim(srv: &TestServer) {
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM}/state"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    )
    .assert_status(200);
}

// -- Helpers -------------------------------------------------------------

fn tag_value<'a>(j: &'a JsonVal, tag: &str) -> Option<&'a str> {
    j.obj_get(tag)?.obj_get("Value")?.as_str_array_first()
}

// -- Coding Scheme Designator -------------------------------------------

#[test]
fn a_private_scheme_is_accepted_without_configuration() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
}

#[test]
fn the_local_l_designator_is_accepted_without_configuration() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("L"))
        .assert_status(201);
}

#[test]
fn a_declared_scheme_is_accepted() {
    let srv = TestServer::start_with_schemes(&["LN", "SCT"]);
    srv.create_json(WORKITEM, &workitem_body("LN"))
        .assert_status(201);
}

#[test]
fn an_undeclared_scheme_is_refused_by_name() {
    let srv = TestServer::start();
    let resp = srv.create_json(WORKITEM, &workitem_body("XYZ_UNKNOWN"));
    resp.assert_status(400);
    let text = resp.text();
    assert!(
        text.contains("XYZ_UNKNOWN"),
        "the failure has to name the designator it refused, got: {text}"
    );
}

#[test]
fn an_empty_designator_is_refused() {
    let srv = TestServer::start();
    let resp = srv.create_json(WORKITEM, &workitem_body(""));
    resp.assert_status(400);
    let text = resp.text();
    assert!(
        text.contains("0008,0102"),
        "the failure has to name the missing designator, got: {text}"
    );
}

// -- Scheduled Processing Parameters carriage ---------------------------

#[test]
fn parameters_roundtrip_unchanged() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body_with_parameters())
        .assert_status(201);

    let fetched = srv.get_json(WORKITEM);
    fetched.assert_status(200);
    let j = fetched.json();

    let items = j
        .obj_get("00741210")
        .and_then(|s| s.obj_get("Value"))
        .and_then(|v| v.as_array())
        .expect("(0074,1210) has to survive the roundtrip");
    assert_eq!(items.len(), 2, "both parameters have to come back");

    // The first item is the TEXT content item. Its Concept Name is nested one
    // level deeper (PS3.3 Table C.30.2-1 makes it a sequence), so reading it
    // through the path a consumer follows proves the nesting survived.
    let name = items[0]
        .obj_get("0040A043")
        .and_then(|s| s.obj_get("Value"))
        .and_then(|v| v.first_of_array())
        .and_then(|i| i.obj_get("00080104"))
        .and_then(|i| i.obj_get("Value"))
        .and_then(|v| v.as_str_array_first())
        .expect("the Concept Name Code Meaning has to survive");
    assert_eq!(name, "WindowCenter");

    assert_eq!(tag_value(&items[0], "0040A040"), Some("TEXT"));
    assert_eq!(tag_value(&items[0], "0040A160"), Some("400"));
    assert_eq!(tag_value(&items[1], "0040A040"), Some("NUMERIC"));
    assert_eq!(tag_value(&items[1], "0040A30A"), Some("40.5"));

    // The stored subtree is handed back byte-for-byte: the render appends the
    // serialized value verbatim, so the Value subtree keeps the document
    // order the requester wrote.
    let raw = fetched.text();
    let position_a043 = raw.find("0040A043").expect("first child present");
    let position_a160 = raw.find("0040A160").expect("second child present");
    assert!(
        position_a043 < position_a160,
        "the subtree keeps the requester's key order"
    );
}

#[test]
fn a_workitem_without_parameters_renders_none() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    let j = srv.get_json(WORKITEM).json();
    assert!(
        j.obj_get("00741210").is_none(),
        "an absent parameter list must not be invented"
    );
}

#[test]
fn parameters_appear_in_a_search_response() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body_with_parameters())
        .assert_status(201);

    let resp = srv.get("/dicomweb/workitems?00741000=SCHEDULED", &[]);
    resp.assert_status(200);
    let text = resp.text();
    assert!(
        text.contains("WindowCenter"),
        "the search resource renders the same workitem, so the parameter has \
         to be there, got: {text}"
    );
}

// -- Update the label, the priority and the parameters -------------------

fn update_label_body(label: &str) -> Vec<u8> {
    format!(r#"{{"00741204":{{"vr":"SH","Value":["{label}"]}}}}"#).into_bytes()
}

fn update_priority_body(priority: &str) -> Vec<u8> {
    format!(r#"{{"00741200":{{"vr":"CS","Value":["{priority}"]}}}}"#).into_bytes()
}

fn update_parameters_body() -> Vec<u8> {
    br#"{"00741210":{"vr":"SQ","Value":[{"0040A040":{"vr":"CS","Value":["TEXT"]},"0040A160":{"vr":"UT","Value":["revised"]}}]}}"#.to_vec()
}

#[test]
fn update_replaces_the_label() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    srv.update_json(WORKITEM, CLAIM_TUID, &update_label_body("RENAMED"))
        .assert_status(200);
    let j = srv.get_json(WORKITEM).json();
    assert_eq!(tag_value(&j, "00741204"), Some("RENAMED"));
}

#[test]
fn update_refuses_an_empty_label() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    let resp = srv.update_json(WORKITEM, CLAIM_TUID, &update_label_body(""));
    resp.assert_status(400);
    assert!(
        resp.text().contains("00741204"),
        "the failure has to name the label element, got: {}",
        resp.text()
    );
}

#[test]
fn update_replaces_the_priority() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    srv.update_json(WORKITEM, CLAIM_TUID, &update_priority_body("HIGH"))
        .assert_status(200);
    let j = srv.get_json(WORKITEM).json();
    assert_eq!(tag_value(&j, "00741200"), Some("HIGH"));
}

#[test]
fn update_refuses_an_unknown_priority() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    srv.update_json(WORKITEM, CLAIM_TUID, &update_priority_body("URGENT"))
        .assert_status(400);
}

// -- Update the Scheduled Workitem Code Sequence ------------------------

/// An Update body carrying (0040,4018) with the designator `scheme`.
///
/// Table CC.2.5-3 types the sequence 3/1 at N-SET (the SCP SHALL handle its
/// update), so the scheme inside it is checked exactly as on Create.
fn update_workitem_code_body(scheme: &str) -> Vec<u8> {
    format!(
        r#"{{"00404018":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["CV2"]}},"00080102":{{"vr":"SH","Value":["{scheme}"]}},"00080104":{{"vr":"LO","Value":["revised code"]}}}}]}}}}"#
    )
    .into_bytes()
}

#[test]
fn update_accepts_a_declared_scheduled_workitem_code() {
    let srv = TestServer::start_with_schemes(&["LN"]);
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    srv.update_json(WORKITEM, CLAIM_TUID, &update_workitem_code_body("LN"))
        .assert_status(200);

    let j = srv.get_json(WORKITEM).json();
    let code = j
        .obj_get("00404018")
        .and_then(|s| s.obj_get("Value"))
        .and_then(|v| v.first_of_array())
        .expect("the updated code is stored");
    assert_eq!(
        tag_value(code, "00080102"),
        Some("LN"),
        "the updated code replaces the created one"
    );
}

#[test]
fn update_refuses_an_unknown_scheduled_workitem_code_scheme() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    let resp = srv.update_json(
        WORKITEM,
        CLAIM_TUID,
        &update_workitem_code_body("XYZ_UNKNOWN"),
    );
    resp.assert_status(400);
    assert!(
        resp.text().contains("XYZ_UNKNOWN"),
        "the failure has to name the designator it refused, got: {}",
        resp.text()
    );
}

#[test]
fn update_refuses_an_empty_scheduled_workitem_code_sequence() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    let resp = srv.update_json(
        WORKITEM,
        CLAIM_TUID,
        br#"{"00404018":{"vr":"SQ","Value":[]}}"#,
    );
    resp.assert_status(400);
    assert!(
        resp.text().contains("0040,4018"),
        "the failure has to name the sequence it refused, got: {}",
        resp.text()
    );
}

#[test]
fn update_replaces_the_parameters() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body_with_parameters())
        .assert_status(201);
    claim(&srv);

    srv.update_json(WORKITEM, CLAIM_TUID, &update_parameters_body())
        .assert_status(200);
    let text = srv.get_json(WORKITEM).text();
    assert!(
        text.contains("revised"),
        "the replacing list has to be the stored one, got: {text}"
    );
    assert!(
        !text.contains("40.5"),
        "a whole-sequence replace must not keep the old items, got: {text}"
    );
}

#[test]
fn update_can_set_an_empty_parameter_list() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body_with_parameters())
        .assert_status(201);
    claim(&srv);

    // Table CC.2.5-3 types the sequence 2 at N-SET, and a type 2 attribute
    // sent with no items says "this workitem carries no parameters".
    srv.update_json(
        WORKITEM,
        CLAIM_TUID,
        br#"{"00741210":{"vr":"SQ","Value":[]}}"#,
    )
    .assert_status(200);
    let j = srv.get_json(WORKITEM).json();
    let items = j
        .obj_get("00741210")
        .and_then(|s| s.obj_get("Value"))
        .and_then(|v| v.as_array())
        .expect("the empty sequence is still a sequence");
    assert!(items.is_empty(), "the list has to be empty now");
}

// -- The requested-procedure reference -----------------------------------

#[test]
fn update_replaces_the_requested_procedure_reason() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    srv.update_json(
        WORKITEM,
        CLAIM_TUID,
        br#"{"0040A370":{"vr":"SQ","Value":[{"00401002":{"vr":"LO","Value":["rule out pneumonia"]}}]}}"#,
    )
    .assert_status(200);

    let text = srv.get_json(WORKITEM).text();
    assert!(
        text.contains("rule out pneumonia"),
        "the reason has to be readable back, got: {text}"
    );
}

#[test]
fn update_refuses_an_unsupported_nested_attribute() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    // (0040,1001) Requested Procedure ID is "Not allowed" at N-SET, so
    // accepting it would report a write that never lands. The failure names
    // the tag.
    let resp = srv.update_json(
        WORKITEM,
        CLAIM_TUID,
        br#"{"0040A370":{"vr":"SQ","Value":[{"00401001":{"vr":"SH","Value":["RP-1"]}}]}}"#,
    );
    resp.assert_status(400);
    assert!(
        resp.text().contains("00401001"),
        "the failure has to name the nested tag it refused, got: {}",
        resp.text()
    );
}

#[test]
fn update_refuses_a_reference_without_an_item() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);
    claim(&srv);

    srv.update_json(
        WORKITEM,
        CLAIM_TUID,
        br#"{"0040A370":{"vr":"SQ","Value":[]}}"#,
    )
    .assert_status(400);
}

// -- Subscribe filter ----------------------------------------------------

#[test]
fn a_subscription_cannot_filter_on_the_parameter_list() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM, &workitem_body("99CLARUS"))
        .assert_status(201);

    // PS3.18 11.7 lets a subscriber filter on the workitem attributes the
    // standard enumerates; a parameter list is not one of them, so the filter
    // key is unknown and the subscription is refused rather than silently
    // accepting a filter that would never match.
    let resp = srv.post_json(
        "/dicomweb/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLARUS_TEST?filter=00741210=anything",
        &[],
    );
    resp.assert_status(400);
    assert!(
        resp.text().contains("00741210"),
        "the failure has to name the unsupported key, got: {}",
        resp.text()
    );
}
