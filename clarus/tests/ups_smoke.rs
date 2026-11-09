// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Freeze UPS-RS worklist + SSE wire behaviour.
//!
//! Boots the server over a real TCP socket and exercises the
//! PS3.18 Section 11 UPS-RS endpoints - create, search, update, delete -
//! plus the Server-Sent Events subscription (`/events`). The non-standard
//! `/claim` endpoint is no longer exposed; the SCHEDULED -> IN PROGRESS
//! transition is driven by a state-change UPDATE instead (PS3.18 Section 11.6).
//!
//! The SSE event carries a monotonic `id:` and a wall-clock `timestamp`, so
//! the whole-body byte-exact bytes are not reproducible verbatim. Instead the
//! SSE scenario freezes the *framing* (the `event:`/`data:`/`id:` line layout
//! axum's `Sse` emits) and the StateChange JSON shape - the golden structure
//! against which the formatter is written.
//!
//! The same scenarios also run against the extracted core (W6,
//! `ups_pure.rs` CRUD) and MUST pass without edits.

#![forbid(unsafe_code)]

use clarus_server::config::Config;
use clarus_server::handlers::json_common::{JsonVal, parse_json};
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Test server harness (mirror of wado_smoke.rs) -----------------------

const WORKITEM_A: &str = "1.2.826.0.1.3680043.10.1234.8101";
const WORKITEM_B: &str = "1.2.826.0.1.3680043.10.1234.8102";
const UNKNOWN_WORKITEM: &str = "1.2.826.0.1.3680043.10.1234.9999";

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

        let mut config = Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

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

    /// Create a workitem the way PS3.18 11.4.1.1 defines it.
    ///
    /// Table 11.4.1-1 puts the Workitem UID in a QUERY parameter on the
    /// worklist resource (`/workitems?workitem={uid}`), NOT in a path
    /// segment. A `POST /workitems/{uid}` is the Update transaction (11.6),
    /// so a create sent to the old path would be answered as an update of a
    /// workitem that does not exist yet.
    fn create_json(&self, workitem_uid: &str, body: &[u8]) -> Response {
        self.post_json(
            &format!("/dicomweb/workitems?workitem={workitem_uid}"),
            body,
        )
    }

    /// Update workitem attributes the way PS3.18 11.6.1 defines it.
    ///
    /// POST on the workitem resource, with the Transaction UID as a QUERY
    /// parameter and the attribute set as the payload.
    fn update_json(&self, workitem_uid: &str, transaction_uid: &str, body: &[u8]) -> Response {
        self.post_json(
            &format!("/dicomweb/workitems/{workitem_uid}?TransactionUid={transaction_uid}"),
            body,
        )
    }

    fn delete(&self, path: &str) -> Response {
        self.request("DELETE", path, &[], None)
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
        if std::env::var_os("UPS_DEBUG").is_some() {
            eprintln!("DEBUG {method} {path} => {}", String::from_utf8_lossy(&buf));
        }
        parse_http(&buf)
    }

    /// Open a persistent SSE connection to `/events` and return a reader that
    /// accumulates raw bytes to a shared buffer for later inspection.
    fn open_sse(&self, workitem_uid: &str, out: Arc<StdRwLock<Vec<u8>>>) -> SseGuard {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .expect("read timeout");
        let head = format!(
            "GET /dicomweb/workitems/{workitem_uid}/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nAccept: text/event-stream\r\n\r\n",
            self.addr.port()
        );
        stream.write_all(head.as_bytes()).expect("write sse req");

        let keep = Arc::new(AtomicBool::new(true));
        let keep2 = keep.clone();
        let out2 = out.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut idle = 0u32;
            while keep2.load(Ordering::Relaxed) {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // Append straight into the shared buffer so the test
                        // can WAIT for the frame to arrive instead of racing
                        // against a fixed sleep (the old flake ~1/3).
                        out2.write().unwrap().extend_from_slice(&buf[..n]);
                        idle = 0;
                    }
                    // Read timeout - keep polling until told to stop (SSE
                    // stream stays open; there is no clean EOF).
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        idle += 1;
                        if idle > 200 {
                            // ~60s cap: never spin forever.
                            break;
                        }
                        continue;
                    }
                    Err(_) => break,
                }
            }
        });

        SseGuard { handle, keep }
    }
}

struct SseGuard {
    handle: std::thread::JoinHandle<()>,
    keep: Arc<AtomicBool>,
}

impl SseGuard {
    /// Close the reader thread and wait for it to flush.
    fn close(self) {
        self.keep.store(false, Ordering::Relaxed);
        let _ = self.handle.join();
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

fn decode_chunked(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let line_end = find_crlf(data, pos)?;
        let size_str = std::str::from_utf8(&data[pos..line_end]).ok()?;
        let size_str = size_str.split(';').next().unwrap_or(size_str).trim();
        let chunk_size = usize::from_str_radix(size_str, 16).ok()?;
        let data_start = line_end + 2;
        if chunk_size == 0 {
            return Some(out);
        }
        let chunk = data.get(data_start..data_start + chunk_size)?;
        out.extend_from_slice(chunk);
        pos = data_start + chunk_size + 2;
    }
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
    fn json(&self) -> JsonVal {
        parse_json(&self.body).expect("json body")
    }
}

// -- Workitem JSON body builders (DICOMweb Appendix F) -------------------

/// A create body carrying the type-1 set of PS3.4 Table CC.2.5-3.
///
/// There is no Modality: UPS defines no such attribute. The label defaults to
/// "CAD" so the push filter and the search key have something to match.
fn workitem_body(step_label: &str) -> Vec<u8> {
    format!(
        r#"{{"00404005":{{"vr":"DT","Value":["20260917T103000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"SH","Value":["{step_label}"]}},"00404025":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["STATION01"]}},"00080102":{{"vr":"SH","Value":["99CLARUS"]}},"00080104":{{"vr":"LO","Value":["AI station"]}}}}]}},"00100010":{{"vr":"PN","Value":[{{"Alphabetic":"UPS^PATIENT"}}]}},"00100020":{{"vr":"LO","Value":["UPSPAT01"]}}}}"#
    )
    .into_bytes()
}

/// A create body that also names the input study (PS3.4 CC.2.5-3 makes
/// (0040,4021) the only standard route from a workitem to its study).
fn workitem_body_with_study(step_label: &str, study_uid: &str) -> Vec<u8> {
    format!(
        r#"{{"00404005":{{"vr":"DT","Value":["20260917T103000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"SH","Value":["{step_label}"]}},"00404021":{{"vr":"SQ","Value":[{{"0040E020":{{"vr":"CS","Value":["DICOM"]}},"0020000D":{{"vr":"UI","Value":["{study_uid}"]}}}}]}}}}"#
    )
    .into_bytes()
}

/// A create body that names the study and the accession through the
/// Referenced Request Sequence (0040,A370) - the carriage the requester
/// (Weasis, following the nroduit rule) actually uses.
///
/// CC.2.5-3 makes Accession Number Required and Study Instance UID Optional
/// inside the sequence; a requester that follows the rule fills both.
fn workitem_body_with_referenced_request(
    step_label: &str,
    accession: &str,
    study_uid: &str,
) -> Vec<u8> {
    format!(
        r#"{{"00404005":{{"vr":"DT","Value":["20260917T103000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"SH","Value":["{step_label}"]}},"0040A370":{{"vr":"SQ","Value":[{{"00080050":{{"vr":"SH","Value":["{accession}"]}},"0020000D":{{"vr":"UI","Value":["{study_uid}"]}}}}]}}}}"#
    )
    .into_bytes()
}

fn state_change_body(new_state: &str) -> Vec<u8> {
    format!(r#"{{"00741000":{{"vr":"CS","Value":["{new_state}"]}}}}"#).into_bytes()
}

/// A state change carrying a Transaction UID - the standard shape (PS3.4
/// CC.2.1-1: both the state and the UID are type 1 in every Change State).
fn claim_body(new_state: &str, transaction_uid: &str) -> Vec<u8> {
    format!(
        r#"{{"00741000":{{"vr":"CS","Value":["{new_state}"]}},"00081195":{{"vr":"UI","Value":["{transaction_uid}"]}}}}"#
    )
    .into_bytes()
}

/// A state change naming a NON-claim transition (COMPLETED/CANCELED).
///
/// The same shape as [`claim_body`] - the standard's Change UPS State payload
/// does not vary with the target state - but named separately so a test that
/// is about completing reads as one, rather than reusing a helper called
/// "claim" for a transition that is not a claim.
fn state_change_body_with_uid(new_state: &str, transaction_uid: &str) -> Vec<u8> {
    claim_body(new_state, transaction_uid)
}

/// A cancel carrying the UID and the standard Reason For Cancellation.
fn cancel_body(transaction_uid: &str, reason: &str) -> Vec<u8> {
    format!(
        r#"{{"00741000":{{"vr":"CS","Value":["CANCELED"]}},"00081195":{{"vr":"UI","Value":["{transaction_uid}"]}},"00741238":{{"vr":"LT","Value":["{reason}"]}}}}"#
    )
    .into_bytes()
}

const CLAIM_TUID: &str = "2.25.1111111111111111111";
const OTHER_TUID: &str = "2.25.2222222222222222222";
const STUDY_A: &str = "1.2.826.0.1.3680043.10.1709.7";
/// A second study UID, used to prove which carriage WINS when a body carries
/// the study through both the bare/Input-Information route and the Referenced
/// Request Sequence.
const STUDY_BARE: &str = "1.2.826.0.1.3680043.10.1709.8";

/// Helper: the workitem identity at `tag.0008.0018 Value[0]`.
fn wi_uid(j: &JsonVal) -> Option<&str> {
    j.obj_get("00080018")?
        .obj_get("Value")?
        .as_str_array_first()
}

/// Helper: the state at `tag.0074.1000 Value[0]` on an object.
fn wi_state(j: &JsonVal) -> Option<&str> {
    j.obj_get("00741000")?
        .obj_get("Value")?
        .as_str_array_first()
}

/// Helper: the first value of an arbitrary tag, for presence assertions.
fn wi_tag<'a>(j: &'a JsonVal, tag: &str) -> Option<&'a str> {
    j.obj_get(tag)?.obj_get("Value")?.as_str_array_first()
}

