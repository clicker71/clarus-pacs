// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Dispatch loopback unit tests.
//!
//! Exercises the synchronous dispatch layer over a real loopback TCP pair
//! (`TcpListener` on `127.0.0.1:0` + `TcpStream`), without the accept loop.
//! A worker thread parses the head, matches the route, dispatches to
//! the pure handler, and writes the reply. We assert status line, headers, and
//! body byte-for-byte.
//!
//! Acceptance representatives: health, qido_studies (query + includefield),
//! qido_series_instances (UID from segments), ups_search, admin_status,
//! capabilities, viewer, wado_metadata, 404, 405+Allow, plus a body-route test
//! (ups_create) and the 1 MiB body limit -> 413.

#![forbid(unsafe_code)]

use clarus_core::http::conn::{Conn, HEADER_BUF_SIZE};
use clarus_core::http::parse::{parse_headers, parse_request_line};
use clarus_core::http::router::{RouterOutcome, match_route};
use clarus_server::config::Config;
use clarus_server::dispatch::{RequestContext, dispatch};
use clarus_server::shadow::ShadowTier;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::{Duration, Instant};

// -- Response capture -----------------------------------------------------

struct Response {
    status: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
}

/// Parse an HTTP/1.1 response buffer into status line, headers, body.
fn parse_response(raw: &[u8]) -> Response {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head terminator");
    let (head_bytes, body) = (&raw[..split], &raw[split + 4..]);
    let head_str = String::from_utf8_lossy(head_bytes);
    let mut lines = head_str.split("\r\n");
    let status = lines.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(": ") {
            headers.push((k.to_string(), v.to_string()));
        } else if let Some((k, v)) = line.split_once(":") {
            headers.push((k.to_string(), v.trim_start().to_string()));
        }
    }
    Response {
        status,
        headers,
        body: body.to_vec(),
    }
}

// -- Test setup ----------------------------------------------------------

struct TestCtx {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
}

fn setup() -> TestCtx {
    let temp = tempfile::tempdir().expect("temp dir");

    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&idx_dir).unwrap();

    let mut config = Config::test_default();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.port = 0;
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;
    let storage_dir = temp.path().join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();
    config.storage_dir = storage_dir.to_string_lossy().to_string();

    let cas = clarus_adapters::storage::ContentAddressedStorage::new(temp.path().join("cas"))
        .expect("open cas");

    let state = AppState {
        cas,
        codec_registry: clarus_adapters::codec::CodecRegistry::default(),
        ups_events: clarus_server::sse_fanout::DualEventSink::new(),
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

    TestCtx {
        _temp: temp,
        state: Arc::new(state),
    }
}

// -- Dispatch harness -----------------------------------------------------

/// Finish a request so the peer sees both the reply and EOF.
///
/// Returning the stream as the thread's value kept the socket open inside the
/// unjoined `JoinHandle`, so every reply ended at the client's read timeout
/// instead of at EOF. Dropping it outright aborts a peer that is still sending
/// its body, and the abort can discard the reply. Shutting down the write half
/// first sends FIN, then draining the read half absorbs anything still in
/// flight, so the close is orderly and the reply survives.
fn close_politely(mut conn: Conn) {
    let _ = conn.stream().shutdown(std::net::Shutdown::Write);
    // Drain what the client still had in flight, so the kernel does not answer
    // the close with a reset that discards the reply. Short and bounded: the
    // reply is already out, so this waits only for a tail that is arriving now.
    let _ = conn
        .stream()
        .set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 4096];
    while let Ok(n) = std::io::Read::read(conn.stream_mut(), &mut buf) {
        if n == 0 {
            break;
        }
    }
}

