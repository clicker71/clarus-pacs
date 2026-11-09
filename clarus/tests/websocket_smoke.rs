// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! WebSocket Notification Connection smoke (PS3.18 8.10.4, RFC 6455).
//!
//! Raw `TcpStream`, no dependencies, matching the style of `ups_push_smoke`:
//! the point is to speak the protocol byte-for-byte, so a helper library would
//! hide the very thing under test.
//!
//! The tests cover the handshake, the event transport (RAD-87), the delivery
//! filter, the lifecycle, and coexistence with the two older transports.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clarus_core::http::conn::{Conn, HEADER_BUF_SIZE};
use clarus_core::http::parse::{parse_headers, parse_request_line};
use clarus_core::http::router::{RouterOutcome, match_route};
use clarus_server::dispatch::{RequestContext, dispatch};
use clarus_server::handlers::ups::event_bus::FilterRouting;
use clarus_server::state::AppState;

// -- Setup (mirrors dispatch_smoke) --------------------------------------

struct TestCtx {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
}

fn setup() -> TestCtx {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path();
    let idx_dir = root.join("idx");
    let manifest_dir = root.join("manifests");
    let btree_root = root.join("btree");
    std::fs::create_dir_all(&idx_dir).unwrap();
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&btree_root).unwrap();

    let mut config = clarus_server::config::Config::test_default();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.btree.audit_dir = root.join("audit").to_string_lossy().to_string();
    config.btree.access_dir = root.join("access").to_string_lossy().to_string();
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;

    let cas =
        clarus_adapters::storage::ContentAddressedStorage::new(root.join("cas")).expect("open cas");

    let state = AppState {
        cas,
        codec_registry: clarus_adapters::codec::CodecRegistry::default(),
        ups_events: clarus_server::sse_fanout::DualEventSink::new(),
        start_time: Instant::now(),
        metrics_ring: Arc::new(std::sync::RwLock::new(std::collections::VecDeque::new())),
        log_buffer: Arc::new(std::sync::RwLock::new(std::collections::VecDeque::new())),
        maintenance: Arc::new(std::sync::RwLock::new(
            clarus_server::state::MaintenanceState::default(),
        )),
        #[cfg(feature = "s3")]
        s3: Arc::new(std::sync::OnceLock::new()),
        cold_cas: None,
        shadow: clarus_server::shadow::ShadowTier::new(idx_dir.clone()),
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
        config: config.clone(),
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(2),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    };

    TestCtx {
        _temp: temp,
        state: Arc::new(state),
    }
}

/// Run one raw request through the production dispatch path and return the
/// open socket plus the response head, so a test can continue speaking the
/// protocol on the same connection (which is the whole point for WebSocket).
fn serve_upgrade(state: &Arc<AppState>, path: &str, extra_headers: &str) -> (TcpStream, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let state = Arc::clone(state);
    let extra = extra_headers.to_string();
    let path_owned = path.to_string();
    let path_for_panic = path_owned.clone();

    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
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
                    keep_alive: route.keep_alive,
                    conn: &mut conn,
                    wado_series_expected: None,
                    stow_accepted: 0,
                    proxy_trusted: false,
                    route,
                };
                let _ = dispatch(&route, &mut ctx);
            }
            other => panic!("expected a match for {path_for_panic}, got {other:?}"),
        }
    });

    let mut socket = TcpStream::connect(addr).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let request =
        format!("GET {path_owned} HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\n{extra}\r\n");
    socket.write_all(request.as_bytes()).unwrap();
    socket.flush().unwrap();
    (socket, request)
}

/// A valid client key + version for the handshake.
const VALID_EXTRA: &str = "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n";

/// Read the HTTP head (up to the blank line) off `socket`.
fn read_head(socket: &mut TcpStream) -> String {
    let mut reader = BufReader::new(socket.try_clone().expect("clone"));
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let done = line == "\r\n" || line == "\n";
        head.push_str(&line);
        if done {
            break;
        }
    }
    head
}