/// Helper: the first item of a sequence attribute (`seq.Value[0]`).
fn wi_seq_item<'a>(j: &'a JsonVal, seq_tag: &str) -> Option<&'a JsonVal> {
    j.obj_get(seq_tag)?.obj_get("Value")?.first_of_array()
}

/// Helper: the one item of (0074,1216) Unified Procedure Step Performed
/// Procedure Sequence - where PS3.3 Table C.30.3-1 places the whole performer
/// report. `None` means the workitem carries no such item.
fn wi_nested_item<'a>(j: &'a JsonVal, seq_tag: &str) -> Option<&'a JsonVal> {
    wi_seq_item(j, seq_tag)
}

/// Helper: a tag read INSIDE the (0074,1216) item. Uses the path a consumer
/// follows, so a value that leaked to the top level would NOT be found here -
/// which is what makes a nesting assertion meaningful.
fn wi_performed_tag<'a>(j: &'a JsonVal, tag: &str) -> Option<&'a str> {
    wi_nested_item(j, "00741216")?
        .obj_get(tag)?
        .obj_get("Value")?
        .as_str_array_first()
}

/// Helper: the Code Value of a code sequence nested inside (0074,1216).
fn wi_performed_code_value<'a>(j: &'a JsonVal, tag: &str) -> Option<&'a str> {
    wi_nested_item(j, "00741216")?
        .obj_get(tag)?
        .obj_get("Value")?
        .first_of_array()?
        .obj_get("00080100")?
        .obj_get("Value")?
        .as_str_array_first()
}

/// Helper: the first value of a tag INSIDE the first item of a sequence.
///
/// The study and the accession are no longer top-level: Task 3 carries them
/// through (0040,A370) Referenced Request Sequence, so a test that wants
/// them reads the sequence.
fn wi_seq_tag<'a>(j: &'a JsonVal, seq_tag: &str, item_tag: &str) -> Option<&'a str> {
    wi_seq_item(j, seq_tag)?
        .obj_get(item_tag)?
        .obj_get("Value")?
        .as_str_array_first()
}

/// Claim helper: SCHEDULED -> IN PROGRESS with our own UID, which is what
/// every later transition then has to echo.
fn claim(srv: &TestServer, uid: &str) {
    srv.put_json(
        &format!("/dicomweb/workitems/{uid}/state"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    )
    .assert_status(200);
}

// -- Scenarios ----------------------------------------------------------

/// Wire layout of an axum `Sse` event (PS3.18 Section 11.8 / W3C SSE). Frozen so
/// the formatter can be written against it.
///
/// axum emits the fields in the order `event:` -> `id:` -> `data:`, separated by
/// a single `\n` (LF) per the W3C EventSource spec, with a blank line (`\n\n`)
/// terminating each event. We capture the shape up to the `id:`/`data:` markers.
const STATE_CHANGE_FRAMING: &str = "event: StateChange\nid: ";

/// A DICOM JSON body wrapped as the ARRAY of datasets PS3.18 F.2 defines.
fn as_f2_array(object_body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(object_body.len() + 2);
    out.push(b'[');
    out.extend_from_slice(object_body);
    out.push(b']');
    out
}

#[test]
fn ups_create_accepts_the_f2_array_body() {
    let srv = TestServer::start();

    // PS3.18 F.2: the DICOM JSON Model is an ARRAY of datasets, so a conformant
    // client sends `[{...}]`. This is the shape the Weasis UPS-RS client sends
    // and the one that used to be answered 400 "Missing required tag
    // (0074,1200)" while the tag was right there in the payload.
    let r = srv.create_json(WORKITEM_A, &as_f2_array(&workitem_body("CAD")));
    r.assert_status(201);
    let j = r.json();
    assert_eq!(wi_uid(&j), Some(WORKITEM_A));
    assert_eq!(wi_state(&j), Some("SCHEDULED"));
    assert_eq!(wi_tag(&j, "00741200"), Some("MEDIUM"));
    assert_eq!(wi_tag(&j, "00741204"), Some("CAD"));

    // The workitem is a normal one: the worklist search finds it.
    let listed = srv.get("/dicomweb/workitems?ProcedureStepState=SCHEDULED", &[]);
    listed.assert_status(200);
    assert!(
        format!("{:?}", listed.json()).contains(WORKITEM_A),
        "an F.2 array create must be searchable: {:?}",
        listed.json()
    );
}

#[test]
fn ups_create_still_accepts_the_object_body() {
    let srv = TestServer::start();

    // The bare object form dcm4chee-arc and the older writers here send. It
    // stays valid: accepting the array is an extension, not a replacement.
    let r = srv.create_json(WORKITEM_A, &workitem_body("CAD"));
    r.assert_status(201);
    assert_eq!(wi_tag(&r.json(), "00741200"), Some("MEDIUM"));
}

#[test]
fn ups_create_refuses_an_array_without_exactly_one_dataset() {
    let srv = TestServer::start();
    let object = workitem_body("CAD");

    // An empty array names no dataset.
    srv.create_json(WORKITEM_A, b"[]").assert_status(400);

    let mut two = Vec::from(b"[".as_slice());
    two.extend_from_slice(&object);
    two.push(b',');
    two.extend_from_slice(&object);
    two.push(b']');
    srv.create_json(WORKITEM_A, &two).assert_status(400);

    // An array of something that is not a dataset is not a dataset either.
    srv.create_json(WORKITEM_A, b"[1,2]").assert_status(400);
    srv.create_json(WORKITEM_A, b"[null]").assert_status(400);
}

#[test]
fn ups_create_201_and_search() {
    let srv = TestServer::start();
    // Scenario 1: POST /workitems/{uid} -> 201 with SCHEDULED workitem JSON.
    let r = srv.create_json(WORKITEM_A, &workitem_body("CAD"));
    r.assert_status(201);
    let ct = r.header("content-type").unwrap();
    assert!(
        ct.contains("application/dicom+json"),
        "create content-type should be dicom+json, got {ct}"
    );
    // PS3.18 11.4.2: the SCP returns the created resource's URL in the
    // Location header field (11.4.3.2.1 lists it among the response fields).
    let loc = r
        .header("location")
        .expect("UPS Create 201 must carry a Location header (PS3.18 11.4.2)");
    assert!(
        loc.starts_with("http://"),
        "Location must be absolute: {loc}"
    );
    assert!(
        loc.ends_with(&format!("/dicomweb/workitems/{WORKITEM_A}")),
        "Location must name the created workitem resource: {loc}"
    );
    let j = r.json();
    assert_eq!(wi_uid(&j), Some(WORKITEM_A));
    assert_eq!(wi_state(&j), Some("SCHEDULED"));
    // The type-1 set is echoed, and the identity pair the standard names.
    assert_eq!(wi_tag(&j, "00741204"), Some("CAD"));
    assert_eq!(wi_tag(&j, "00404041"), Some("READY"));
    assert_eq!((wi_tag(&j, "00741200")), Some("MEDIUM"));
    assert_eq!(wi_tag(&j, "00080016"), Some("1.2.840.10008.5.1.4.34.6.1"));
    // Modality is not a UPS attribute, so this externally created workitem has
    // none: the server has no study to derive one from and the IOD gives the
    // client no way to state one, so the element is omitted rather than
    // fabricated. A STOW-created workitem from a study does render it (the
    // vendor extension) - see cad_workitem_smoke.
    assert!(j.obj_get("00080060").is_none(), "no Modality: {j:?}");
    // The Transaction UID is never echoed (11.5.3.3).
    assert!(
        j.obj_get("00081195").is_none(),
        "no TUID on retrieve: {j:?}"
    );

    // Scenario 2: GET /workitems -> created workitem is visible.
    let r = srv.get("/dicomweb/workitems", &[]);
    r.assert_status(200);
    let arr = r.json();
    let arr = arr.as_array().expect("search returns array");
    let found = arr.iter().any(|it| wi_uid(it) == Some(WORKITEM_A));
    assert!(found, "created workitem must be listed in search");
}

#[test]
fn ups_update_transition_and_delete() {
    let srv = TestServer::start();

    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);

    // Scenario 5a: the standard flow - claim with our own Transaction UID,
    // then complete while echoing it (PS3.4 Table CC.1.1-2).
    claim(&srv, WORKITEM_A);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &claim_body("COMPLETED", CLAIM_TUID),
    );
    r.assert_status(200);
    let j = r.json();
    assert_eq!(wi_state(&j), Some("COMPLETED"));

    // Scenario 5b: DELETE -> 204 per semantics.
    let r = srv.delete(&format!("/dicomweb/workitems/{WORKITEM_A}"));
    r.assert_status(204);
    // Deleted workitem is gone.
    let r = srv.get("/dicomweb/workitems", &[]);
    let arr = r.json();
    let arr = arr.as_array().unwrap();
    assert!(
        !arr.iter().any(|it| wi_uid(it) == Some(WORKITEM_A)),
        "deleted workitem must be absent from search"
    );
}