/// Send one raw request through the dispatch layer and read the reply.
fn serve(state: &Arc<AppState>, method: &str, path: &str, body: &[u8]) -> Response {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let state = Arc::clone(state);
    let worker = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut conn = Conn::new(stream, b"127.0.0.1".to_vec());
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let n = conn
            .read_head(&mut head_buf, Duration::from_secs(10))
            .expect("read head");
        let head_buf = &head_buf[..n];
        let (mut head, off) = parse_request_line(head_buf).expect("line");
        // `off` points at the request-line CRLF; headers begin after it.
        let _ = parse_headers(&mut head, &head_buf[off + 2..]).expect("hdrs");
        match match_route(head.method, head.path) {
            RouterOutcome::Match(route) => {
                let mut ctx = RequestContext {
                    head: &head,
                    uids: route.uids,
                    n_uids: route.n_uids,
                    client_ip: b"127.0.0.1",
                    user: b"anonymous",
                    via: "loopback",
                    state: &state,
                    // The real loop ANDs the route's preference with the request's
                    // Connection header (serve.rs). The client here reads until EOF,
                    // so a keep-alive reply would wait out its whole timeout.
                    keep_alive: if head.connection_close {
                        clarus_core::http::router::KeepAlive::Off
                    } else {
                        route.keep_alive
                    },
                    conn: &mut conn,
                    wado_series_expected: None,
                    stow_accepted: 0,
                    proxy_trusted: false,
                    route,
                };
                let _ = dispatch(&route, &mut ctx);
            }
            RouterOutcome::NotFound => {
                let _ = conn.send_error(404, "Not Found", false);
            }
            RouterOutcome::MethodNotAllowed { allow } => {
                conn.header
                    .extend_from_slice(format!("HTTP/1.1 405 Method Not Allowed\r\nAllow: {allow}\r\nContent-Length: 0\r\n\r\n").as_bytes());
                let _ = conn.write_head();
            }
            RouterOutcome::BadUid => {
                let _ = conn.send_error(400, "Bad Request", false);
            }
            RouterOutcome::Options => {
                let _ = conn.send_error(204, "No Content", false);
            }
        }
        // Close politely so the reply is not lost. Returning the stream kept
        // it alive inside the unjoined JoinHandle, so the reply ended at the
        // client's read timeout; dropping it outright aborts a peer that is
        // still sending, and the abort discards the response. Shutting down
        // the write half signals EOF, then draining absorbs whatever the
        // client still had in flight before the socket goes away.
        close_politely(conn);
    });

    let mut c = TcpStream::connect(addr).expect("connect");
    let mut head_bytes = method.as_bytes().to_vec();
    head_bytes.extend_from_slice(b" ");
    head_bytes.extend_from_slice(path.as_bytes());
    head_bytes.extend_from_slice(b" HTTP/1.1\r\nHost: x\r\nContent-Length: ");
    head_bytes.extend_from_slice(body.len().to_string().as_bytes());
    head_bytes
        .extend_from_slice(b"\r\nAccept: application/dicom+json\r\nConnection: close\r\n\r\n");
    c.write_all(&head_bytes).unwrap();
    // `read_head` terminates on the head's `\r\n\r\n` tail; when a body is sent
    // in the same syscall it would be consumed into the head buffer instead of
    // left on the socket for the body reader. A short gap keeps the head and
    // the body in separate reads on loopback.
    if !body.is_empty() {
        std::thread::sleep(Duration::from_millis(30));
    }
    c.write_all(body).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let _ = worker.join();
    parse_response(&all)
}

/// Same as `serve`, but with an explicit request `Content-Type`. The STOW
/// media-type contract is decided by that header alone, so the default
/// `serve` (which sends none) cannot exercise it.
fn serve_with_ct(
    state: &Arc<AppState>,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> Response {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let state = Arc::clone(state);
    let worker = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut conn = Conn::new(stream, b"127.0.0.1".to_vec());
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let n = conn
            .read_head(&mut head_buf, Duration::from_secs(10))
            .expect("read head");
        let head_buf = &head_buf[..n];
        let (mut head, off) = parse_request_line(head_buf).expect("line");
        let _ = parse_headers(&mut head, &head_buf[off + 2..]).expect("hdrs");
        match match_route(head.method, head.path) {
            RouterOutcome::Match(route) => {
                let mut ctx = RequestContext {
                    head: &head,
                    uids: route.uids,
                    n_uids: route.n_uids,
                    client_ip: b"127.0.0.1",
                    user: b"anonymous",
                    via: "loopback",
                    state: &state,
                    // The real loop ANDs the route's preference with the request's
                    // Connection header (serve.rs). The client here reads until EOF,
                    // so a keep-alive reply would wait out its whole timeout.
                    keep_alive: if head.connection_close {
                        clarus_core::http::router::KeepAlive::Off
                    } else {
                        route.keep_alive
                    },
                    conn: &mut conn,
                    wado_series_expected: None,
                    stow_accepted: 0,
                    proxy_trusted: false,
                    route,
                };
                let _ = dispatch(&route, &mut ctx);
            }
            RouterOutcome::NotFound => {
                let _ = conn.send_error(404, "Not Found", false);
            }
            RouterOutcome::MethodNotAllowed { allow } => {
                conn.header
                    .extend_from_slice(format!("HTTP/1.1 405 Method Not Allowed\r\nAllow: {allow}\r\nContent-Length: 0\r\n\r\n").as_bytes());
                let _ = conn.write_head();
            }
            RouterOutcome::BadUid => {
                let _ = conn.send_error(400, "Bad Request", false);
            }
            RouterOutcome::Options => {
                let _ = conn.send_error(204, "No Content", false);
            }
        }
        // Close politely so the reply is not lost. Returning the stream kept
        // it alive inside the unjoined JoinHandle, so the reply ended at the
        // client's read timeout; dropping it outright aborts a peer that is
        // still sending, and the abort discards the response. Shutting down
        // the write half signals EOF, then draining absorbs whatever the
        // client still had in flight before the socket goes away.
        close_politely(conn);
    });

    let mut c = TcpStream::connect(addr).expect("connect");
    let mut head_bytes = method.as_bytes().to_vec();
    head_bytes.extend_from_slice(b" ");
    head_bytes.extend_from_slice(path.as_bytes());
    head_bytes.extend_from_slice(b" HTTP/1.1\r\nHost: x\r\nContent-Type: ");
    head_bytes.extend_from_slice(content_type.as_bytes());
    head_bytes.extend_from_slice(b"\r\nContent-Length: ");
    head_bytes.extend_from_slice(body.len().to_string().as_bytes());
    head_bytes
        .extend_from_slice(b"\r\nAccept: application/dicom+json\r\nConnection: close\r\n\r\n");
    c.write_all(&head_bytes).unwrap();
    if !body.is_empty() {
        std::thread::sleep(Duration::from_millis(30));
    }
    c.write_all(body).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let _ = worker.join();
    parse_response(&all)
}

