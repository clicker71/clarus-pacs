// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Wire-level proof that the SSE routes frame their bodies correctly.
//!
//! The incident this pins: both SSE handlers declared
//! `Transfer-Encoding: chunked` in the head and then wrote the body with
//! `write_all_configured`, i.e. RAW bytes with no chunk-size line. The head
//! promised chunked framing and the body did not deliver it, so every byte
//! after the head was ambiguous. A lenient client (curl, the project's own
//! smoke helpers that read to EOF) never noticed; a STRICT HTTP/1.1 parser
//! (httpx -> h11, which the requester sidecars use) reads the first payload
//! byte as a chunk size, fails to parse it, and waits for a valid frame that
//! never comes - while the 15 s keep-alive keeps resetting its read timeout,
//! so the wait never ends.
//!
//! WHY THESE TESTS LOOK THE WAY THEY DO. The existing SSE smoke tests read the
//! whole response to EOF as opaque bytes, which is exactly the weak client
//! behaviour that hid the bug. These tests instead DECODE the chunk framing
//! themselves - size line, payload, CRLF, terminating zero-size chunk - so a
//! body that is not chunked fails here even though it looks fine to `curl`.

#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Read ONE chunked frame from `r` and return (size_line, payload).
///
/// ONE frame, deliberately, and not "read the body to the end": both SSE
/// streams here are INFINITE by design - the UPS stream holds the connection
/// until the workitem settles, and the admin stream follows the log until the
/// reader leaves. Reading to EOF would block until the test's own read
/// timeout, which is a hang, not a check.
///
/// This is a deliberate re-implementation of what a strict client does, and
/// the point of the test: if the server wrote raw bytes, the first "size line"
/// will not parse as hex and this fails with a clear message.
fn read_one_chunk(r: &mut BufReader<TcpStream>) -> (String, String) {
    let mut size_line = String::new();
    r.read_line(&mut size_line).expect("chunk size line");
    // A chunk-size line is `{hex}[;ext]\r\n`.
    let size_str = size_line
        .trim_end_matches(['\r', '\n'])
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(
        !size_str.is_empty() && size_str.chars().all(|c| c.is_ascii_hexdigit()),
        "body is NOT chunk-framed: expected a hex chunk-size line, got {size_line:?}. \
         A raw write after `Transfer-Encoding: chunked` looks exactly like this."
    );
    let size = usize::from_str_radix(&size_str, 16).expect("hex size");
    if size == 0 {
        return (size_str, String::new());
    }
    let mut payload = vec![0u8; size];
    r.read_exact(&mut payload).expect("chunk payload");
    let mut crlf = [0u8; 2];
    r.read_exact(&mut crlf).expect("chunk trailing CRLF");
    assert_eq!(&crlf, b"\r\n", "each chunk must end with CRLF");
    (size_str, String::from_utf8_lossy(&payload).into_owned())
}

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

    /// Open a stream and send a request head, returning the reader positioned
    /// at the body (the head is consumed and its notable fields returned).
    ///
    /// The read timeout must exceed the server's 15 s SSE keep-alive: the
    /// first body byte of an idle event stream IS that keep-alive, so a shorter
    /// timeout would fail a healthy server.
    fn open(&self, path: &str, extra: &[(&str, &str)]) -> (BufReader<TcpStream>, String) {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(25)))
            .expect("read timeout");
        let mut head = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: text/event-stream\r\nConnection: close\r\n",
            self.addr.port()
        );
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).expect("write head");

        let mut reader = BufReader::new(stream);
        let mut head_lines = String::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("read head line");
            if n == 0 || line == "\r\n" {
                break;
            }
            head_lines.push_str(&line);
        }
        (reader, head_lines)
    }

    fn post_json(&self, path: &str, body: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.addr.port(),
            body.len()
        );
        let mut req = head.into_bytes();
        req.extend_from_slice(body);
        stream.write_all(&req).expect("write req");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status = text
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, text)
    }
}

fn workitem_body(uid: &str, study: &str) -> String {
    format!(
        r#"{{"00404021":{{"vr":"SQ","Value":[{{"0020000D":{{"vr":"UI","Value":["{study}"]}}}}]}},"00741000":{{"vr":"CS","Value":["SCHEDULED"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"LO","Value":["CAD"]}},"00404005":{{"vr":"DT","Value":["20260101120000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00404025":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["STATION01"]}},"00080102":{{"vr":"SH","Value":["99CLARUS"]}},"00080104":{{"vr":"LO","Value":["AI station"]}}}}]}}}}"#
    )
    .replace("{uid}", uid)
}