/// The state-change resource has two names and both must work: the bare
/// workitem PUT, and the `/state` suffix PS3.18 11.2 defines. A client that
/// follows the standard sends the suffixed form, and answering it as "not
/// found" made a conformant client look broken.
#[test]
fn ups_state_route_changes_state_like_the_bare_route() {
    let srv = TestServer::start();

    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);

    // SCHEDULED -> IN PROGRESS through the suffixed route - the transition a
    // client performs when it claims the workitem, so it carries the UID.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("IN PROGRESS"));

    // The change is durable, not just echoed: a fresh retrieve reports it.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("IN PROGRESS"));

    // IN PROGRESS -> COMPLETED through the same suffixed route.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("COMPLETED", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("COMPLETED"));

    // Both names reach one handler, so the state machine is shared: a
    // transition the machine forbids is refused identically on either. The
    // workitem is COMPLETED and SCHEDULED is not a legal Change State value,
    // so this is a 400 on both.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("SCHEDULED", CLAIM_TUID),
    );
    r.assert_status(400);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &claim_body("SCHEDULED", CLAIM_TUID),
    );
    r.assert_status(400);

    // The suffixed route on an unknown workitem is a 404, as the bare route
    // is: the suffix addresses a resource that has to exist.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{UNKNOWN_WORKITEM}/state"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    );
    r.assert_status(404);

    // Cancel through the suffixed route, from a legal predecessor.
    srv.create_json(WORKITEM_B, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_B);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_B}/state"),
        &claim_body("CANCELED", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("CANCELED"));
}

/// A created workitem carries no Transaction UID, and the claimant's own UID
/// is what the optimistic lock then holds.
///
/// PS3.18 11.6: the UID is issued by whoever performs the SCHEDULED ->
/// IN PROGRESS transition. Minting one at create time made every conformant
/// claim fail the lock, because the client sends its own UID and the stored
/// one was ours - a 409 on a workitem nobody else had touched.
#[test]
fn ups_claim_accepts_the_callers_transaction_uid() {
    let srv = TestServer::start();

    let r = srv.create_json(WORKITEM_A, &workitem_body("CAD"));
    r.assert_status(201);
    // The created workitem is unclaimed: it reports no Transaction UID.
    let created = r.json();
    assert!(
        created.obj_get("00081195").is_none(),
        "a created workitem must not report a Transaction UID: {created:?}"
    );

    // Claim with our own UID - the standard shape, and the one clinfer sends.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("IN PROGRESS"));

    // A second claim presenting a different UID is another worker's attempt
    // and must be refused - 400, because CC.2.1-1 makes the UID type 1 and
    // 11.7.3-1 maps "missing/incorrect" to 400.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &claim_body("IN PROGRESS", OTHER_TUID),
    );
    r.assert_status(400);

    // Further transitions with the same UID succeed - the lock recognises the
    // claimant across requests, not just within one.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &claim_body("COMPLETED", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("COMPLETED"));

    // A transition that omits the UID once one is held is refused: the lock
    // is held by a specific caller, not by nobody.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &cancel_body("", ""),
    );
    r.assert_status(400);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &state_change_body("CANCELED"),
    );
    r.assert_status(400);
}

/// The same claim flow over the standard `/state` route, including the
/// cancel case with a foreign UID.
#[test]
fn ups_claim_over_the_state_route_honours_the_lock() {
    let srv = TestServer::start();

    srv.create_json(WORKITEM_B, &workitem_body("CAD"))
        .assert_status(201);

    // Claim through the suffixed route with our own UID.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_B}/state"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("IN PROGRESS"));

    // Cancel with the same UID is allowed, and the reason is stored.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_B}/state"),
        &cancel_body(CLAIM_TUID, "OPERATOR"),
    );
    r.assert_status(200);
    let j = r.json();
    assert_eq!(wi_state(&j), Some("CANCELED"));
    assert_eq!(wi_tag(&j, "00741238"), Some("OPERATOR"));
    // (0040,4052) is SCP-filled on CANCELED (PS3.4 CC.2.5-3).
    assert!(
        wi_tag(&j, "00404052").is_some(),
        "cancellation datetime is filled: {j:?}"
    );

    // A foreign UID is refused on the suffixed route exactly as on the bare
    // one, because both reach the same handler.
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("COMPLETED", OTHER_TUID),
    );
    r.assert_status(400);
}

/// PS3.18 11.7 Table 11.7.2.1-1: `requester = aetitle, O` is an OPTIONAL
/// query parameter on Change State. A client (clinfer) that always sends it
/// must not be refused: the origin reads the state and the Transaction UID
/// from the body and ignores the extra query key.
#[test]
fn ups_state_ignores_the_optional_requester_query_parameter() {
    let srv = TestServer::start();

    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);

    // Claim WITH the optional requester parameter: accepted, state changes.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state?requester=CLINFER"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("IN PROGRESS"));

    // Complete with a DIFFERENT (URL-encoded) requester: still ignored, the
    // lock (TransactionUid in the body) is what decides.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state?requester=STATION%2D1"),
        &claim_body("COMPLETED", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("COMPLETED"));
}

#[test]
fn ups_sse_state_change() {
    let srv = TestServer::start();

    // The SSE endpoint requires the workitem to already exist (404 otherwise),
    // so seed it before subscribing. Subscribe captures subsequent transitions.
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);

    // Subscribe to WORKITEM_A events on a persistent connection.
    let out = Arc::new(StdRwLock::new(Vec::new()));
    let guard = srv.open_sse(WORKITEM_A, out.clone());

    // Deterministic handshake (12.09 CI fix): the server writes the initial
    // `: keep-alive` comment only AFTER registering the subscriber, so wait
    // for it before the PUT - the transition can never overtake the GET.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        {
            let raw = out.read().unwrap();
            if String::from_utf8_lossy(&raw).contains(": keep-alive") {
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // A state-change UPDATE (SCHEDULED -> IN PROGRESS) emits a StateChange
    // event. It is the claim, so it carries the Transaction UID.
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    )
    .assert_status(200);

    // The SSE reader thread drains the event asynchronously. Wait for the
    // framing golden to appear (bounded) instead of a fixed sleep, which
    // flaked under load (~1/3 runs).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        {
            let raw = out.read().unwrap();
            if String::from_utf8_lossy(&raw).contains(STATE_CHANGE_FRAMING) {
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    guard.close();

    let raw = out.read().unwrap().clone();
    let raw_str = String::from_utf8_lossy(&raw);
    if std::env::var_os("UPS_DEBUG").is_some() {
        eprintln!("DEBUG SSE raw {:?}", raw_str);
    }

    // The connection stays open -> no HTTP response terminator; we inspect the
    // body bytes directly. Capture the framing golden.
    assert!(
        raw_str.contains(STATE_CHANGE_FRAMING),
        "SSE must emit event: StateChange + id: (framing golden)"
    );

    // The StateChange payload is a DICOM JSON Data Set (PS3.18 8.10.5.1.1):
    // the affected UPS in (0000,1000) and the state it is now in (0074,1000).
    // The retired envelope (`workitem_uid`/`previous_state`/`new_state`) is
    // asserted ABSENT below.
    let state_needle = format!(
        "\"00001000\":{{\"vr\":\"UI\",\"Value\":[\"{WORKITEM_A}\"]}},\"00404041\":{{\"vr\":\"CS\",\"Value\":[\"READY\"]}},\"00741000\":{{\"vr\":\"CS\",\"Value\":[\"IN PROGRESS\"]}}"
    );
    assert!(
        raw_str.contains(&state_needle),
        "state-change event JSON shape (golden), got: {raw_str:?}"
    );
    for key in [
        "\"workitem_uid\"",
        "\"previous_state\"",
        "\"new_state\"",
        "\"timestamp\"",
        "\"event_id\"",
    ] {
        assert!(
            !raw_str.contains(key),
            "the SSE Event Report must not carry the envelope key {key}"
        );
    }

    // Monotonic id: field present and numeric (exact value is runtime-counter
    // dependent - events are dropped when no subscriber is connected).
    assert!(
        raw_str.contains("\nid: "),
        "SSE must carry an id: field for Last-Event-ID support"
    );
}

#[test]
fn ups_sse_unknown_workitem_404() {
    let srv = TestServer::start();

    // Scenario 7: subscribing to an unknown workitem -> 404.
    let out = Arc::new(StdRwLock::new(Vec::new()));
    let guard = srv.open_sse(UNKNOWN_WORKITEM, out.clone());
    std::thread::sleep(Duration::from_millis(300));
    guard.close();

    let raw = out.read().unwrap().clone();
    let raw_str = String::from_utf8_lossy(&raw);
    assert!(
        raw_str.contains("404"),
        "SSE subscribe to unknown workitem must return 404, got: {raw_str:?}"
    );
    assert!(
        raw_str.contains("Workitem not found"),
        "404 body should mention workitem not found"
    );
}

#[test]
fn ups_crud_unknown_workitem() {
    let srv = TestServer::start();

    // Scenario 8: CRUD on unknown workitem -> 404/400 with current bodies.

    // update unknown -> 404
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{UNKNOWN_WORKITEM}"),
        &claim_body("COMPLETED", CLAIM_TUID),
    );
    r.assert_status(404);
    assert_eq!(
        r.json().obj_get("error").and_then(|x| x.as_str()),
        Some("Not Found")
    );

    // delete unknown -> 404
    let r = srv.delete(&format!("/dicomweb/workitems/{UNKNOWN_WORKITEM}"));
    r.assert_status(404);

    // create without the type-1 set -> 400 naming the first that is missing
    let bad = r#"{"00100020":{"vr":"LO","Value":["PT-1"]}}"#;
    let r = srv.create_json(WORKITEM_B, bad.as_bytes());
    r.assert_status(400);
    let j = r.json();
    assert_eq!(
        j.obj_get("error").and_then(|x| x.as_str()),
        Some("Bad Request")
    );
    assert!(
        j.obj_get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .contains("0074,1200"),
        "400 message should name the missing type-1 attribute"
    );
}