/// Read exactly one server frame (server frames are unmasked and unfragmented).
fn read_server_frame(socket: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 2];
    socket.read_exact(&mut hdr).expect("frame header");
    let opcode = hdr[0] & 0x0F;
    assert_eq!(hdr[0] & 0x80, 0x80, "server frames set FIN");
    assert_eq!(
        hdr[1] & 0x80,
        0,
        "server frames are NOT masked (RFC 6455 5.1)"
    );

    let len = match hdr[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            socket.read_exact(&mut b).expect("len16");
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            socket.read_exact(&mut b).expect("len64");
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    let mut payload = vec![0u8; len];
    socket.read_exact(&mut payload).expect("payload");
    (opcode, payload)
}

/// The matcher input for a test that only cares about the step label.
///
/// Every other filter key is absent, which constrains nothing for a
/// subscription that does not carry it (PS3.18 8.3.4.1: every PRESENT key must
/// hold), so this answers what the old `accepts_for(sub, uid, label)` did.
fn label_values(text: &str) -> clarus_server::push::FilterValues<'_> {
    clarus_server::push::FilterValues {
        step_label: text,
        ..Default::default()
    }
}

/// Complete a handshake and return the open socket.
fn open_channel(state: &Arc<AppState>, requester: &str) -> TcpStream {
    let (mut socket, _) = serve_upgrade(
        state,
        &format!("/dicomweb/subscribers/{requester}"),
        VALID_EXTRA,
    );
    let head = read_head(&mut socket);
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "handshake must succeed: {head}"
    );
    socket
}

/// Serialise the same AE Title for both the subscription and the channel.
fn subscribe_for(requester: &str) {
    let body = "{\"endpoint\":\"http://127.0.0.1:9/ups\"}";
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(requester);
    let r =
        clarus_server::handlers::subscribe_pure::subscribe(requester, body.as_bytes(), false, "");
    assert_eq!(r.status, 201, "subscription must register");
}

/// Serialises the process-wide registries between tests in this binary.
fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

// -- Handshake (D2) ------------------------------------------------------

#[test]
fn a_valid_handshake_returns_101_with_the_rfc_accept_value() {
    let tc = setup();
    let (mut socket, _) =
        serve_upgrade(&tc.state, "/dicomweb/subscribers/WS_HANDSHAKE", VALID_EXTRA);
    let head = read_head(&mut socket);

    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols"),
        "{head}"
    );
    assert!(
        head.contains("Upgrade: websocket") || head.contains("Upgrade: WebSocket"),
        "{head}"
    );
    assert!(head.contains("Connection: Upgrade"), "{head}");
    // RFC 6455 1.3 vector: this key must produce exactly this accept value.
    assert!(
        head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "accept must be the RFC 6455 1.3 value: {head}"
    );
    // Our channel defines no subprotocol, so we must not claim one.
    assert!(
        !head.to_ascii_lowercase().contains("sec-websocket-protocol"),
        "no subprotocol is negotiated: {head}"
    );
}

#[test]
fn an_unsupported_version_draws_426_with_the_supported_version() {
    let tc = setup();
    let (mut socket, _) = serve_upgrade(
        &tc.state,
        "/dicomweb/subscribers/WS_VER",
        "Upgrade: websocket\r\nSec-WebSocket-Version: 8\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
    );
    let head = read_head(&mut socket);
    assert!(head.starts_with("HTTP/1.1 426"), "{head}");
    assert!(
        head.contains("Sec-WebSocket-Version: 13"),
        "426 must advertise what we do support: {head}"
    );
}

#[test]
fn a_bad_key_is_refused_400() {
    let tc = setup();
    let (mut socket, _) = serve_upgrade(
        &tc.state,
        "/dicomweb/subscribers/WS_BADKEY",
        "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: not-base64!!\r\n",
    );
    let head = read_head(&mut socket);
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
}

#[test]
fn a_plain_get_on_the_notification_path_is_not_an_upgrade() {
    // No Upgrade/Connection headers at all: the route-matrix behaviour on this
    // path, i.e. an ordinary JSON answer rather than a protocol error.
    let tc = setup();
    let (mut socket, _) = serve_upgrade(&tc.state, "/dicomweb/subscribers/WS_PLAIN", "");
    let head = read_head(&mut socket);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        !head.starts_with("HTTP/1.1 101"),
        "must not upgrade: {head}"
    );
}

#[test]
fn upgrade_is_matched_as_a_token_not_a_substring() {
    let tc = setup();
    // `websocketx` is not the websocket token, so this is NOT an upgrade.
    let (mut socket, _) = serve_upgrade(
        &tc.state,
        "/dicomweb/subscribers/WS_TOKEN",
        "Upgrade: websocketx\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
    );
    let head = read_head(&mut socket);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "a near-miss token must not upgrade: {head}"
    );
}