// -- Route table integrity -----------------------------------------------
//
// Contract (owner, 13.08): the dispatch table is a data table, not a `match`.
// The integrity test must prove:
//   (a) route ids are unique;
//   (b) EVERY id the router can produce is covered - in ROUTES or in the
//       explicit deferred list (streaming endpoints still to arrive);
//   (c) the table has no dangling entries - every ROUTES id is router-produced.
// ROUTER_IDS is the authoritative mirror of `clarus_core::http::router`;
// `router_corpus_matches_exactly` proves the mirror is complete and current.

/// Every id the new router (`clarus_core::http::router`) can produce.
const ROUTER_IDS: &[&str] = &[
    "health",
    "health_status",
    "metrics",
    "version",
    "viewer",
    "capabilities",
    "capabilities_redirect",
    "commitment_commit",
    "commitment_check",
    "qido_patients",
    "qido_studies",
    "qido_study_instances",
    "qido_series",
    "qido_series_instances",
    "qido_series_global",
    "qido_series_root_instances",
    "qido_instances_global",
    "ups_search",
    "ups_get",
    // PS3.18 14/15: the MWL search and the three MPPS transactions. Create
    // and Update share one id because the router cannot see the `?update`
    // parameter that tells them apart (the same reason `ups_create` covers
    // both UPS Create and search-with-body).
    "mwl_search",
    "mpps_create_or_update",
    "mpps_get",
    // `ups_search_post` is gone: the POST search-with-body shares the
    // worklist resource with Create (PS3.18 11.9 vs 11.4), and the router
    // cannot see the query string that tells them apart, so ONE route id
    // (`ups_create`) covers both and the dispatcher chooses.
    "wado_uri",
    "wado_study",
    "wado_study_metadata",
    "wado_study_thumbnail",
    "wado_series",
    "wado_series_metadata",
    "wado_series_thumbnail",
    "wado_instance",
    "wado_metadata",
    "wado_thumbnail",
    "wado_rendered",
    "wado_frame",
    "wado_frame_thumbnail",
    "wado_bulkdata",
    "ups_events",
    "ups_notification_connect",
    "stow_studies",
    // PS3.18 10.5.1.1 Table 10.5.1-1: `/studies/{study}` is the second target
    // of the Store Transaction, so the router produces a second STOW id.
    "stow_study",
    "ups_create",
    "ups_update",
    "ups_update_attributes",
    "ups_cancelrequest",
    "ups_delete",
    "ups_subscribe",
    "ups_subscribe_filtered",
    "ups_subscribe_workitem",
    "ups_unsubscribe",
    "ups_unsubscribe_workitem",
    "admin_status",
    "admin_health",
    "admin_version",
    "admin_dashboard",
    "admin_audit_log",
    "admin_access_log",
    "admin_audit_copies",
    "admin_storage",
    "admin_logs_stream",
    "admin_metrics_history",
    "admin_recovery_rebuild",
    "admin_recovery_rebuild_status",
    "admin_instances_search",
    "admin_storage_s3",
    "admin_cas_audit_status",
    "admin_maintenance",
    "admin_storage_migrate",
    "admin_instances_delete",
    "admin_instances_hidden",
    "admin_instances_raw",
    "admin_instances_restore",
    "admin_cas_audit",
];

/// Streaming endpoints deferred from ROUTES.
///
/// The streaming transports (sendfile WADO, chunked
/// multipart, STOW shim, SSE fan-out, logs stream) out of the table until 8b.
/// 8b wired all of them, so nothing is deferred and the list is empty - the
/// integrity test below proves every router id resolves in ROUTES.
const DEFERRED_8B: &[&str] = &[];

#[test]
fn route_table_integrity_contract() {
    use clarus_server::dispatch::ROUTES;

    // (a) ROUTES ids unique.
    let mut seen = std::collections::HashSet::new();
    for (id, _) in ROUTES {
        assert!(seen.insert(*id), "duplicate route id in ROUTES: {id}");
    }

    // (b) Full corpus coverage: every router id is in ROUTES or deferred to 8b.
    let routes: std::collections::HashSet<&str> = ROUTES.iter().map(|(id, _)| *id).collect();
    let deferred: std::collections::HashSet<&str> = DEFERRED_8B.iter().copied().collect();
    for id in ROUTER_IDS {
        assert!(
            routes.contains(id) || deferred.contains(id),
            "router id {id} not covered by ROUTES or DEFERRED_8B",
        );
    }

    // (c) No dangling entries: every ROUTES id is produced by the router.
    let router_ids: std::collections::HashSet<&str> = ROUTER_IDS.iter().copied().collect();
    for (id, _) in ROUTES {
        assert!(
            router_ids.contains(id),
            "dangling ROUTES id {id} not produced by router",
        );
    }

    // DEFERRED_8B must be unique and disjoint from ROUTES.
    let mut seen_deferred = std::collections::HashSet::new();
    for id in DEFERRED_8B {
        assert!(seen_deferred.insert(*id), "duplicate DEFERRED_8B id {id}");
        assert!(
            !routes.contains(id),
            "DEFERRED_8B id {id} is already in ROUTES",
        );
    }
}