#[test]
fn ups_create_duplicate_conflict() {
    let srv = TestServer::start();

    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);

    // Duplicate create -> 409 Conflict.
    let r = srv.create_json(WORKITEM_A, &workitem_body("CAD"));
    r.assert_status(409);
    assert_eq!(
        r.json().obj_get("error").and_then(|x| x.as_str()),
        Some("Conflict")
    );
}

// -- The standard-first state machine (PS3.4 Table CC.1.1-2) --------------

/// A claim is a state change like any other: the Transaction UID is type 1
/// (CC.2.1-1), so a request without one is refused with 400 rather than
/// accepted as an anonymous claim.
#[test]
fn ups_claim_without_a_transaction_uid_is_refused() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);

    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &state_change_body("IN PROGRESS"),
    );
    r.assert_status(400);
    // The refusal did not move the workitem: it is still unclaimed.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    assert_eq!(wi_state(&r.json()), Some("SCHEDULED"));
}

/// Missing and incorrect are the same refusal with the same code (C301 ->
/// 400 per 11.7.3-1), on both a held workitem and on the cancel transition.
#[test]
fn ups_missing_or_foreign_uid_is_400_not_409() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // Missing UID -> 400.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &state_change_body("COMPLETED"),
    );
    r.assert_status(400);

    // Foreign UID -> 400.
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("COMPLETED", OTHER_TUID),
    );
    r.assert_status(400);

    // A cancel with the foreign UID -> 400 (the live-run failure).
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &cancel_body(OTHER_TUID, "X"),
    );
    r.assert_status(400);
}

/// The equal-transition rows of Table CC.1.1-2: the two terminal states answer
/// a warning (B306H/B304H), which is a success; SCHEDULED and IN PROGRESS
/// answer C303H/C302H, which are failures.
#[test]
fn ups_equal_transitions_follow_the_table() {
    let srv = TestServer::start();

    // SCHEDULED -> SCHEDULED: a Change State may not name SCHEDULED at all.
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("SCHEDULED", CLAIM_TUID),
    );
    r.assert_status(400);

    // IN PROGRESS -> IN PROGRESS: C302H.
    claim(&srv, WORKITEM_A);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("IN PROGRESS", CLAIM_TUID),
    );
    r.assert_status(400);

    // COMPLETED -> COMPLETED: B306H, a success.
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("COMPLETED", CLAIM_TUID),
    )
    .assert_status(200);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("COMPLETED", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("COMPLETED"));

    // CANCELED -> CANCELED: B304H, a success.
    srv.create_json(WORKITEM_B, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_B);
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_B}/state"),
        &claim_body("CANCELED", CLAIM_TUID),
    )
    .assert_status(200);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_B}/state"),
        &claim_body("CANCELED", CLAIM_TUID),
    );
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("CANCELED"));
}

/// Every remaining row of Table CC.1.1-2 that the SCP must refuse -> 400.
#[test]
fn ups_illegal_transitions_are_400() {
    let srv = TestServer::start();

    // SCHEDULED -> COMPLETED and SCHEDULED -> CANCELED: C310H. The prose note
    // in CC.1.1 is explicit that a SCHEDULED instance may not be set final
    // without first being set IN PROGRESS.
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    for target in ["COMPLETED", "CANCELED"] {
        let r = srv.put_json(
            &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
            &claim_body(target, CLAIM_TUID),
        );
        r.assert_status(400);
    }

    // IN PROGRESS -> SCHEDULED: C303H.
    claim(&srv, WORKITEM_A);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("SCHEDULED", CLAIM_TUID),
    );
    r.assert_status(400);

    // COMPLETED -> IN PROGRESS and COMPLETED -> CANCELED.
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("COMPLETED", CLAIM_TUID),
    )
    .assert_status(200);
    for target in ["IN PROGRESS", "CANCELED"] {
        let r = srv.put_json(
            &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
            &claim_body(target, CLAIM_TUID),
        );
        r.assert_status(400);
    }

    // CANCELED -> IN PROGRESS and CANCELED -> COMPLETED.
    srv.create_json(WORKITEM_B, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_B);
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_B}/state"),
        &claim_body("CANCELED", CLAIM_TUID),
    )
    .assert_status(200);
    for target in ["IN PROGRESS", "COMPLETED"] {
        let r = srv.put_json(
            &format!("/dicomweb/workitems/{WORKITEM_B}/state"),
            &claim_body(target, CLAIM_TUID),
        );
        r.assert_status(400);
    }
}

/// SUSPENDED is not a 2026c UPS state (Table CC.1.1-1, PS3.3 C.30.1): it
/// cannot be created, cannot be requested, and is refused by the search.
#[test]
fn ups_suspended_is_not_a_state() {
    let srv = TestServer::start();

    // Create asking for SUSPENDED -> 400 (the state is always SCHEDULED).
    let body = r#"{"00404005":{"vr":"DT","Value":["20260917T103000"]},"00404041":{"vr":"CS","Value":["READY"]},"00741200":{"vr":"CS","Value":["MEDIUM"]},"00741204":{"vr":"SH","Value":["CAD"]},"00741000":{"vr":"CS","Value":["SUSPENDED"]}}"#;
    let r = srv.create_json(WORKITEM_A, body.as_bytes());
    r.assert_status(400);

    // A legal create, then IN PROGRESS -> SUSPENDED -> 400.
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);
    let r = srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &claim_body("SUSPENDED", CLAIM_TUID),
    );
    r.assert_status(400);

    // Search by the state -> 400 naming the legal set.
    let r = srv.get("/dicomweb/workitems?00741000=SUSPENDED", &[]);
    r.assert_status(400);
}

/// A create that asks for a state other than SCHEDULED is refused, so a
/// workitem can never be born final and permanently locked.
#[test]
fn ups_create_other_than_scheduled_is_refused() {
    let srv = TestServer::start();

    for requested in ["COMPLETED", "CANCELED", "IN PROGRESS"] {
        let body = format!(
            r#"{{"00404005":{{"vr":"DT","Value":["20260917T103000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"SH","Value":["CAD"]}},"00741000":{{"vr":"CS","Value":["{requested}"]}}}}"#
        );
        let r = srv.create_json(WORKITEM_A, body.as_bytes());
        r.assert_status(400);
    }

    // Asking for SCHEDULED explicitly is accepted and is what is stored.
    let body = r#"{"00404005":{"vr":"DT","Value":["20260917T103000"]},"00404041":{"vr":"CS","Value":["READY"]},"00741200":{"vr":"CS","Value":["MEDIUM"]},"00741204":{"vr":"SH","Value":["CAD"]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#;
    let r = srv.create_json(WORKITEM_A, body.as_bytes());
    r.assert_status(201);
    assert_eq!(wi_state(&r.json()), Some("SCHEDULED"));
}

/// The type-1 set of CC.2.5-3 is required at create, and each missing
/// attribute is named in the refusal.
#[test]
fn ups_create_requires_the_type_1_set() {
    let srv = TestServer::start();

    // One attribute removed at a time; the message names the missing one.
    // The check reports the first attribute of the CC.2.5-3 set that is
    // absent, so each case removes exactly one and leaves the rest.
    let cases = [
        (
            r#"{"00404005":{"vr":"DT","Value":["20260917T103000"]},"00404041":{"vr":"CS","Value":["READY"]},"00741204":{"vr":"SH","Value":["CAD"]}}"#,
            "0074,1200",
        ),
        (
            r#"{"00404005":{"vr":"DT","Value":["20260917T103000"]},"00404041":{"vr":"CS","Value":["READY"]},"00741200":{"vr":"CS","Value":["MEDIUM"]}}"#,
            "0074,1204",
        ),
        (
            r#"{"00404041":{"vr":"CS","Value":["READY"]},"00741200":{"vr":"CS","Value":["MEDIUM"]},"00741204":{"vr":"SH","Value":["CAD"]}}"#,
            "0040,4005",
        ),
        (
            r#"{"00404005":{"vr":"DT","Value":["20260917T103000"]},"00741200":{"vr":"CS","Value":["MEDIUM"]},"00741204":{"vr":"SH","Value":["CAD"]}}"#,
            "0040,4041",
        ),
    ];
    for (body, expected_tag) in cases {
        let r = srv.create_json(WORKITEM_A, body.as_bytes());
        r.assert_status(400);
        let j = r.json();
        let message = j
            .obj_get("message")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        assert!(
            message.contains(expected_tag),
            "refusal must name the missing attribute {expected_tag}, got {message:?}"
        );
    }

    // With the full set the workitem is created and the values come back.
    let r = srv.create_json(WORKITEM_A, &workitem_body("CAD"));
    r.assert_status(201);
    let j = r.json();
    assert_eq!(wi_tag(&j, "00741204"), Some("CAD"));
    assert_eq!(wi_tag(&j, "00404041"), Some("READY"));
    assert_eq!(wi_tag(&j, "00741200"), Some("MEDIUM"));
    assert_eq!(wi_tag(&j, "00404005"), Some("20260917T103000"));
}

/// A workitem created with an input study exposes it through (0040,4021)
/// Input Information Sequence, and (0020,000D) carries the real study UID
/// rather than the workitem's own identity.
#[test]
fn ups_input_information_names_the_study() {
    let srv = TestServer::start();

    let r = srv.create_json(WORKITEM_A, &workitem_body_with_study("CAD", STUDY_A));
    r.assert_status(201);
    let j = r.json();

    // Identity is (0008,0018); the workitem UID must not be in the study slot.
    assert_eq!(wi_uid(&j), Some(WORKITEM_A));
    // The study is carried through BOTH sequences (Task 3): the Referenced
    // Request Sequence and the Input Information Sequence. There is no bare
    // top-level (0020,000D).
    assert!(
        j.obj_get("0020000D").is_none(),
        "no bare top-level study tag: {j:?}"
    );
    assert_eq!(wi_seq_tag(&j, "0040A370", "0020000D"), Some(STUDY_A));
    assert_eq!(wi_seq_tag(&j, "00404021", "0020000D"), Some(STUDY_A));

    // The sequence: one item, TypeOfInstances DICOM + the study UID.
    let item = j
        .obj_get("00404021")
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.first_of_array())
        .expect("input information sequence item");
    assert_eq!(
        item.obj_get("0040E020")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("DICOM")
    );
    assert_eq!(
        item.obj_get("0020000D")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some(STUDY_A)
    );

    // The retrieve response never carries the Transaction UID.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    assert!(r.json().obj_get("00081195").is_none());
}