// -- Delivery (D3, RAD-87) ----------------------------------------------

#[test]
fn a_state_change_reaches_an_open_channel_as_the_sse_payload() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_DELIVER";
    subscribe_for(REQ);
    let mut socket = open_channel(&tc.state, REQ);

    // Publish through the production sink, exactly as a state change does.
    use clarus_server::handlers::ups::event_bus::UpsEvent;
    use clarus_server::handlers::ups_pure::EventSink;
    tc.state.ups_events.publish(UpsEvent {
        workitem_uid: "1.2.3".into(),
        previous_state: "SCHEDULED".into(),
        new_state: "IN PROGRESS".into(),
        timestamp: "1700000000".into(),
        event_id: 7,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".into(),
        filter: Box::default(),
    });

    let (opcode, payload) = read_server_frame(&mut socket);
    assert_eq!(
        opcode,
        clarus_server::handlers::websocket::opcode::TEXT,
        "an Event Report is a text frame"
    );
    let text = String::from_utf8(payload).expect("utf-8");
    // The Event Report is a DICOM JSON Data Set (PS3.18 8.10.5.1.1) and is
    // byte-identical to what the SSE `data:` line carries: one producer, two
    // transports. The three attributes are the affected UPS identity
    // (0000,1000, the command attribute the Event Report carries), the
    // CURRENT Input Readiness State (0040,4041, required by PS3.4 CC.2.4.3)
    // and the Procedure Step State (0074,1000).
    assert_eq!(
        text,
        "{\"00001000\":{\"vr\":\"UI\",\"Value\":[\"1.2.3\"]},\
         \"00404041\":{\"vr\":\"CS\",\"Value\":[\"READY\"]},\
         \"00741000\":{\"vr\":\"CS\",\"Value\":[\"IN PROGRESS\"]}}"
            .replace("\\\n         ", "")
    );
    // The retired envelope must be GONE, not merely joined: a payload carrying
    // both would still be non-conformant.
    for key in [
        "\"workitem_uid\"",
        "\"previous_state\"",
        "\"new_state\"",
        "\"timestamp\"",
        "\"event_id\"",
    ] {
        assert!(
            !text.contains(key),
            "the Event Report must not carry the envelope key {key}: {text}"
        );
    }

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    clarus_server::ws_channels::channels().clear();
}

/// The full Workitem Subscription flow (PS3.18 11.10.1.1), end to end.
///
/// Register through the WORKITEM-scoped resource, open the Notification
/// Connection, publish an event for that workitem, and receive it. Then the
/// same for a different workitem, which must NOT arrive: that is what makes
/// the subscription a scope rather than a second unfiltered subscription.
#[test]
fn a_workitem_subscription_receives_that_workitem_and_no_other() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_WORKITEM";
    const WI: &str = "1.2.3.4.5";

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    let r =
        clarus_server::handlers::subscribe_pure::subscribe_scoped(REQ, b"{}", false, Some(WI), "");
    assert_eq!(r.status, 201, "workitem subscription must register");

    let mut socket = open_channel(&tc.state, REQ);

    use clarus_server::handlers::ups::event_bus::UpsEvent;
    use clarus_server::handlers::ups_pure::EventSink;

    // An event for ANOTHER workitem: nothing may arrive. Published first so a
    // subsequent read proves ordering rather than racing the assertion.
    tc.state.ups_events.publish(UpsEvent {
        workitem_uid: "9.9.9.9".into(),
        previous_state: "SCHEDULED".into(),
        new_state: "IN PROGRESS".into(),
        timestamp: "1700000000".into(),
        event_id: 1,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".into(),
        filter: Box::default(),
    });

    // The event for the subscribed workitem.
    tc.state.ups_events.publish(UpsEvent {
        workitem_uid: WI.into(),
        previous_state: "SCHEDULED".into(),
        new_state: "IN PROGRESS".into(),
        timestamp: "1700000001".into(),
        event_id: 2,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".into(),
        filter: Box::default(),
    });

    // The first (and only) frame on the wire must be the subscribed workitem's
    // event - if the 9.9.9.9 event had been delivered it would arrive first.
    let (opcode, payload) = read_server_frame(&mut socket);
    assert_eq!(opcode, clarus_server::handlers::websocket::opcode::TEXT);
    let text = String::from_utf8(payload).expect("utf-8");
    assert!(
        text.contains("\"1.2.3.4.5\""),
        "the subscribed workitem's event must arrive: {text}"
    );
    assert!(
        !text.contains("9.9.9.9"),
        "an event for another workitem must never reach a scoped subscriber: {text}"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    clarus_server::ws_channels::channels().clear();
}

