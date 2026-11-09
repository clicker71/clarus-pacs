// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! MWL Search (PS3.18 §14) and MPPS Create/Update/Retrieve (PS3.18 §15) over
//! the real dispatch loop.
//!
//! The suite is deliberately non-vacuous in both directions:
//!   - a NEGATIVE path: a search with a wrong-case key returns 400 naming the
//!     key (not an empty or unfiltered list), an N-CREATE with a status other
//!     than IN PROGRESS is refused, an N-SET to COMPLETED with an empty
//!     Performed Series Sequence is refused, an N-SET after COMPLETED is
//!     refused, and an N-SET on an unknown UID is refused;
//!   - a POSITIVE path: one SPS record seeded into `workflow/mwl/` is returned
//!     by the search, and after a successful MPPS Create a Get returns the
//!     stored record and the N-SET transition is visible on the next Get.
//!
//! A handler that always answered 400 would pass the negative half and fail
//! the positive one, which is what makes this a test rather than a fixture.

#![forbid(unsafe_code)]

use clarus_core::workflow::{MppsRecord, SpsRecord, WorkflowStore};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const MPPS_UID: &str = "1.2.826.0.1.3680043.10.1234.9701";

struct TestServer {
    addr: SocketAddr,
    workflow_dir: std::path::PathBuf,
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

        let workflow_dir = temp.path().join("workflow");
        let mut config = clarus_server::config::Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        // Turn the surface ON and point the store at this temp dir.
        config.workflow.enabled = true;
        config.workflow.dir = workflow_dir.to_string_lossy().to_string();

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