#[test]
fn router_corpus_matches_exactly() {
    // The path corpus must produce EXACTLY ROUTER_IDS - proves the mirror
    // list is complete (no forgotten id) and current (no removed id).
    let corpus = router_corpus();
    let expected: std::collections::HashSet<&'static str> = ROUTER_IDS.iter().copied().collect();
    assert_eq!(corpus, expected, "router corpus != ROUTER_IDS mirror");
}

fn router_corpus() -> std::collections::HashSet<&'static str> {
    use clarus_core::http::parse::Method;
    let mut ids = std::collections::HashSet::new();
    let gets: &[&[u8]] = &[
        b"/health",
        b"/health/status",
        b"/metrics",
        b"/version",
        b"/",
        b"/dicomweb/",
        b"/dicomweb",
        b"/dicomweb/patients",
        b"/dicomweb/studies",
        b"/dicomweb/series",
        b"/dicomweb/series/1.2.3/instances",
        b"/dicomweb/instances",
        b"/dicomweb/workitems",
        b"/wadouri",
        b"/dicomweb/studies/1.2.3",
        b"/dicomweb/studies/1.2.3/instances",
        b"/dicomweb/studies/1.2.3/series",
        b"/dicomweb/studies/1.2.3/thumbnail",
        // PS3.18 10.4.1.1.2 Table 10.4.1-2: Study Metadata is a Metadata
        // Resource, so /studies/{study}/metadata is a fourth-segment target.
        b"/dicomweb/studies/1.2.3/metadata",
        b"/dicomweb/studies/1.2.3/series/4.5",
        b"/dicomweb/studies/1.2.3/series/4.5/instances",
        b"/dicomweb/studies/1.2.3/series/4.5/metadata",
        b"/dicomweb/studies/1.2.3/series/4.5/thumbnail",
        b"/dicomweb/studies/1.2.3/series/4.5/instances/6.7",
        b"/dicomweb/studies/1.2.3/series/4.5/instances/6.7/metadata",
        b"/dicomweb/studies/1.2.3/series/4.5/instances/6.7/thumbnail",
        b"/dicomweb/studies/1.2.3/series/4.5/instances/6.7/rendered",
        b"/dicomweb/studies/1.2.3/series/4.5/instances/6.7/frames/0",
        b"/dicomweb/studies/1.2.3/series/4.5/instances/6.7/frames/0/thumbnail",
        b"/dicomweb/studies/1.2.3/series/4.5/instances/6.7/bulkdata/00100010",
        b"/dicomweb/workitems/1.2.3/events",
        b"/dicomweb/subscribers/CLARUS",
        b"/dicomweb/subscribers/CLARUS",
        b"/dicomweb/workitems/1.2.3",
        b"/dicomweb/commitment-requests/1.2.3",
        // PS3.18 14.4: MWL Search on the collection resource.
        b"/dicomweb/modality-scheduled-procedure-steps",
        // PS3.18 15.6: MPPS Retrieve (N-GET).
        b"/dicomweb/modality-performed-procedure-steps/1.2.3",
        b"/admin/v1/status",
        b"/admin/v1/health",
        b"/admin/v1/version",
        b"/admin/v1/dashboard",
        b"/admin/v1/audit-log",
        b"/admin/v1/access-log",
        b"/admin/v1/storage",
        b"/admin/v1/logs/stream",
        b"/admin/v1/metrics/history",
        b"/admin/v1/instances/search",
        b"/admin/v1/instances/hidden",
        b"/admin/v1/instances/raw",
        b"/admin/v1/storage/s3-validation",
        b"/admin/v1/storage/cas-audit/status",
        b"/admin/v1/storage/audit-copies",
        // rebuild status GET.
        b"/admin/v1/recovery/rebuild/status",
    ];
    for p in gets {
        if let RouterOutcome::Match(m) = match_route(Method::Get, p) {
            ids.insert(m.id);
        }
    }
    let posts: &[&[u8]] = &[
        b"/dicomweb/studies",
        // PS3.18 10.5.1.1 Table 10.5.1-1: the study-scoped Store Transaction
        // target, distinct from the root path above.
        b"/dicomweb/studies/1.2.3",
        b"/dicomweb/workitems",
        // PS3.18 11.6.1: Update is a POST on the workitem resource. The
        // corpus needs this path for the `ups_update_attributes` id, which
        // the worklist path above cannot produce.
        b"/dicomweb/workitems/1.2.3",
        // PS3.18 11.8: Request Cancellation is a POST on the workitem
        // resource plus the `cancelrequest` operation segment.
        b"/dicomweb/workitems/1.2.3/cancelrequest",
        // PS3.18 15.4/15.5: MPPS Create and Update share the POST resource.
        b"/dicomweb/modality-performed-procedure-steps/1.2.3",
        b"/dicomweb/commitment-requests/1.2.3",
        b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/CLARUS",
        b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLARUS",
        // Workitem Subscription template (PS3.18 11.10.1.1): a real
        // workitem UID in the subscription position.
        b"/dicomweb/workitems/1.2.3.4.5/subscribers/CLARUS",
        b"/admin/v1/maintenance",
        b"/admin/v1/storage/migrate",
        b"/admin/v1/instances/delete",
        b"/admin/v1/instances/restore",
        b"/admin/v1/storage/cas-audit/start",
        // rebuild POST.
        b"/admin/v1/recovery/rebuild",
    ];
    for p in posts {
        if let RouterOutcome::Match(m) = match_route(Method::Post, p) {
            ids.insert(m.id);
        }
    }
    let put_delete: &[&[u8]] = &[
        b"/dicomweb/workitems/1.2.3",
        b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/CLARUS",
        b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5.1/subscribers/CLARUS",
        // Workitem Subscription template (PS3.18 11.10.1.1): a real
        // workitem UID in the subscription position.
        b"/dicomweb/workitems/1.2.3.4.5/subscribers/CLARUS",
    ];
    for p in put_delete {
        if let RouterOutcome::Match(m) = match_route(Method::Put, p) {
            ids.insert(m.id);
        }
        if let RouterOutcome::Match(m) = match_route(Method::Delete, p) {
            ids.insert(m.id);
        }
    }
    ids
}