/// RED FACT. A Create that names the study and the accession through the
/// Referenced Request Sequence (0040,A370) keeps BOTH, and the workitem a
/// later search by PatientID + the sequence accession finds it.
///
/// The requester (Weasis, following the nroduit rule) fills the sequence, not
/// the bare tags. Before this the server read only the bare (0008,0050) and
/// the Input Information Sequence, so a workitem created this way lost its
/// accession and its study - and the follow-up search by
/// PatientID+AccessionNumber returned nothing.
#[test]
fn ups_create_reads_the_referenced_request_sequence() {
    let srv = TestServer::start();
    let accession = "ACC-2026-0042";

    let mut body = workitem_body_with_referenced_request("CAD", accession, STUDY_A);
    // Add the PatientID the search will use (the workitem_body helpers do not
    // carry one; the nroduit rule keys on PatientID + accession).
    let with_pid = String::from_utf8(body.clone()).expect("utf8");
    body = with_pid
        .replacen(
            r#""00741204""#,
            r#""00100020":{"vr":"LO","Value":["PID-RRQ"]},"00741204""#,
            1,
        )
        .into_bytes();

    let r = srv.create_json(WORKITEM_A, &body);
    r.assert_status(201);
    let j = r.json();

    // The accession came from the sequence, not from a bare tag (there is none).
    assert!(
        j.obj_get("00080050").is_none(),
        "no bare top-level accession: {j:?}"
    );
    assert_eq!(wi_seq_tag(&j, "0040A370", "00080050"), Some(accession));
    // The study came from the sequence too, and is rendered inside it.
    assert_eq!(wi_seq_tag(&j, "0040A370", "0020000D"), Some(STUDY_A));

    // The follow-up search of the nroduit rule now finds the workitem:
    // the sequence-qualified accession key and the sequence study key are
    // both accepted (Task 2 part a).
    let r = srv.get(
        &format!(
            "/dicomweb/workitems?00100020=PID-RRQ&0040A370.00080050={accession}&0040A370.0020000D={STUDY_A}"
        ),
        &[],
    );
    r.assert_status(200);
    let arr = r.json();
    let arr = arr.as_array().expect("array");
    assert_eq!(
        arr.len(),
        1,
        "the search finds exactly the created workitem"
    );
    assert_eq!(wi_uid(&arr[0]), Some(WORKITEM_A));
}

/// The Referenced Request Sequence WINS over the bare fallbacks when a body
/// carries both, so a workitem created from the standard carriage is
/// findable by the query that named it.
#[test]
fn ups_create_prefers_the_referenced_request_over_the_bare_tags() {
    let srv = TestServer::start();

    // Sequence says ACC-SEQ / STUDY-SEQ; the bare tags say ACC-BARE / STUDY-BARE.
    let body = format!(
        r#"{{"00404005":{{"vr":"DT","Value":["20260917T103000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"SH","Value":["CAD"]}},"00080050":{{"vr":"SH","Value":["ACC-BARE"]}},"00404021":{{"vr":"SQ","Value":[{{"0020000D":{{"vr":"UI","Value":["{STUDY_BARE}"]}}}}]}},"0040A370":{{"vr":"SQ","Value":[{{"00080050":{{"vr":"SH","Value":["ACC-SEQ"]}},"0020000D":{{"vr":"UI","Value":["{STUDY_A}"]}}}}]}}}}"#
    );
    let r = srv.create_json(WORKITEM_A, body.as_bytes());
    r.assert_status(201);
    let j = r.json();
    assert_eq!(
        wi_seq_tag(&j, "0040A370", "00080050"),
        Some("ACC-SEQ"),
        "the sequence accession is the standard carriage and wins"
    );
    assert_eq!(
        wi_seq_tag(&j, "0040A370", "0020000D"),
        Some(STUDY_A),
        "the sequence study wins over the Input Information Sequence"
    );
    // The Input Information Sequence names the SAME study: on the Create path
    // the Referenced Request Sequence study is the input study (Task 2), so
    // both carriages render one fact. The bare Input Information Sequence value
    // from the body is therefore not a second opinion - it is the same study,
    // reached through the winning carriage.
    assert_eq!(wi_seq_tag(&j, "00404021", "0020000D"), Some(STUDY_A));
    assert!(
        j.obj_get("00080050").is_none() && j.obj_get("0020000D").is_none(),
        "no bare top-level duplicates: {j:?}"
    );
}

/// Search matches on the standard Procedure Step Label key, and on the
/// vendor-extended Modality key - which is not a UPS attribute, so an
/// externally created workitem has no value for it and no modality query can
/// reach one.
#[test]
fn ups_search_matches_on_the_standard_keys() {
    let srv = TestServer::start();

    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    srv.create_json(WORKITEM_B, &workitem_body("OTHER"))
        .assert_status(201);

    // The label key selects exactly the matching workitem.
    let r = srv.get("/dicomweb/workitems?00741204=CAD", &[]);
    r.assert_status(200);
    let arr = r.json();
    let arr = arr.as_array().expect("array");
    assert_eq!(arr.len(), 1, "one workitem carries the CAD label");
    assert_eq!(wi_uid(&arr[0]), Some(WORKITEM_A));

    // The state key still works.
    let r = srv.get("/dicomweb/workitems?00741000=SCHEDULED", &[]);
    r.assert_status(200);
    assert_eq!(r.json().as_array().expect("array").len(), 2);

    // The Modality key is LIVE and matches case-insensitively, but the UPS IOD
    // has no Modality for a client to state: these workitems were created
    // externally and carry none, so no modality value can select them. The
    // query answers an empty 200 rather than everything - an unmatched filter
    // is not an ignored one.
    for query in ["00080060=CAD", "00080060=CT"] {
        let r = srv.get(&format!("/dicomweb/workitems?{query}"), &[]);
        r.assert_status(200);
        assert_eq!(
            r.json().as_array().expect("array").len(),
            0,
            "{query}: workitems created externally carry no Modality"
        );
    }
}

// -- Update Workitem (PS3.18 11.6) ---------------------------------------

/// An Update payload carrying (0040,4033) Output Information Sequence: one
/// item per produced object, each a (0008,1150)/(0008,1155) pair.
fn output_body(instance_uids: &[&str]) -> Vec<u8> {
    let items: Vec<String> = instance_uids
        .iter()
        .map(|uid| {
            format!(
                r#"{{"00081150":{{"vr":"UI","Value":["1.2.840.10008.5.1.4.1.1.66.4"]}},"00081155":{{"vr":"UI","Value":["{uid}"]}}}}"#
            )
        })
        .collect();
    format!(
        r#"{{"00404033":{{"vr":"SQ","Value":[{}]}}}}"#,
        items.join(",")
    )
    .into_bytes()
}