/// DELETE removes the subscription, so a later event reaches nothing.
#[test]
fn deleting_a_workitem_subscription_stops_delivery() {
    let _guard = registry_lock();
    const REQ: &str = "WS_WORKITEM_DEL";
    const WI: &str = "1.2.3.4.5";

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    let r =
        clarus_server::handlers::subscribe_pure::subscribe_scoped(REQ, b"{}", false, Some(WI), "");
    assert_eq!(r.status, 201);
    assert!(clarus_server::push::subscriptions().accepts_for(REQ, WI, &label_values("")));

    // DELETE is keyed by AE Title alone (PS3.4 CC.2.3.1), so the workitem UID
    // in the path narrows the URL but not the removal.
    let del = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    assert_eq!(del.status, 204);
    assert!(
        !clarus_server::push::subscriptions().accepts_for(REQ, WI, &label_values("")),
        "after DELETE the subscriber must match nothing"
    );
}

#[test]
fn a_channel_without_a_subscription_receives_nothing() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_NOSUB";
    // Deliberately NOT subscribed: only the connection is open.
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    let mut socket = open_channel(&tc.state, REQ);

    use clarus_server::handlers::ups::event_bus::UpsEvent;
    use clarus_server::handlers::ups_pure::EventSink;
    tc.state.ups_events.publish(UpsEvent {
        workitem_uid: "1.2.3".into(),
        previous_state: "SCHEDULED".into(),
        new_state: "IN PROGRESS".into(),
        timestamp: "1700000000".into(),
        event_id: 8,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".into(),
        filter: Box::default(),
    });

    socket
        .set_read_timeout(Some(Duration::from_millis(600)))
        .unwrap();
    let mut buf = [0u8; 64];
    match socket.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("an unsubscribed channel must receive nothing, got {n} bytes"),
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Err(e) => panic!("unexpected read error: {e}"),
    }

    clarus_server::ws_channels::channels().clear();
}

#[test]
fn two_channels_of_one_requester_both_receive() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_TWO";
    subscribe_for(REQ);
    let mut first = open_channel(&tc.state, REQ);
    let mut second = open_channel(&tc.state, REQ);
    assert_eq!(clarus_server::ws_channels::channels().count_for(REQ), 2);

    use clarus_server::handlers::ups::event_bus::UpsEvent;
    use clarus_server::handlers::ups_pure::EventSink;
    tc.state.ups_events.publish(UpsEvent {
        workitem_uid: "1.2.3".into(),
        previous_state: "SCHEDULED".into(),
        new_state: "IN PROGRESS".into(),
        timestamp: "1700000000".into(),
        event_id: 9,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".into(),
        filter: Box::default(),
    });

    for socket in [&mut first, &mut second] {
        let (opcode, payload) = read_server_frame(socket);
        assert_eq!(opcode, clarus_server::handlers::websocket::opcode::TEXT);
        let text = String::from_utf8(payload).unwrap();
        assert!(
            text.contains("\"00001000\":{\"vr\":\"UI\",\"Value\":[\"1.2.3\"]}"),
            "both connections get the same event: {text}"
        );
    }

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    clarus_server::ws_channels::channels().clear();
}