// -- Representative handlers ---------------------------------------------

#[test]
fn health_is_200() {
    let tc = setup();
    let resp = serve(&tc.state, "GET", "/health", &[]);
    assert!(resp.status.starts_with("HTTP/1.1 200"), "{}", resp.status);
}

/// `GET /version` identifies the binary: version, git revision and commit
/// date. The stamp is what lets two field builds be told apart, so an empty or
/// placeholder revision would defeat the whole route.
#[test]
fn version_reports_a_build_stamp() {
    let tc = setup();
    let resp = serve(&tc.state, "GET", "/version", &[]);
    assert_eq!(resp.status, "HTTP/1.1 200 OK");
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("text/plain; charset=utf-8")
    );
    let body = String::from_utf8_lossy(&resp.body).to_string();
    assert!(body.starts_with("clarus "), "got: {body}");
    assert!(
        body.contains(clarus_server::version::VERSION),
        "got: {body}"
    );
    assert!(
        body.contains(clarus_server::version::GIT_REV),
        "got: {body}"
    );
    assert!(
        body.contains(clarus_server::version::COMMIT_DATE),
        "got: {body}"
    );
    // The stamp describes the CODE, not the build moment: the date is the one
    // git reports for the commit, never a clock reading taken now.
    assert!(
        !clarus_server::version::GIT_REV.is_empty(),
        "git revision must never be an empty string"
    );
    assert_eq!(
        clarus_server::version::COMMIT_DATE.len(),
        10,
        "commit date must be YYYY-MM-DD, got {}",
        clarus_server::version::COMMIT_DATE
    );
}

/// `/health/status` stays valid JSON and carries the build stamp alongside
/// the operational indicators the viewer reads.
#[test]
fn health_status_carries_version_and_build() {
    let tc = setup();
    let resp = serve(&tc.state, "GET", "/health/status", &[]);
    assert_eq!(resp.status, "HTTP/1.1 200 OK");
    let body = String::from_utf8_lossy(&resp.body).to_string();
    assert!(body.starts_with('{') && body.ends_with('}'), "got: {body}");
    assert!(
        body.contains(&format!(
            "\"version\":\"{}\"",
            clarus_server::version::VERSION
        )),
        "got: {body}"
    );
    assert!(body.contains("\"build\":\""), "got: {body}");
    // Adding the pair must not disturb the fields the viewer already reads.
    assert!(body.contains("\"mnf\":\""), "got: {body}");
    assert!(body.contains("\"identity\":{"), "got: {body}");
    // 0810: the CAS audit tri-state + counts drive the viewer red badge.
    assert!(body.contains("\"audit_state\":\""), "got: {body}");
    assert!(body.contains("\"audit_corrupted\":"), "got: {body}");
    assert!(body.contains("\"audit_unreadable\":"), "got: {body}");
}

#[test]
fn qido_studies_200_with_query() {
    let tc = setup();
    let resp = serve(
        &tc.state,
        "GET",
        "/dicomweb/studies?includefield=00080060",
        &[],
    );
    assert!(resp.status.starts_with("HTTP/1.1 200"), "{}", resp.status);
    assert_eq!(resp.header("x-pragma").as_deref(), Some("once"));
    assert!(resp.body.starts_with(b"[") || resp.body.starts_with(b"{"));
}

#[test]
fn qido_series_instances_uid_from_segments() {
    let tc = setup();
    // /dicomweb/studies/{study}/series/{series}/instances (route qido_series_instances)
    let resp = serve(
        &tc.state,
        "GET",
        "/dicomweb/studies/1.2.3.4/series/5.6.7/instances",
        &[],
    );
    assert_eq!(resp.status, "HTTP/1.1 200 OK");
}

#[test]
fn qido_series_uid_from_segments() {
    let tc = setup();
    // /dicomweb/studies/{study}/series (route qido_series, PS3.18 SearchForSeries)
    let resp = serve(&tc.state, "GET", "/dicomweb/studies/1.2.3.4/series", &[]);
    assert_eq!(resp.status, "HTTP/1.1 200 OK");
}

#[test]
fn ups_search_200() {
    let tc = setup();
    // The standard Procedure Step State key (0074,1000).
    let resp = serve(
        &tc.state,
        "GET",
        "/dicomweb/workitems?00741000=IN+PROGRESS",
        &[],
    );
    assert_eq!(resp.status, "HTTP/1.1 200 OK");
}

#[test]
fn admin_status_200() {
    let tc = setup();
    let resp = serve(&tc.state, "GET", "/admin/v1/status", &[]);
    assert_eq!(resp.status, "HTTP/1.1 200 OK");
}