/// An Update payload that carries the Transaction UID (0008,1195) IN THE BODY
/// plus an output list of `instance_uids`.
///
/// This is the CC.2.6.2 shape: the locking UID is an Attribute of the data
/// set, and the produced objects ride along in (0040,4033). An empty
/// `instance_uids` yields a body with the UID alone.
fn body_with_uid(transaction_uid: &str, instance_uids: &[&str]) -> Vec<u8> {
    let mut body = format!(r#"{{"00081195":{{"vr":"UI","Value":["{transaction_uid}"]}}"#);
    if !instance_uids.is_empty() {
        let outputs = output_body(instance_uids);
        // Splice in the sequence from the output-only body (drop its braces).
        body.push(',');
        body.push_str(std::str::from_utf8(&outputs[1..outputs.len() - 1]).expect("utf8"));
    }
    body.push('}');
    body.into_bytes()
}

/// A one-item `CodeSequenceMacro` attribute body.
fn code_body(tag: &str, value: &str, scheme: &str, meaning: &str) -> Vec<u8> {
    format!(
        r#"{{"{tag}":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["{value}"]}},"00080102":{{"vr":"SH","Value":["{scheme}"]}},"00080104":{{"vr":"LO","Value":["{meaning}"]}}}}]}}}}"#
    )
    .into_bytes()
}

/// A `CodeSequenceMacro` item as it appears INSIDE the (0074,1216) item - the
/// three sub-elements wrapped in the item's own braces, with no enclosing tag
/// name.
fn code_child(value: &str, scheme: &str, meaning: &str) -> String {
    format!(
        r#"{{"00080100":{{"vr":"SH","Value":["{value}"]}},"00080102":{{"vr":"SH","Value":["{scheme}"]}},"00080104":{{"vr":"LO","Value":["{meaning}"]}}}}"#
    )
}

/// An Update payload in the STANDARD shape: one item of (0074,1216) Unified
/// Procedure Step Performed Procedure Sequence, whose children are the
/// performed datetimes, the two performed codes, and the output list.
///
/// This is what a conformant Task Performer sends (PS3.3 Table C.30.3-1 nests
/// every one of those attributes), and what dcm4chee-arc accepts - it drops
/// the flat top-level form silently.
fn performed_item_body(
    instance_uids: &[&str],
    station_code: Option<(&str, &str, &str)>,
    workitem_code: Option<(&str, &str, &str)>,
    datetimes: Option<(&str, &str)>,
) -> Vec<u8> {
    let items: Vec<String> = instance_uids
        .iter()
        .map(|uid| {
            format!(
                r#"{{"00081150":{{"vr":"UI","Value":["1.2.840.10008.5.1.4.1.1.66.4"]}},"00081155":{{"vr":"UI","Value":["{uid}"]}}}}"#
            )
        })
        .collect();
    let mut children: Vec<String> = Vec::new();
    if let Some((value, scheme, meaning)) = station_code {
        children.push(format!(
            "\"00404028\":{{\"vr\":\"SQ\",\"Value\":[{}]}}",
            code_child(value, scheme, meaning)
        ));
    }
    if let Some((value, scheme, meaning)) = workitem_code {
        children.push(format!(
            "\"00404019\":{{\"vr\":\"SQ\",\"Value\":[{}]}}",
            code_child(value, scheme, meaning)
        ));
    }
    if let Some((start, end)) = datetimes {
        children.push(format!(
            "\"00404050\":{{\"vr\":\"DT\",\"Value\":[\"{start}\"]}},\
             \"00404051\":{{\"vr\":\"DT\",\"Value\":[\"{end}\"]}}"
        ));
    }
    if !items.is_empty() {
        children.push(format!(
            "\"00404033\":{{\"vr\":\"SQ\",\"Value\":[{}]}}",
            items.join(",")
        ));
    }
    let performed_item = format!("{{{}}}", children.join(","));
    format!("{{\"00741216\":{{\"vr\":\"SQ\",\"Value\":[{performed_item}]}}}}").into_bytes()
}

/// The same nested body with the Transaction UID carried as a payload
/// attribute (CC.2.6.2) alongside the (0074,1216) item.
fn performed_item_body_with_uid(
    transaction_uid: &str,
    instance_uids: &[&str],
    datetimes: Option<(&str, &str)>,
) -> Vec<u8> {
    let nested = performed_item_body(instance_uids, None, None, datetimes);
    let inner = std::str::from_utf8(&nested[1..nested.len() - 1]).expect("utf8");
    format!(r#"{{"00081195":{{"vr":"UI","Value":["{transaction_uid}"]}},{inner}}}"#).into_bytes()
}

/// RED FACT. A change-state payload carrying (0040,4033) does NOT store it.
///
/// PS3.18 11.7.1.4: the Change State payload carries the Procedure Step State
/// and the Transaction UID "and nothing else is read". An output list sent
/// there is therefore discarded, which is what this pins - and it is the
/// reason the completion has to be two requests.
///
/// Before the fix this test failed, because there was no Update transaction at
/// all: the ONLY way to write a workitem was the change state, so the output
/// list had nowhere to go and a COMPLETED workitem could not name its results.
/// The `ups_update_attributes_writes_the_output_list` test below is the other
/// half: the same payload, sent to the Update resource, IS stored.
#[test]
fn a_change_state_payload_carrying_outputs_stores_nothing() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // A state change whose payload ALSO names produced instances. The state
    // part is honoured; the output part must be ignored.
    let mut body = state_change_body_with_uid("COMPLETED", CLAIM_TUID);
    body.pop(); // drop the closing brace of the state body
    body.extend_from_slice(b",");
    // Splice in (0040,4033) from the output-only body.
    let outputs = output_body(&["1.2.826.0.1.3680043.10.7777.1"]);
    body.extend_from_slice(&outputs[1..outputs.len() - 1]);
    body.push(b'}');

    srv.put_json(&format!("/dicomweb/workitems/{WORKITEM_A}/state"), &body)
        .assert_status(200);

    // The state moved...
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    assert_eq!(
        wi_state(&r.json()),
        Some("COMPLETED"),
        "the state change itself is honoured"
    );
    // ...and the output list was NOT stored: 11.7.1.4 reads nothing but the
    // state and the UID, so the sequence never reached the record.
    assert!(
        r.json().obj_get("00404033").is_none(),
        "(0040,4033) must NOT be stored through the change-state transaction"
    );
}

/// RED FACT. Update (11.6) accepts the Transaction UID from the PAYLOAD
/// (0008,1195), which is the carriage PS3.4 CC.2.6.2 makes mandatory for an
/// IN PROGRESS workitem and the only one dcm4chee-arc reads.
///
/// Before this the server read the UID from the query only and refused any
/// payload key outside the merge set, so a body-TUID was rejected as "not
/// updatable" - an asymmetry with the Change State transaction, which had
/// always read it from the body.
#[test]
fn ups_update_attributes_accepts_the_transaction_uid_from_the_body() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // The body carries (0008,1195) AND a nested (0074,1216) item with the
    // output list, no query parameter.
    srv.post_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &performed_item_body_with_uid(CLAIM_TUID, &["1.2.826.0.1.3680043.10.7777.1"], None),
    )
    .assert_status(200);

    // The lock was accepted and the merge attribute was stored.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    assert!(
        wi_nested_item(&r.json(), "00741216")
            .and_then(|item| item.obj_get("00404033"))
            .is_some(),
        "the output list is stored through a body-TUID Update"
    );

    // The TUID itself is NOT merged into the record: it is the lock, and the
    // retrieve must not echo it (11.5.3.3).
    assert!(
        r.json().obj_get("00081195").is_none(),
        "the Transaction UID is a lock, never an echoed attribute"
    );
}

/// The query form (PS3.18 11.6.1.2) still works - it is the carriage a client
/// literally following 11.6 sends, and removing it would break that client.
#[test]
fn ups_update_attributes_still_accepts_the_query_transaction_uid() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // The existing helper uses `?TransactionUid=` and a body carrying the
    // nested (0074,1216) item.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(&["1.2.826.0.1.3680043.10.7777.2"], None, None, None),
    )
    .assert_status(200);

    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    assert!(
        wi_nested_item(&r.json(), "00741216").is_some(),
        "the query form still stores the merge set"
    );
}

/// When BOTH carriages are present with DIFFERENT values the request is a
/// contradiction and is refused; when they AGREE it is one statement and is
/// accepted. A UID that is absent from both is refused.
#[test]
fn ups_update_attributes_checks_both_transaction_uid_carriages() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // Body and query DIFFER -> 400.
    srv.post_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}?TransactionUid={OTHER_TUID}"),
        &performed_item_body_with_uid(CLAIM_TUID, &["1.2.3.4"], None),
    )
    .assert_status(400);

    // Body and query AGREE -> 200.
    srv.post_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}?TransactionUid={CLAIM_TUID}"),
        &performed_item_body_with_uid(CLAIM_TUID, &["1.2.3.4"], None),
    )
    .assert_status(200);

    // Neither carriage -> 400.
    srv.post_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &performed_item_body(&["1.2.3.4"], None, None, None),
    )
    .assert_status(400);

    // A body that carries ONLY the UID and no updatable attribute says
    // nothing -> 400, exactly like an empty payload.
    srv.post_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &body_with_uid(CLAIM_TUID, &[]),
    )
    .assert_status(400);

    // A body that carries the UID plus an attribute OUTSIDE the merge set is
    // still refused on that attribute (the UID exemption does not open the
    // set).
    let mut bad = body_with_uid(CLAIM_TUID, &["1.2.3.4"]);
    bad.pop();
    bad.extend_from_slice(br#","00741204":{"vr":"SH","Value":["SNEAKY"]}}"#);
    srv.post_json(&format!("/dicomweb/workitems/{WORKITEM_A}"), &bad)
        .assert_status(400);
}

/// m20 (PS3.18 11.6.2): a workitem in a Final State cannot be updated.
///
/// 11.6.2 states the answer in one sentence and repeats it in the status
/// table: "If the Workitem is in the COMPLETED or CANCELED state, the response
/// shall be a 400 (Bad Request) failure response". The transaction is the UPS
/// DIMSE N-SET, and CC.2.6.2 admits none once the performed procedure step is
/// final: the record is closed and the objects it produced are attested by the
/// completion report. Answering 200 would let a performer rewrite a report the
/// archive has already accepted, so this asserts the status AND that the stored
/// output list is untouched.
#[test]
fn ups_update_attributes_is_refused_on_a_completed_workitem() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // Record one output while the workitem is still open.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(&["1.2.826.0.1.3680043.10.7777.1"], None, None, None),
    )
    .assert_status(200);

    // Close it.
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &claim_body("COMPLETED", CLAIM_TUID),
    )
    .assert_status(200);

    // A second Update is now a 400 per 11.6.2, not a silent 200 and not 409
    // (11.6.2 reserves 409 for a request inconsistent with the current state).
    let r = srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(&["1.2.826.0.1.3680043.10.7777.2"], None, None, None),
    );
    r.assert_status(400);

    // The closed record was not rewritten: it still names only the first
    // output. A 400 that had already merged would be a worse answer than 200.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let j = r.json();
    let item = wi_nested_item(&j, "00741216").expect("(0074,1216) with one item");
    let items = item
        .obj_get("00404033")
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.as_array())
        .expect("(0040,4033) inside the performed item");
    assert_eq!(items.len(), 1, "the refused update must not have merged");
}