        Self {
            addr,
            workflow_dir,
            _temp: temp,
        }
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path, &[("Accept", "application/dicom+json")], None)
    }

    fn post_json(&self, path: &str, body: &str) -> Response {
        self.request(
            "POST",
            path,
            &[("Content-Type", "application/json")],
            Some(body.as_bytes()),
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
    #[allow(dead_code)] // parsed for completeness; no test reads a header here
    headers: std::collections::HashMap<String, Vec<String>>,
    body: Vec<u8>,
}

impl Response {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A minimal SPS record carrying the seven REQUIRED K.6-1 keys.
fn sps_record(uid: &str, patient_id: &str, modality: &str) -> SpsRecord {
    SpsRecord {
        sps_uid: uid.to_string(),
        station_aet: "CT01".to_string(),
        sps_start_date: "20261011".to_string(),
        sps_start_time: "090000".to_string(),
        modality: modality.to_string(),
        scheduled_performing_physician: "SMITH^JOHN".to_string(),
        patient_name: "IVANOV^IVAN".to_string(),
        patient_id: patient_id.to_string(),
        accession_number: Some("ACC-1".to_string()),
        ..Default::default()
    }
}

// -- MWL ------------------------------------------------------------------

#[test]
fn mwl_search_returns_a_seeded_step_and_refuses_an_unknown_key() {
    let s = TestServer::start();
    let store = WorkflowStore::new(s.workflow_dir.clone());
    store
        .put_sps(&sps_record("1.2.826.0.1.9.1", "PAT-1", "CT"))
        .expect("seed");

    // POSITIVE: the seeded step comes back.
    let resp = s.get("/dicomweb/modality-scheduled-procedure-steps?PatientID=PAT-1");
    assert_eq!(resp.status, 200, "body: {}", resp.text());
    let body = resp.text();
    assert!(
        body.contains("IVANOV^IVAN"),
        "the seeded step must be returned: {body}"
    );
    // The reply is SPS-level (14.4 returns "a list of matching scheduled
    // procedure steps"), so it must NOT be wrapped in an instance hierarchy.
    assert!(
        !body.contains("\"00080018\""),
        "an MWL reply is SPS-level, not an instance: {body}"
    );

    // And a search for a different patient returns nothing.
    let empty = s.get("/dicomweb/modality-scheduled-procedure-steps?PatientID=NOPE");
    assert_eq!(empty.status, 200);
    assert_eq!(empty.text(), "[]", "no match -> an empty list");

    // NEGATIVE: a wrong-case key is refused, naming the key.
    let bad = s.get("/dicomweb/modality-scheduled-procedure-steps?patientid=PAT-1");
    assert_eq!(bad.status, 400, "body: {}", bad.text());
    assert!(
        bad.text().contains("patientid"),
        "the message must name the bad key: {}",
        bad.text()
    );
}

#[test]
fn mwl_search_fuzzymatches_a_person_name() {
    let s = TestServer::start();
    let store = WorkflowStore::new(s.workflow_dir.clone());
    store
        .put_sps(&sps_record("1.2.826.0.1.9.10", "PAT-10", "CT"))
        .expect("seed");

    // "IVANOV IVN" is one edit from the stored "IVANOV^IVAN", and the `+`
    // form-decodes to a space that fuzzy matching folds with the stored `^`.
    let hit = s.get(
        "/dicomweb/modality-scheduled-procedure-steps?PatientName=IVANOV+IVN&fuzzymatching=true",
    );
    assert_eq!(hit.status, 200, "body: {}", hit.text());
    assert!(hit.text().contains("IVANOV^IVAN"), "got: {}", hit.text());

    // The same query without fuzzymatching is exact and returns nothing.
    let miss = s.get("/dicomweb/modality-scheduled-procedure-steps?PatientName=IVANOV+IVN");
    assert_eq!(miss.status, 200);
    assert_eq!(miss.text(), "[]", "exact only: {}", miss.text());

    // A typed space aligns with the stored separator under fuzzy matching.
    let spaced = s.get(
        "/dicomweb/modality-scheduled-procedure-steps?PatientName=IVANOV+IVAN&fuzzymatching=true",
    );
    assert_eq!(spaced.status, 200, "body: {}", spaced.text());
    assert!(
        spaced.text().contains("IVANOV^IVAN"),
        "got: {}",
        spaced.text()
    );
}

#[test]
fn mwl_has_no_create_transaction() {
    let s = TestServer::start();
    // PS3.18 14.3: no Create exists in DIMSE or DICOMweb.
    let resp = s.post_json("/dicomweb/modality-scheduled-procedure-steps", "[]");
    assert_eq!(resp.status, 404, "body: {}", resp.text());
}

// -- MPPS -----------------------------------------------------------------

/// A valid N-CREATE body: status IN PROGRESS (Type 1).
const CREATE_BODY: &str = r#"[{"00400252":{"vr":"CS","Value":["IN PROGRESS"]},
                             "00080060":{"vr":"CS","Value":["CT"]},
                             "00400241":{"vr":"AE","Value":["CT01"]}}]"#;

#[test]
fn mpps_create_get_update_roundtrips() {
    let s = TestServer::start();

    // CREATE.
    let created = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"),
        CREATE_BODY,
    );
    assert_eq!(created.status, 201, "body: {}", created.text());

    // GET: the stored record is visible and carries the status.
    let got = s.get(&format!(
        "/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"
    ));
    assert_eq!(got.status, 200, "body: {}", got.text());
    let body = got.text();
    assert!(body.contains("IN PROGRESS"), "got: {body}");
    assert!(body.contains(MPPS_UID), "the UID is the identity: {body}");

    // UPDATE to COMPLETED, with a non-empty Performed Series Sequence (F.7).
    let completed = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}?update"),
        r#"[{"00400252":{"vr":"CS","Value":["COMPLETED"]},
             "00400250":{"vr":"DA","Value":["20261011"]},
             "00400251":{"vr":"TM","Value":["093000"]},
             "00400340":{"vr":"SQ","Value":[
               {"00081150":{"vr":"UI","Value":["1.2.840.10008.5.1.4.1.1.2"]},
                "00081155":{"vr":"UI","Value":["1.2.3.4"]}}]}}]"#,
    );
    assert_eq!(completed.status, 200, "body: {}", completed.text());

    // GET: the transition is visible.
    let after = s.get(&format!(
        "/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"
    ));
    assert!(after.text().contains("COMPLETED"), "got: {}", after.text());
    assert!(after.text().contains("1.2.3.4"), "got: {}", after.text());
}