#[test]
fn capabilities_200() {
    let tc = setup();
    let resp = serve(&tc.state, "GET", "/dicomweb/", &[]);
    assert!(resp.status.starts_with("HTTP/1.1 200"), "{}", resp.status);
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("application/vnd.sun.wadl+xml")
    );
    assert!(String::from_utf8_lossy(&resp.body).contains("<application"));
}

/// PS3.18 Table 8.9.2-1 / Table 11.1.1-1: the capabilities document advertises
/// the worklist subscription resource under the UPS workitem, and the workitem
/// resource itself is intact.
#[test]
fn capabilities_advertises_worklist_subscriptions() {
    let tc = setup();
    let resp = serve(&tc.state, "GET", "/dicomweb/", &[]);
    assert!(resp.status.starts_with("HTTP/1.1 200"), "{}", resp.status);

    let body = String::from_utf8_lossy(&resp.body).into_owned();
    assert!(
        body.contains(r#"<resource path="subscribers">"#),
        "worklist subscription resource missing: {body}"
    );
    assert!(
        body.contains(r#"<resource path="{subscriber}">"#),
        "subscriber resource template missing: {body}"
    );
    assert!(
        body.contains(r#"<method name="DELETE" id="Delete Subscription"/>"#),
        "Delete Subscription method missing: {body}"
    );
    // The pre-existing UPS resource must not have been disturbed.
    assert!(
        body.contains(r#"<resource path="workitems">"#),
        "existing UPS resource changed: {body}"
    );
    assert!(
        body.contains(r#"<method name="GET" id="Search for Workitems"/>"#),
        "existing UPS Search method changed: {body}"
    );
    assert!(
        body.contains(r#"<method name="POST" id="Create Workitem"/>"#),
        "existing UPS Create method changed: {body}"
    );
}

/// `X-Accel-Buffering: no` must be in the HEAD of the workitem event stream so
/// nginx stops buffering it. A header written into the body would be silently
/// ignored by the proxy, so the placement is asserted, not just the presence.
///
/// The stream stays open, so the test reads until it has the head terminator
/// and then stops - it must not wait for the body to end. The workitem must
/// exist first, or the handler answers 404 and never opens a stream.
#[test]
fn workitem_events_disables_proxy_buffering() {
    let tc = setup();

    // Create a real workitem so the events route streams instead of 404ing.
    // The body carries the type-1 set of PS3.4 Table CC.2.5-3: there is no
    // Modality, because UPS defines no such attribute.
    const WI: &str = "1.2.3.4.5";
    let body = "{\"00404005\":{\"vr\":\"DT\",\"Value\":[\"20260917T103000\"]},\
          \"00404041\":{\"vr\":\"CS\",\"Value\":[\"READY\"]},\
          \"00741200\":{\"vr\":\"CS\",\"Value\":[\"MEDIUM\"]},\
          \"00741204\":{\"vr\":\"SH\",\"Value\":[\"CAD\"]}}";
    let created = serve(
        &tc.state,
        "POST",
        &format!("/dicomweb/workitems?workitem={WI}"),
        body.as_bytes(),
    );
    assert_eq!(created.status, "HTTP/1.1 201 Created", "{}", created.status);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let state = Arc::clone(&tc.state);

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut conn = Conn::new(stream, b"127.0.0.1".to_vec());
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let n = conn
            .read_head(&mut head_buf, Duration::from_secs(10))
            .expect("read head");
        let head_buf = &head_buf[..n];
        let (mut head, off) = parse_request_line(head_buf).expect("line");
        let _ = parse_headers(&mut head, &head_buf[off + 2..]).expect("hdrs");
        match match_route(head.method, head.path) {
            RouterOutcome::Match(route) => {
                let mut ctx = RequestContext {
                    head: &head,
                    uids: route.uids,
                    n_uids: route.n_uids,
                    client_ip: b"127.0.0.1",
                    user: b"anonymous",
                    via: "loopback",
                    state: &state,
                    // The real loop ANDs the route's preference with the request's
                    // Connection header (serve.rs). The client here reads until EOF,
                    // so a keep-alive reply would wait out its whole timeout.
                    keep_alive: if head.connection_close {
                        clarus_core::http::router::KeepAlive::Off
                    } else {
                        route.keep_alive
                    },
                    conn: &mut conn,
                    wado_series_expected: None,
                    stow_accepted: 0,
                    proxy_trusted: false,
                    route,
                };
                let _ = dispatch(&route, &mut ctx);
            }
            other => panic!("expected a match for the events route, got {other:?}"),
        }
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client
        .write_all(
            format!(
                "GET /dicomweb/workitems/{WI}/events HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();

    // Read only until the head ends; the body is an open stream.
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        match client.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => raw.push(byte[0]),
            Err(_) => break,
        }
        assert!(raw.len() < 8192, "no head terminator within 8 KiB");
    }
    drop(client);
    let _ = server.join();

    let resp = parse_response(&raw);
    assert_eq!(
        resp.header("X-Accel-Buffering").as_deref(),
        Some("no"),
        "X-Accel-Buffering must be 'no' in the head: {:?}",
        resp.headers
    );
    assert!(
        resp.header("Content-Type")
            .is_some_and(|v| v.contains("text/event-stream")),
        "SSE content type must be unchanged: {:?}",
        resp.header("Content-Type")
    );
}

#[test]
fn viewer_200_html() {
    let tc = setup();
    let resp = serve(&tc.state, "GET", "/", &[]);
    assert!(resp.status.starts_with("HTTP/1.1 200"), "{}", resp.status);
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
}

#[test]
fn wado_metadata_404_on_missing() {
    let tc = setup();
    let resp = serve(
        &tc.state,
        "GET",
        "/dicomweb/studies/1.2.3.4.5/series/6.7.8/instances/9.10.11/metadata",
        &[],
    );
    assert_eq!(resp.status, "HTTP/1.1 404 Not Found");
}

#[test]
fn unknown_route_404() {
    let tc = setup();
    let resp = serve(&tc.state, "GET", "/no/such/route", &[]);
    assert_eq!(resp.status, "HTTP/1.1 404 Not Found");
}

#[test]
fn method_not_allowed_405_allow() {
    let _tc = setup();
    // An unknown method on any path -> router 405 + Allow (method-first).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let worker = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut conn = Conn::new(stream, b"127.0.0.1".to_vec());
        let mut head_buf = [0u8; HEADER_BUF_SIZE];
        let n = conn
            .read_head(&mut head_buf, Duration::from_secs(10))
            .unwrap();
        let head_buf = &head_buf[..n];
        let (mut head, off) = parse_request_line(head_buf).unwrap();
        let _ = parse_headers(&mut head, &head_buf[off + 2..]).unwrap();
        match match_route(head.method, head.path) {
            RouterOutcome::MethodNotAllowed { allow } => {
                conn.header
                    .extend_from_slice(format!("HTTP/1.1 405 Method Not Allowed\r\nAllow: {allow}\r\nContent-Length: 0\r\n\r\n").as_bytes());
                let _ = conn.write_head();
            }
            other => panic!("expected MNA, got {other:?}"),
        }
        // Close politely so the reply is not lost. Returning the stream kept
        // it alive inside the unjoined JoinHandle, so the reply ended at the
        // client's read timeout; dropping it outright aborts a peer that is
        // still sending, and the abort discards the response. Shutting down
        // the write half signals EOF, then draining absorbs whatever the
        // client still had in flight before the socket goes away.
        close_politely(conn);
    });
    let mut c = TcpStream::connect(addr).unwrap();
    c.write_all(b"PATCH /health HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n")
        .unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = c.read(&mut buf) {
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
    let _ = worker.join();
    let resp = parse_response(&all);
    assert_eq!(resp.status, "HTTP/1.1 405 Method Not Allowed");
    assert!(
        resp.header("allow")
            .map(|a| a.contains("GET"))
            .unwrap_or(false)
    );
}

#[test]
fn ups_create_body_json() {
    let tc = setup();
    let body = br#"{"0020000D":{"vr":"UI","Value":["1.2.3"]}}"#;
    let resp = serve(&tc.state, "POST", "/dicomweb/workitems/1.2.3/items", body);
    // Empty workitem store: create succeeds (201) or 400; scope is body read.
    assert!(resp.status.starts_with("HTTP/1.1"), "{}", resp.status);
}

/// The Deletion Lock has to survive the whole wire path.
///
/// The pure handler takes the query string as an argument, so a unit test
/// cannot show that the router actually passes the request's query through to
/// it. This goes through `match_route`, `RequestContext` and `dispatch` with a
/// real request line carrying `?deletionlock=true` - PS3.18 11.10's own
/// template `POST .../{subscriber}{?deletionlock}{&filter}` - and then asserts
/// the registration the retention sweep will read.
#[test]
fn subscribe_with_deletionlock_on_the_wire_locks_the_subscription() {
    let tc = setup();
    const SUB: &str = "WIRELOCK";
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(SUB);

    let body = br#"{"endpoint":"http://127.0.0.1:8124/ups"}"#;
    let resp = serve(
        &tc.state,
        "POST",
        &format!(
            "/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/{SUB}?deletionlock=true"
        ),
        body,
    );
    assert_eq!(
        resp.status,
        "HTTP/1.1 201 Created",
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    assert!(
        String::from_utf8_lossy(&resp.body).contains("\"deletionlock\":true"),
        "the 201 states the lock the router parsed: {}",
        String::from_utf8_lossy(&resp.body)
    );

    // The lock the sweep reads must actually be set, and it must belong to
    // THIS subscriber.
    //
    // The assertion is scoped rather than global on purpose. The registry is
    // process-wide and `dispatch_smoke` runs its tests in parallel, so a
    // global "some lock exists" is satisfied by another test's lock and a
    // global "no lock exists" is broken by it. The first draft of these two
    // tests used the global form and failed intermittently in exactly this
    // way (a state change in a neighbouring test supplied the lock); the same
    // defect class was fixed in the unit tests by D0.
    let lock_of = |subscriber: &str| {
        clarus_server::push::subscriptions()
            .snapshot()
            .into_iter()
            .find(|s| s.subscriber == subscriber)
            .and_then(|s| s.deletion_lock_at)
    };

    assert!(
        lock_of(SUB).is_some(),
        "the wire path registered a subscription carrying a Deletion Lock"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(SUB);
    assert_eq!(
        lock_of(SUB),
        None,
        "unsubscribing releases it (PS3.4 CC.2.3.1 Table CC.2.3-2)"
    );
}

/// The parameter is optional, so its absence must not be read as an error and
/// must not silently lock either.
#[test]
fn subscribe_without_deletionlock_on_the_wire_registers_unlocked() {
    let tc = setup();
    const SUB: &str = "WIREPLAIN";
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(SUB);

    let body = br#"{"endpoint":"http://127.0.0.1:8124/ups"}"#;
    let resp = serve(
        &tc.state,
        "POST",
        &format!("/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/{SUB}"),
        body,
    );
    assert_eq!(
        resp.status,
        "HTTP/1.1 201 Created",
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    assert!(
        String::from_utf8_lossy(&resp.body).contains("\"deletionlock\":false"),
        "the 201 reports no lock: {}",
        String::from_utf8_lossy(&resp.body)
    );
    // Scoped to THIS subscriber: the global form is broken by any parallel
    // test that holds a lock (see the note in the test above).
    assert!(
        clarus_server::push::subscriptions()
            .snapshot()
            .into_iter()
            .find(|s| s.subscriber == SUB)
            .is_some_and(|s| s.deletion_lock_at.is_none()),
        "this subscription registered WITHOUT a lock"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(SUB);
}

/// A value that is neither true nor false is refused on the wire too, and the
/// refusal must not leave a half-registered subscription behind.
#[test]
fn subscribe_with_a_bad_deletionlock_is_refused_400_on_the_wire() {
    let tc = setup();
    const SUB: &str = "WIREBAD";
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(SUB);

    let body = br#"{"endpoint":"http://127.0.0.1:8124/ups"}"#;
    let resp = serve(
        &tc.state,
        "POST",
        &format!(
            "/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/{SUB}?deletionlock=maybe"
        ),
        body,
    );
    assert_eq!(
        resp.status,
        "HTTP/1.1 400 Bad Request",
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    assert!(
        String::from_utf8_lossy(&resp.body).contains("deletionlock"),
        "the message names the parameter: {}",
        String::from_utf8_lossy(&resp.body)
    );
    // Unsubscribe answers 204 whether or not anything was removed, so the
    // absence of a registration has to be read off the registry itself.
    assert!(
        clarus_server::push::subscriptions()
            .snapshot()
            .iter()
            .all(|s| s.subscriber != SUB),
        "the refused request registered nothing"
    );
}

#[test]
fn body_over_limit_413() {
    let tc = setup();
    // Send a body larger than the 1 MiB cap via Content-Length.
    //
    // The path is the standard Create resource (PS3.18 11.4.1.1: the Workitem
    // UID is a query parameter). It used to be `/workitems/1.2.3/items`,
    // which no longer matches a route at all - an unrouted path would answer
    // 404 and this test would be asserting the cap through a route that does
    // not exist.
    let big = vec![b'x'; 2 * 1024 * 1024];
    let resp = serve(
        &tc.state,
        "POST",
        "/dicomweb/workitems?workitem=1.2.3",
        &big,
    );
    assert_eq!(resp.status, "HTTP/1.1 413 Payload Too Large");
}

// -- 14.10: rendered narrowing, STOW media type ---------------------------

/// PS3.18 8.7.9: `/rendered` names the Rendered Resource at the INSTANCE
/// level. The study and series levels used to resolve to the thumbnail
/// handler, so `/studies/{s}/rendered` returned the thumbnail bytes under a
/// name 8.7.4-1 gives no multi-frame image/jpeg for - a MANDATORY deviation
/// (10.4.3.3.3). Those names must now be unrouted (404), never a thumbnail.
#[test]
fn study_and_series_rendered_are_not_routed() {
    let tc = setup();
    for path in [
        "/dicomweb/studies/1.2.3/rendered",
        "/dicomweb/studies/1.2.3/series/4.5/rendered",
        "/dicomweb/studies/1.2.3/series/4.5/instances/6.7/frames/1/rendered",
    ] {
        let resp = serve(&tc.state, "GET", path, &[]);
        assert_eq!(
            resp.status, "HTTP/1.1 404 Not Found",
            "{path} must not resolve to the thumbnail handler, got {}",
            resp.status
        );
    }
    // The instance-level Rendered Resource stays routed (it reaches the
    // handler; with no stored instance that is a 404 of its own, not 405/400).
    let resp = serve(
        &tc.state,
        "GET",
        "/dicomweb/studies/1.2.3/series/4.5/instances/6.7/rendered",
        &[],
    );
    assert_ne!(
        resp.status, "HTTP/1.1 405 Method Not Allowed",
        "instance-level /rendered must remain a routed GET"
    );
}

/// PS3.18 10.5.3-1: a Store Transaction body whose media type is neither
/// `application/dicom` nor `multipart/related` is an UNSUPPORTED MEDIA TYPE
/// (415), not a malformed request (400). The body is irrelevant - the type
/// alone decides.
#[test]
fn stow_unsupported_media_type_is_415() {
    let tc = setup();
    for ct in ["text/plain", "application/json", "application/octet-stream"] {
        let resp = serve_with_ct(&tc.state, "POST", "/dicomweb/studies", ct, b"not dicom");
        assert_eq!(
            resp.status, "HTTP/1.1 415 Unsupported Media Type",
            "Content-Type: {ct} must be 415, got {}",
            resp.status
        );
    }
}