/// THE incident test: the event stream's body must be real chunked framing.
///
/// Before the fix the head said `Transfer-Encoding: chunked` and the body was
/// raw `: keep-alive\n\n`, so `decode_chunked` here would read ": keep-" as a
/// size line and fail. After the fix the first frame is a well-formed chunk.
#[test]
fn the_ups_event_stream_frames_its_body_as_chunks() {
    let srv = TestServer::start();
    let study = "1.2.826.0.1.3680043.10.1234.9001";
    let (status, _) = srv.post_json(
        "/dicomweb/workitems?workitem=1.2.826.0.1.3680043.10.1234.9000",
        workitem_body("1.2.826.0.1.3680043.10.1234.9000", study).as_bytes(),
    );
    assert!(status == 200 || status == 201, "create -> {status}");

    let (mut reader, head) = srv.open(
        "/dicomweb/workitems/1.2.826.0.1.3680043.10.1234.9000/events",
        &[],
    );
    assert!(head.contains("200"), "head: {head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "the stream must declare chunked: {head}"
    );

    // The first frame the handler writes is the SSE keep-alive comment. Decode
    // it through the chunked reader: if the body were raw this fails with the
    // "body is NOT chunk-framed" message.
    let (size, payload) = read_one_chunk(&mut reader);
    assert!(
        payload.starts_with(": keep-alive"),
        "first chunked frame must be the SSE keep-alive comment, got {payload:?} (size line {size:?})"
    );
}

/// The admin log stream has the same defect and the same fix.
#[test]
fn the_admin_log_stream_frames_its_body_as_chunks() {
    let srv = TestServer::start();
    let (mut reader, head) = srv.open("/admin/v1/logs/stream", &[("Accept", "text/event-stream")]);
    assert!(
        head.contains("200"),
        "the admin stream should answer 200 (admin gate is loopback in tests): {head}"
    );
    if !head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        // The admin gate may answer 403 for a non-loopback test client; that is
        // a different concern from framing and is asserted elsewhere.
        assert!(
            head.contains("403"),
            "expected either a chunked stream or an admin-gate refusal: {head}"
        );
        return;
    }
    let (size, payload) = read_one_chunk(&mut reader);
    // The snapshot phase writes one frame per buffered entry; with an empty
    // buffer the stream still emits its tick within 500 ms, so the first chunk
    // is whichever came first - either is fine, both must be framed.
    assert!(
        payload.starts_with("data:") || payload.starts_with(":") || payload.starts_with("event:"),
        "first chunked frame must be an SSE frame, got {payload:?} (size line {size:?})"
    );
}

/// `0020000D` is the STUDY key: it must return that study's workitems.
///
/// Routing it into `StoreFilter.workitem_uid` compared it against the
/// workitem's own UID, which a study UID only equals by coincidence, so the
/// search returned nothing at all.
#[test]
fn search_by_study_uid_returns_that_studys_workitems() {
    let srv = TestServer::start();
    let study_a = "1.2.826.0.1.3680043.10.1234.9101";
    let study_b = "1.2.826.0.1.3680043.10.1234.9102";

    for (uid, study) in [
        ("1.2.826.0.1.3680043.10.1234.9001", study_a),
        ("1.2.826.0.1.3680043.10.1234.9002", study_a),
        ("1.2.826.0.1.3680043.10.1234.9003", study_b),
    ] {
        let (status, _) = srv.post_json(
            &format!("/dicomweb/workitems?workitem={uid}"),
            workitem_body(uid, study).as_bytes(),
        );
        assert!(status == 200 || status == 201, "create {uid} -> {status}");
    }

    // GET search by the study key.
    let mut stream = TcpStream::connect(srv.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "GET /dicomweb/workitems?0020000D={study_a} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/dicom+json\r\nConnection: close\r\n\r\n",
        srv.addr.port()
    );
    stream.write_all(head.as_bytes()).expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");

    assert!(
        body.contains("9001"),
        "study A's first workitem must be found: {body}"
    );
    assert!(
        body.contains("9002"),
        "study A's second workitem must be found: {body}"
    );
    assert!(
        !body.contains("9003"),
        "study B's workitem must NOT be returned for a study A query: {body}"
    );
}