#[test]
fn mpps_create_requires_in_progress_status() {
    let s = TestServer::start();
    // (0040,0252) is Type 1 at N-CREATE and must be IN PROGRESS (F.7.2.1).
    let resp = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"),
        r#"[{"00400252":{"vr":"CS","Value":["COMPLETED"]}}]"#,
    );
    assert_eq!(resp.status, 400, "body: {}", resp.text());
    assert!(
        resp.text().contains("IN PROGRESS"),
        "the message must state the rule: {}",
        resp.text()
    );

    // A missing status is equally refused.
    let missing = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"),
        r#"[{"00080060":{"vr":"CS","Value":["CT"]}}]"#,
    );
    assert_eq!(missing.status, 400, "body: {}", missing.text());
}

#[test]
fn mpps_create_refuses_a_duplicate_uid() {
    let s = TestServer::start();
    let first = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"),
        CREATE_BODY,
    );
    assert_eq!(first.status, 201);
    let second = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"),
        CREATE_BODY,
    );
    assert_eq!(second.status, 409, "body: {}", second.text());
    assert!(
        second.text().contains("0111H"),
        "the Duplicate SOP Instance code is named: {}",
        second.text()
    );
}

#[test]
fn mpps_update_to_final_requires_a_non_empty_series() {
    let s = TestServer::start();
    s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"),
        CREATE_BODY,
    );
    // An empty Performed Series Sequence at COMPLETED is refused (F.7).
    let resp = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}?update"),
        r#"[{"00400252":{"vr":"CS","Value":["COMPLETED"]},
             "00400340":{"vr":"SQ","Value":[]}}]"#,
    );
    assert_eq!(resp.status, 400, "body: {}", resp.text());
    assert!(
        resp.text().contains("0040,0340"),
        "the message must name the sequence: {}",
        resp.text()
    );
}

#[test]
fn mpps_update_after_a_final_state_is_refused() {
    let s = TestServer::start();
    s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"),
        CREATE_BODY,
    );
    // First advance to COMPLETED with a valid series.
    let completed = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}?update"),
        r#"[{"00400252":{"vr":"CS","Value":["COMPLETED"]},
             "00400340":{"vr":"SQ","Value":[
               {"00081150":{"vr":"UI","Value":["1.2.3"]},
                "00081155":{"vr":"UI","Value":["4.5.6"]}}]}}]"#,
    );
    assert_eq!(completed.status, 200, "body: {}", completed.text());

    // Any further N-SET is refused: the state machine has no exit from final.
    let again = s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}?update"),
        r#"[{"00400252":{"vr":"CS","Value":["DISCONTINUED"]}}]"#,
    );
    assert_eq!(again.status, 400, "body: {}", again.text());
}

#[test]
fn mpps_update_on_unknown_uid_is_refused() {
    let s = TestServer::start();
    // No implicit create on the DICOMweb path.
    let resp = s.post_json(
        "/dicomweb/modality-performed-procedure-steps/1.2.3.4.5?update",
        r#"[{"00400252":{"vr":"CS","Value":["COMPLETED"]}}]"#,
    );
    assert_eq!(resp.status, 404, "body: {}", resp.text());
}

#[test]
fn mpps_get_on_unknown_uid_is_404() {
    let s = TestServer::start();
    let resp = s.get("/dicomweb/modality-performed-procedure-steps/9.8.7.6");
    assert_eq!(resp.status, 404, "body: {}", resp.text());
}

#[test]
fn the_mpps_store_holds_the_record_the_create_wrote() {
    let s = TestServer::start();
    s.post_json(
        &format!("/dicomweb/modality-performed-procedure-steps/{MPPS_UID}"),
        CREATE_BODY,
    );
    // Read the record straight off disk: proves the handler persisted it,
    // rather than keeping it in memory for the answer.
    let store = WorkflowStore::new(s.workflow_dir.clone());
    let rec: MppsRecord = store
        .find_mpps(MPPS_UID)
        .expect("read")
        .expect("the record is on disk");
    assert_eq!(rec.status, "IN PROGRESS");
    assert_eq!(rec.modality.as_deref(), Some("CT"));
}