/// Update (11.6) stores the merger set, and the stored values are visible
/// from retrieve AND from search.
#[test]
fn ups_update_attributes_writes_the_output_list() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(
            &[
                "1.2.826.0.1.3680043.10.7777.1",
                "1.2.826.0.1.3680043.10.7777.2",
            ],
            None,
            None,
            None,
        ),
    )
    .assert_status(200);

    // Visible from retrieve.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let j = r.json();
    let item = wi_nested_item(&j, "00741216").expect("(0074,1216) with one item");
    let items = item
        .obj_get("00404033")
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.as_array())
        .expect("(0040,4033) with a Value array inside the performed item");
    assert_eq!(items.len(), 2, "both produced instances are named");
    let instances: Vec<&str> = items
        .iter()
        .map(|item| {
            item.obj_get("00081155")
                .and_then(|v| v.obj_get("Value"))
                .and_then(|v| v.as_str_array_first())
                .expect("a referenced instance")
        })
        .collect();
    assert_eq!(
        instances,
        vec![
            "1.2.826.0.1.3680043.10.7777.1",
            "1.2.826.0.1.3680043.10.7777.2"
        ]
    );
    // (0008,1155) is the key a consumer retrieves by, so the STORED value must
    // be the first UID the performer sent, not a re-derived or reordered one.
    // The list above proves both entries exist; this pins the field itself.
    assert!(
        items[0]
            .obj_get("00081155")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first())
            == Some("1.2.826.0.1.3680043.10.7777.1"),
        "the stored (0008,1155) of the first output item is the first UID sent"
    );
    // The update did NOT move the state: 11.6 modifies attributes only.
    assert_eq!(
        wi_state(&j),
        Some("IN PROGRESS"),
        "an attribute update must not change the workflow state"
    );

    // Visible from search too - the same store backs both routes.
    let r = srv.get("/dicomweb/workitems", &[]);
    r.assert_status(200);
    let arr = r.json();
    let found = arr
        .as_array()
        .expect("array")
        .iter()
        .find(|wi| wi_uid(wi) == Some(WORKITEM_A))
        .expect("the workitem is in the search result");
    assert!(
        wi_nested_item(found, "00741216").is_some(),
        "the output list survives into the search render"
    );
}

/// The series the performer names in an output item travels with it: a
/// COMPLETED Update that carries (0020,000E) next to (0008,1150)/(0008,1155)
/// is stored and rendered back with the series, so a consumer can build the
/// direct retrieve URL. PS3.3 Table 10-3b types Series Instance UID 1C for an
/// object with a Series IE, and this server must not drop it.
#[test]
fn ups_update_keeps_the_series_in_the_output_item() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    let body = concat!(
        r#"{"00741216":{"vr":"SQ","Value":[{"00404033":{"vr":"SQ","Value":["#,
        r#"{"00081150":{"vr":"UI","Value":["1.2.840.10008.5.1.4.1.1.7"]},"#,
        r#""0020000E":{"vr":"UI","Value":["1.2.826.0.1.3680043.10.7777.100"]},"#,
        r#""00081155":{"vr":"UI","Value":["1.2.826.0.1.3680043.10.7777.1"]}}"#,
        r#"]}}]}}"#,
    )
    .as_bytes()
    .to_vec();
    srv.update_json(WORKITEM_A, CLAIM_TUID, &body)
        .assert_status(200);

    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let j = r.json();
    let item = wi_nested_item(&j, "00741216")
        .and_then(|p| p.obj_get("00404033"))
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .expect("the output item");
    assert_eq!(
        item.obj_get("0020000E")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("1.2.826.0.1.3680043.10.7777.100"),
        "the series the performer named survives the round trip"
    );
    assert_eq!(
        item.obj_get("00081150")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("1.2.840.10008.5.1.4.1.1.7")
    );
    assert_eq!(
        item.obj_get("00081155")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.as_str_array_first()),
        Some("1.2.826.0.1.3680043.10.7777.1")
    );
}

/// The performed report is written by the same Update and rendered back -
/// INSIDE the (0074,1216) item, which is the only place PS3.3 Table C.30.3-1
/// puts it.
#[test]
fn ups_update_attributes_writes_the_performed_report() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // One (0074,1216) item carrying both performed codes and the two
    // datetimes: the shape a conformant performer sends.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(
            &["1.2.826.0.1.3680043.10.7777.3"],
            Some(("STATION01", "99CLARUS", "Scanner One")),
            Some(("CT-ORGAN-SEG", "99CLARUS", "AI inference")),
            Some(("20261010120000", "20261010123456")),
        ),
    )
    .assert_status(200);

    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let j = r.json();
    assert_eq!(
        wi_performed_code_value(&j, "00404019"),
        Some("CT-ORGAN-SEG"),
        "the performed workitem code round-trips inside (0074,1216)"
    );
    assert_eq!(
        wi_performed_code_value(&j, "00404028"),
        Some("STATION01"),
        "the performed station code round-trips inside (0074,1216)"
    );
    assert_eq!(
        wi_performed_tag(&j, "00404050"),
        Some("20261010120000"),
        "(0040,4050) is stored from the performed item"
    );
    assert_eq!(
        wi_performed_tag(&j, "00404051"),
        Some("20261010123456"),
        "(0040,4051) is stored from the performed item"
    );
}

/// RED FACT. The nested (0074,1216) update IS the standard shape and is
/// stored; the legacy TOP-LEVEL spelling is REJECTED.
///
/// PS3.3 Table C.30.3-1 makes (0040,4033)/(0040,4019)/(0040,4028) children of
/// (0074,1216). A conformant client sends the nested item and a flat one is
/// not the same statement written differently - it is a workitem attribute set
/// the standard does not define - so it is refused with 400 rather than
/// silently stored, which is what the server did before this change.
///
/// Before the fix this test failed at the FIRST assertion: the nested item was
/// rejected as an unknown attribute (the merge set held only the flat tags).
#[test]
fn the_nested_performed_item_is_accepted_and_the_flat_form_is_refused() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // The nested item: stored, and its children readable inside the sequence.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(
            &["1.2.826.0.1.3680043.10.7777.4"],
            None,
            None,
            Some(("20261010120000", "20261010123456")),
        ),
    )
    .assert_status(200);

    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let j = r.json();
    assert!(
        wi_nested_item(&j, "00741216")
            .and_then(|item| item.obj_get("00404033"))
            .is_some(),
        "the output list is stored inside (0074,1216)"
    );
    assert!(
        j.obj_get("00404033").is_none(),
        "(0040,4033) must not ALSO appear at the top level"
    );

    // The legacy flat spelling: refused, with a message naming the one
    // attribute this transaction writes.
    let refused = srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &output_body(&["1.2.826.0.1.3680043.10.7777.5"]),
    );
    refused.assert_status(400);
    let text = String::from_utf8(refused.body.clone()).expect("utf8");
    assert!(
        text.contains("00741216"),
        "the refusal names the attribute the Update transaction writes: {text}"
    );

    // The refused request changed nothing: the earlier nested output list is
    // still the stored one.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let instance = wi_nested_item(&r.json(), "00741216")
        .and_then(|item| item.obj_get("00404033"))
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.obj_get("00081155"))
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.as_str_array_first())
        .map(str::to_string);
    assert_eq!(instance.as_deref(), Some("1.2.826.0.1.3680043.10.7777.4"));

    // The other two flat spellings are refused on the same rule.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &code_body("00404019", "X", "99CLARUS", "X"),
    )
    .assert_status(400);
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &code_body("00404028", "X", "99CLARUS", "X"),
    )
    .assert_status(400);
}

/// An item that names only SOME of the report leaves the rest as it was.
///
/// PS3.18 11.6.1.4 replaces the elements a request carries and nothing else, so
/// an Update that reports the outputs but no datetime must not erase a start
/// time an earlier Update stored - a performer that reports twice is stating
/// one performance, not two.
#[test]
fn an_item_naming_one_child_leaves_the_others_alone() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // First Update: the datetimes only.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(&[], None, None, Some(("20261010120000", "20261010123456"))),
    )
    .assert_status(200);

    // Second Update: the output list only.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(&["1.2.826.0.1.3680043.10.7777.6"], None, None, None),
    )
    .assert_status(200);

    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let j = r.json();
    assert!(
        wi_nested_item(&j, "00741216")
            .and_then(|item| item.obj_get("00404033"))
            .is_some(),
        "the second update wrote the output list"
    );
    assert_eq!(
        wi_performed_tag(&j, "00404050"),
        Some("20261010120000"),
        "the start time from the first update survives the second"
    );
    assert_eq!(
        wi_performed_tag(&j, "00404051"),
        Some("20261010123456"),
        "the end time from the first update survives the second"
    );
}

/// The performed item is type "zero or one": two items name two performances
/// and the server refuses rather than choosing one, while an EMPTY sequence
/// says nothing and leaves the record unchanged.
#[test]
fn the_performed_item_cardinality_is_enforced() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // Two items -> 400 (Table C.30.3-1 allows zero or one).
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        br#"{"00741216":{"vr":"SQ","Value":[{"00404050":{"vr":"DT","Value":["20261010120000"]}},{"00404051":{"vr":"DT","Value":["20261010123456"]}}]}}"#,
    )
    .assert_status(400);

    // An empty sequence is an empty update: the payload names no attribute to
    // modify -> 400 from the merge-set rule, not from the sequence reader.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        br#"{"00741216":{"vr":"SQ","Value":[]}}"#,
    )
    .assert_status(400);

    // A datetime present but empty claims a performance at no time -> 400.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        br#"{"00741216":{"vr":"SQ","Value":[{"00404050":{"vr":"DT","Value":[""]}}]}}"#,
    )
    .assert_status(400);

    // An empty output list inside the item is still refused (the performer
    // would be claiming it produced nothing).
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        br#"{"00741216":{"vr":"SQ","Value":[{"00404033":{"vr":"SQ","Value":[]}}]}}"#,
    )
    .assert_status(400);

    // Nothing was written by any of the refusals.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    assert!(
        wi_nested_item(&r.json(), "00741216").is_none(),
        "a refused update must not have written anything"
    );
}