/// The live-e2e defect, end to end over a real channel: Weasis subscribes with
/// `StudyInstanceUID=<uid>` and this server used to answer 400, so the client
/// fell back to polling.
///
/// PS3.4 CC.2.5-3 types Study Instance UID a matching key (O). This proves the
/// key is accepted AND that it really narrows the channel: the event for the
/// subscribed study arrives, the event for another study does not.
#[test]
fn a_channel_filtered_by_study_uid_receives_only_that_study() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_STUDY";
    const STUDY: &str = "1.2.840.10008.5.1.4.1.1.2.1.1";

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    let r = clarus_server::handlers::subscribe_pure::subscribe(
        REQ,
        b"{}",
        true,
        &format!("filter=StudyInstanceUID={STUDY}"),
    );
    assert_eq!(
        r.status,
        201,
        "StudyInstanceUID must be accepted (it was a 400 before): {}",
        String::from_utf8_lossy(&r.body)
    );

    let mut socket = open_channel(&tc.state, REQ);

    use clarus_server::handlers::ups_pure::EventSink;
    // An event for a DIFFERENT study first: it must not arrive.
    tc.state.ups_events.notify_created(
        "1.2.840.10008.5.1.4.34.6.1.20260301.1",
        FilterRouting {
            step_label: "CAD".to_string(),
            input_readiness: Some("READY".to_string()),
            study_uid: Some("9.9.9.9".to_string()),
            ..Default::default()
        },
    );
    // Then one for the subscribed study.
    tc.state.ups_events.notify_created(
        "1.2.840.10008.5.1.4.34.6.1.20260301.2",
        FilterRouting {
            step_label: "CAD".to_string(),
            input_readiness: Some("READY".to_string()),
            study_uid: Some(STUDY.to_string()),
            ..Default::default()
        },
    );

    // The FIRST frame the channel sees must be the subscribed study's event:
    // if the other study had leaked through it would arrive first.
    let (opcode, payload) = read_server_frame(&mut socket);
    assert_eq!(opcode, clarus_server::handlers::websocket::opcode::TEXT);
    let text = String::from_utf8(payload).unwrap();
    assert!(
        text.contains("1.2.840.10008.5.1.4.34.6.1.20260301.2"),
        "only the subscribed study's event may arrive: {text}"
    );
    assert!(
        !text.contains("20260301.1"),
        "an event for another study must not be delivered: {text}"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    clarus_server::ws_channels::channels().clear();
}

/// Register a subscription narrowed by Coding Scheme Designator, with NO
/// endpoint: the channel-only shape a performer that opens a Notification
/// Connection uses (`clinfer`).
///
/// `subscribe_pure::subscribe` takes the filter as the `filter` QUERY
/// parameter, whose key form is `<tag>=<value>` (PS3.18 8.3.4.1).
fn subscribe_for_code_scheme(requester: &str, scheme: &str) {
    let body = "{}";
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(requester);
    let query = format!("filter=00404018.00080102={scheme}");
    let r = clarus_server::handlers::subscribe_pure::subscribe(
        requester,
        body.as_bytes(),
        true,
        &query,
    );
    assert_eq!(r.status, 201, "code-filtered subscription must register");
}

#[test]
fn a_code_filtered_subscription_receives_the_event_over_its_channel() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_CODEFILTER";
    subscribe_for_code_scheme(REQ, "99CLARUS");
    let mut socket = open_channel(&tc.state, REQ);

    use clarus_server::handlers::ups_pure::EventSink;
    // The production path: `notify_created` fills `step_label` and
    // `scheduled_code` into the event, and the channel filter matches on both,
    // exactly as the POST push does.
    tc.state.ups_events.notify_created(
        "1.2.840.10008.5.1.4.34.6.1.20260201.1",
        FilterRouting {
            step_label: "CAD".to_string(),
            scheduled_code: Some(("CT-ORGAN-SEG".to_string(), "99CLARUS".to_string())),
            input_readiness: Some("READY".to_string()),
            ..Default::default()
        },
    );

    let (opcode, payload) = read_server_frame(&mut socket);
    assert_eq!(
        opcode,
        clarus_server::handlers::websocket::opcode::TEXT,
        "an Event Report is a text frame"
    );
    let text = String::from_utf8(payload).expect("utf-8");
    assert!(
        text.contains("\"00741000\":{\"vr\":\"CS\",\"Value\":[\"SCHEDULED\"]}"),
        "a code-filtered channel subscriber must receive the creation event: {text}"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    clarus_server::ws_channels::channels().clear();
}