/// 11.6.1.2 makes the Transaction UID a QUERY parameter, and it is required.
#[test]
fn ups_update_attributes_requires_and_checks_the_query_uid() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // No query parameter at all -> 400.
    srv.post_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}"),
        &performed_item_body(&["1.2.3.4"], None, None, None),
    )
    .assert_status(400);

    // A different UID -> 400 (the lock is not held by the caller).
    srv.update_json(
        WORKITEM_A,
        OTHER_TUID,
        &performed_item_body(&["1.2.3.4"], None, None, None),
    )
    .assert_status(400);

    // The correct UID -> 200.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(&["1.2.3.4"], None, None, None),
    )
    .assert_status(200);

    // An unclaimed workitem cannot be updated: only the claim issues the lock.
    srv.create_json(WORKITEM_B, &workitem_body("CAD"))
        .assert_status(201);
    srv.update_json(
        WORKITEM_B,
        CLAIM_TUID,
        &performed_item_body(&["1.2.3.4"], None, None, None),
    )
    .assert_status(400);
}

/// An unknown workitem is 404, and an attribute outside the merge set is
/// REFUSED rather than silently dropped.
#[test]
fn ups_update_attributes_refuses_what_it_does_not_implement() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // Unknown workitem.
    srv.update_json(
        UNKNOWN_WORKITEM,
        CLAIM_TUID,
        &performed_item_body(&["1.2.3.4"], None, None, None),
    )
    .assert_status(404);

    // A payload naming an attribute this implementation does not write. It
    // must be a refusal, not a 200 that quietly changed nothing: a client
    // that meant it has to find out. (0074,1000) Procedure Step State is
    // carried by the Change State transaction, never by an attribute update.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        br#"{"00741000":{"vr":"CS","Value":["COMPLETED"]}}"#,
    )
    .assert_status(400);

    // The FLAT top-level spellings of the performed report are refused too:
    // they are not the standard carriage, so accepting them would store one
    // fact in a shape a reader of the nested form would never look at.
    srv.update_json(WORKITEM_A, CLAIM_TUID, &output_body(&["1.2.3.4"]))
        .assert_status(400);

    // An empty payload says nothing -> 400 rather than a no-op 200.
    srv.update_json(WORKITEM_A, CLAIM_TUID, b"{}")
        .assert_status(400);

    // A present-but-empty output sequence claims "produced nothing" ->
    // 400, because omitting it is how a caller says nothing.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        br#"{"00741216":{"vr":"SQ","Value":[{"00404033":{"vr":"SQ","Value":[]}}]}}"#,
    )
    .assert_status(400);

    // None of the refusals touched the record.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    assert!(
        wi_nested_item(&r.json(), "00741216").is_none(),
        "a refused update must not have written anything"
    );
    assert_eq!(
        wi_tag(&r.json(), "00741204"),
        Some("CAD"),
        "label untouched"
    );
}

/// The two-step completion, end to end: Update with the outputs, then Change
/// State to COMPLETED. The state change leaves the output list intact, so a
/// completed workitem names its results.
#[test]
fn ups_two_step_completion_keeps_the_outputs() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // Step 1: Update (11.6) with the nested (0074,1216) report.
    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(
            &["1.2.826.0.1.3680043.10.7777.9"],
            None,
            None,
            Some(("20261010120000", "20261010123456")),
        ),
    )
    .assert_status(200);

    // Step 2: Change State (11.7) with the state and the UID and nothing else.
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_A}/state"),
        &state_change_body_with_uid("COMPLETED", CLAIM_TUID),
    )
    .assert_status(200);

    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let j = r.json();
    assert_eq!(wi_state(&j), Some("COMPLETED"));
    let item = wi_nested_item(&j, "00741216").expect("the performed item survives the transition");
    let instances: Vec<&str> = item
        .obj_get("00404033")
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.as_array())
        .expect("the completed workitem still names its outputs")
        .iter()
        .map(|item| {
            item.obj_get("00081155")
                .and_then(|v| v.obj_get("Value"))
                .and_then(|v| v.as_str_array_first())
                .expect("a referenced instance")
        })
        .collect();
    assert_eq!(instances, vec!["1.2.826.0.1.3680043.10.7777.9"]);
    assert_eq!(
        wi_performed_tag(&j, "00404051"),
        Some("20261010123456"),
        "the end time survives the completed transition"
    );
}

/// An Update publishes no event: 11.6 is an attribute modification, and a
/// subscriber woken by an attribute write would be told a transition
/// happened when none did.
#[test]
fn ups_update_attributes_publishes_no_event() {
    use std::io::Read;
    use std::net::TcpStream;

    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    // Open the per-workitem SSE stream, then update, then read what arrived.
    let mut stream = TcpStream::connect(srv.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(1200)))
        .expect("timeout");
    let req = format!(
        "GET /dicomweb/workitems/{WORKITEM_A}/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nAccept: text/event-stream\r\n\r\n",
        srv.addr.port()
    );
    stream.write_all(req.as_bytes()).expect("write req");
    std::thread::sleep(Duration::from_millis(250));

    srv.update_json(
        WORKITEM_A,
        CLAIM_TUID,
        &performed_item_body(&["1.2.3.4"], None, None, None),
    )
    .assert_status(200);

    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    assert!(
        !text.contains("event: StateChange"),
        "an attribute update must publish no state-change event; got: {text}"
    );
}

// -- Request Cancellation (PS3.18 11.8, product feature) -----------------

/// The cancel resource path for a workitem.
fn cancel_path(uid: &str, requester: Option<&str>) -> String {
    match requester {
        Some(ae) => format!("/dicomweb/workitems/{uid}/cancelrequest?requester={ae}"),
        None => format!("/dicomweb/workitems/{uid}/cancelrequest"),
    }
}

/// RED FACT. Request Cancellation (11.8) moves a queued (SCHEDULED) workitem
/// to CANCELED and answers 202 Accepted.
///
/// This is a PRODUCT FEATURE, not an AIW-I obligation: AIW-I requires
/// RAD-80..RAD-87 and RAD-109, and 11.8 has no RAD number. It exists because
/// the Task Requester (Weasis) cancels a workitem it does not own through
/// exactly this request. Before this the route did not exist at all, so the
/// requester got a 404 and the workitem stayed queued forever.
#[test]
fn ups_cancelrequest_cancels_a_scheduled_workitem() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);

    // A payload carrying a Reason for Cancellation (the only 11.8 field with
    // an attribute to live in).
    let body = br#"{"00741238":{"vr":"LT","Value":["no longer needed"]}}"#;
    let r = srv.post_json(&cancel_path(WORKITEM_A, Some("WEASIS")), body);
    r.assert_status(202);
    // The response body is the updated workitem, so the requester sees the
    // state it asked for.
    let j = r.json();
    assert_eq!(wi_state(&j), Some("CANCELED"));

    // The stored record carries the cancellation: state, reason and the
    // Procedure Step Cancellation DateTime.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    let j = r.json();
    assert_eq!(
        wi_state(&j),
        Some("CANCELED"),
        "SCHEDULED -> CANCELED through cancelrequest"
    );
    assert_eq!(wi_tag(&j, "00741238"), Some("no longer needed"));
    assert!(
        wi_tag(&j, "00404052").is_some(),
        "the Procedure Step Cancellation DateTime is a final-state requirement"
    );
}

/// The `requester` parameter is OPTIONAL (Table 11.7.2.1-1): an absent one is
/// not a 400, and the cancel still succeeds. The payload is optional too.
#[test]
fn ups_cancelrequest_works_without_a_requester_or_a_payload() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);

    let r = srv.post_json(&cancel_path(WORKITEM_A, None), b"");
    r.assert_status(202);
    assert_eq!(wi_state(&r.json()), Some("CANCELED"));
}

/// A terminal workitem cannot be canceled: COMPLETED and an already-CANCELED
/// workitem both answer 409. The repeat is a refusal because the state is
/// terminal - 11.8 names "Workitem in the COMPLETED state" as a conflict.
#[test]
fn ups_cancelrequest_refuses_a_terminal_workitem() {
    let srv = TestServer::start();

    // CANCELED (twice): the first cancel succeeds, the second is 409.
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    srv.post_json(&cancel_path(WORKITEM_A, None), b"")
        .assert_status(202);
    srv.post_json(&cancel_path(WORKITEM_A, None), b"")
        .assert_status(409);

    // COMPLETED: claim, complete, then try to cancel.
    srv.create_json(WORKITEM_B, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_B);
    srv.put_json(
        &format!("/dicomweb/workitems/{WORKITEM_B}/state"),
        &state_change_body_with_uid("COMPLETED", CLAIM_TUID),
    )
    .assert_status(200);
    srv.post_json(&cancel_path(WORKITEM_B, None), b"")
        .assert_status(409);
}

/// IN PROGRESS is refused with 409: this origin is the Task Manager, not the
/// performer, and the "UPS Cancel Requested" fan-out to the owner is not
/// implemented in this release - the status the standard names for "no
/// possibility the performer will be informed". Accepting would promise a
/// cancellation nobody performs.
#[test]
fn ups_cancelrequest_refuses_an_in_progress_workitem() {
    let srv = TestServer::start();
    srv.create_json(WORKITEM_A, &workitem_body("CAD"))
        .assert_status(201);
    claim(&srv, WORKITEM_A);

    let r = srv.post_json(&cancel_path(WORKITEM_A, None), b"");
    r.assert_status(409);
    let message = r
        .json()
        .obj_get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains("IN PROGRESS"),
        "the refusal names the state: {message}"
    );

    // The workitem is untouched: a refusal is not a partial cancel.
    let r = srv.get(&format!("/dicomweb/workitems/{WORKITEM_A}"), &[]);
    r.assert_status(200);
    assert_eq!(wi_state(&r.json()), Some("IN PROGRESS"));
}

/// An unknown workitem is 404.
#[test]
fn ups_cancelrequest_unknown_is_404() {
    let srv = TestServer::start();
    srv.post_json(&cancel_path(UNKNOWN_WORKITEM, None), b"")
        .assert_status(404);
}