#[test]
fn a_channel_filtered_on_another_scheme_receives_nothing() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_CODEFILTER_OTHER";
    subscribe_for_code_scheme(REQ, "OTHER");
    let mut socket = open_channel(&tc.state, REQ);

    use clarus_server::handlers::ups_pure::EventSink;
    tc.state.ups_events.notify_created(
        "1.2.840.10008.5.1.4.34.6.1.20260201.2",
        FilterRouting {
            step_label: "CAD".to_string(),
            scheduled_code: Some(("CT-ORGAN-SEG".to_string(), "99CLARUS".to_string())),
            input_readiness: Some("READY".to_string()),
            ..Default::default()
        },
    );

    // The filter is a real filter: a subscription naming a different Coding
    // Scheme Designator must NOT be served this event.
    socket
        .set_read_timeout(Some(Duration::from_millis(600)))
        .unwrap();
    let mut buf = [0u8; 64];
    match socket.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("a foreign-scheme channel must receive nothing, got {n} bytes"),
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Err(e) => panic!("unexpected read error: {e}"),
    }

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    clarus_server::ws_channels::channels().clear();
}

// -- Lifecycle (D4) ------------------------------------------------------

#[test]
fn a_close_frame_removes_the_channel_from_the_registry() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_CLOSE";
    subscribe_for(REQ);
    let mut socket = open_channel(&tc.state, REQ);
    let live_before = clarus_server::ws_channels::channels().count_for(REQ);
    assert_eq!(live_before, 1);

    // Client sends a masked close frame (1000 = normal closure).
    let frame = clarus_server::handlers::websocket::encode_client_frame_masked(
        clarus_server::handlers::websocket::opcode::CLOSE,
        &1000u16.to_be_bytes(),
        [1, 2, 3, 4],
    );
    socket.write_all(&frame).expect("send close");
    socket.flush().unwrap();

    // The server echoes a close frame...
    let (opcode, _payload) = read_server_frame(&mut socket);
    assert_eq!(opcode, clarus_server::handlers::websocket::opcode::CLOSE);

    // ...and the registry entry is released. The connection thread finishes
    // asynchronously, so poll briefly rather than assume instant.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if clarus_server::ws_channels::channels().count_for(REQ) == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the channel must be removed from the registry after close"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
}

#[test]
fn a_tcp_eof_removes_the_channel() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_EOF";
    subscribe_for(REQ);
    let socket = open_channel(&tc.state, REQ);
    assert_eq!(clarus_server::ws_channels::channels().count_for(REQ), 1);

    drop(socket); // abrupt disconnect, no close frame

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if clarus_server::ws_channels::channels().count_for(REQ) == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TCP EOF must release the registry entry"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
}

#[test]
fn reopening_after_a_close_works() {
    let _guard = registry_lock();
    let tc = setup();
    const REQ: &str = "WS_REOPEN";
    subscribe_for(REQ);
    {
        let socket = open_channel(&tc.state, REQ);
        drop(socket);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while clarus_server::ws_channels::channels().count_for(REQ) != 0 {
        assert!(Instant::now() < deadline, "first channel must close");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Reopening must be allowed at any time (the statement puts reconnection
    // on the subscriber).
    let mut socket = open_channel(&tc.state, REQ);
    assert_eq!(clarus_server::ws_channels::channels().count_for(REQ), 1);

    use clarus_server::handlers::ups::event_bus::UpsEvent;
    use clarus_server::handlers::ups_pure::EventSink;
    tc.state.ups_events.publish(UpsEvent {
        workitem_uid: "1.2.3".into(),
        previous_state: "A".into(),
        new_state: "B".into(),
        timestamp: "0".into(),
        event_id: 11,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".into(),
        filter: Box::default(),
    });
    let (opcode, payload) = read_server_frame(&mut socket);
    assert_eq!(opcode, clarus_server::handlers::websocket::opcode::TEXT);
    // Reopening proves the frame arrives on the NEW connection; the payload
    // no longer carries `event_id` (that is transport-level, SSE `id:` only),
    // so the assertion is on the Event Report's own identity.
    let reopened = String::from_utf8(payload).unwrap();
    assert!(
        reopened.contains("\"00001000\""),
        "the reopened channel must receive an Event Report: {reopened}"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    clarus_server::ws_channels::channels().clear();
}

// -- Coexistence (D5) ----------------------------------------------------

/// The three notification transports must not interfere. This asserts the
/// older two still work with a WebSocket channel open alongside them, because
/// the whole point of adding a third channel is that it does not replace the
/// other two.
#[test]
fn the_sse_stream_and_the_websocket_channel_both_receive_an_event() {
    let _guard = registry_lock();
    let tc = setup();
    const WS_REQ: &str = "WS_COEXIST";
    subscribe_for(WS_REQ);
    let mut ws_socket = open_channel(&tc.state, WS_REQ);

    // The SSE side subscribes through the same fan-out the HTTP handler uses.
    let (sse_rx, _count) = tc
        .state
        .ups_events
        .fanout_try_subscribe("1.2.3.COEXIST")
        .expect("sse subscribe");

    use clarus_server::handlers::ups::event_bus::UpsEvent;
    use clarus_server::handlers::ups_pure::EventSink;
    tc.state.ups_events.publish(UpsEvent {
        workitem_uid: "1.2.3.COEXIST".into(),
        previous_state: "SCHEDULED".into(),
        new_state: "IN PROGRESS".into(),
        timestamp: "1700000000".into(),
        event_id: 21,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".into(),
        filter: Box::default(),
    });

    // WebSocket side: a text frame arrives.
    let (opcode, ws_payload) = read_server_frame(&mut ws_socket);
    assert_eq!(opcode, clarus_server::handlers::websocket::opcode::TEXT);

    // SSE side: the same event arrives on the fan-out receiver.
    //
    // The id is STAMPED per UID by the fan-out (W30), so the first event a
    // subscriber of this workitem receives carries 0 regardless of the
    // placeholder the caller put in the event. The point of the assertion is
    // that an id exists and is not a fixed zero forever - see the fan-out's
    // own monotonic tests for the sequence.
    let event = sse_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("sse receives");
    assert_eq!(event.event_id, 0, "the fan-out stamps its own per-UID id");

    // And the payloads agree: one writer, two transports. The frame is the
    // DICOM JSON Data Set; `event_id` is not in it (it is transport-level, the
    // SSE `id:` line, asserted above via `event.event_id`).
    let ws_text = String::from_utf8(ws_payload).unwrap();
    assert!(
        ws_text.contains("\"00741000\":{\"vr\":\"CS\",\"Value\":[\"IN PROGRESS\"]}"),
        "the same Event Report reached both transports: {ws_text}"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(WS_REQ);
    clarus_server::ws_channels::channels().clear();
}

/// The POST push path is untouched by the WebSocket feature: its subscription
/// matching still works, and a WebSocket channel does not change it.
#[test]
fn the_post_push_subscription_matching_is_unaffected() {
    let _guard = registry_lock();
    const REQ: &str = "WS_PUSHCOEXIST";
    subscribe_for(REQ);
    assert!(
        clarus_server::push::subscriptions().accepts_for(REQ, "1.2.3", &label_values("CAD")),
        "the subscription still matches for the POST push path"
    );
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(REQ);
    assert!(
        !clarus_server::push::subscriptions().accepts_for(REQ, "1.2.3", &label_values("CAD")),
        "and stops matching after an unsubscribe"
    );
}

// -- Route matrix --------------------------------------------------------

/// The new route must not shadow or break the existing subscription routes.
#[test]
fn the_notification_route_is_separate_from_the_subscription_routes() {
    use clarus_core::http::parse::Method;

    // The notification connection resource.
    match match_route(Method::Get, b"/dicomweb/subscribers/CLARUS") {
        RouterOutcome::Match(m) => assert_eq!(m.id, "ups_notification_connect"),
        other => panic!("expected a match, got {other:?}"),
    }
    // The POST push registration route is unchanged (still under `workitems`).
    match match_route(
        Method::Post,
        b"/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/CLARUS",
    ) {
        RouterOutcome::Match(m) => assert_eq!(m.id, "ups_subscribe"),
        other => panic!("expected a match, got {other:?}"),
    }
    // A plain GET on the notification path must still route (to the handler,
    // which decides it is not an upgrade).
    match match_route(Method::Get, b"/dicomweb/subscribers/CLARUS") {
        RouterOutcome::Match(m) => assert_eq!(m.id, "ups_notification_connect"),
        other => panic!("expected a match, got {other:?}"),
    }
}

#[test]
fn a_bad_requester_segment_is_a_bad_uid() {
    use clarus_core::http::parse::Method;
    match match_route(Method::Get, b"/dicomweb/subscribers/") {
        RouterOutcome::BadUid | RouterOutcome::NotFound => {}
        other => panic!("an empty requester must not match: {other:?}"),
    }
}
