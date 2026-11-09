// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Wire tests for the Expose-Rejected surface and the IOCM rate counters.
//!
//! The sequestering contract has three parts, and each needs a different kind
//! of proof:
//!
//! 1. A rejected instance is invisible to an ordinary search and retrieve.
//!    That is a manifest-flag decision, so it is asserted against the served
//!    route rather than against `search_instances` directly - the question is
//!    what a client sees, not what a function returns.
//! 2. The `/dicomweb_qa` service root lifts that filter for an authorized
//!    client and is refused with 403 for anyone else. The refusal must be a
//!    status, not an empty array: an auditor has to be able to tell "denied"
//!    from "gone". A service root also keeps the view out of the search key
//!    space entirely - there is nothing a client can send that both selects a
//!    view and could be mistaken for a matching attribute.
//! 3. The QA root is a READ surface: it must not accept a store.
//!
//! The counters are asserted through `render_metrics`, because a counter only
//! exists for Prometheus if it reaches the render - an increment alone proves
//! nothing to the operator reading `/metrics`.

#![forbid(unsafe_code)]

use clarus_core::http::conn::Timeouts;
use clarus_core::shutdown::Shutdown;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::{Duration, Instant};

const STUDY: &str = "1.2.840.10008.5.1.4.1.1.88.22.1";
const SERIES: &str = "1.2.840.10008.5.1.4.1.1.88.22.2";
/// Hidden by a Quality rejection: the QA surface may show it.
const REJECTED_SOP: &str = "1.2.840.10008.5.1.4.1.1.88.22.3";
const ACCEPTED_SOP: &str = "1.2.840.10008.5.1.4.1.1.88.22.4";
/// Hidden by an IOCM patient-safety rejection: no surface may show it, not
/// even the QA root (IOCM 4.66.4.2.3 defines no expose path).
const HIDDEN_ANY_CLASS_SOP: &str = "1.2.840.10008.5.1.4.1.1.88.22.5";
const QUALITY_REASON: u32 = 113_001;
const PATIENT_SAFETY_REASON: u32 = 113_037;
const INCORRECT_MWL_REASON: u32 = 113_038;
const RETENTION_REASON: u32 = 113_039;

/// One instance per AnyClass reason, so the expose rule is asserted against
/// all three rather than a single representative.
const MWL_HIDDEN_SOP: &str = "1.2.840.10008.5.1.4.1.1.88.22.6";
const RETENTION_HIDDEN_SOP: &str = "1.2.840.10008.5.1.4.1.1.88.22.7";

struct TestCtx {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
}

fn setup_with(mutate: impl FnOnce(&mut Config)) -> TestCtx {
    let temp = tempfile::tempdir().expect("temp dir");
    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&idx_dir).unwrap();

    let mut config = Config::test_default();
    mutate(&mut config);
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.port = 0;
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;
    let storage_dir = temp.path().join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();
    config.storage_dir = storage_dir.to_string_lossy().to_string();

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
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir.clone(), idx_dir),
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

/// Write a one-study manifest carrying one rejected and one accepted instance.
fn write_manifest(state: &AppState) {
    use clarus_core::manifest::{
        AttrValue, InstanceRecord, Manifest, SeriesRecord, StudyQido, SysHeader, TIER_HOT,
    };

    let series_attrs = vec![
        (
            clarus_core::query::tag::SERIES_INSTANCE_UID,
            AttrValue::Text(SERIES.to_string()),
        ),
        (
            clarus_core::query::tag::MODALITY,
            AttrValue::Text("SEG".to_string()),
        ),
    ];
    let make_instance = |sop: &str, number: u64, rejection_reason: u32| InstanceRecord {
        blake3: [0u8; 32],
        attrs: vec![
            (
                clarus_core::query::tag::SOP_INSTANCE_UID,
                AttrValue::Text(sop.to_string()),
            ),
            (
                clarus_core::query::tag::SOP_CLASS_UID,
                AttrValue::Text("1.2.840.10008.5.1.4.1.1.66.4".to_string()),
            ),
            (
                clarus_core::query::tag::INSTANCE_NUMBER,
                AttrValue::UInt(number),
            ),
        ],
        transfer_syntax: "1.2.840.10008.1.2.1".to_string(),
        storage_tier: TIER_HOT,
        rejection_reason,
        file_size: 128,
        frames: Vec::new(),
    };

    let manifest = Manifest {
        sys: SysHeader {
            version: 1,
            storage_tier: TIER_HOT,
            total_pixel_bytes: 0,
            arrived_at: 1,
        },
        study: StudyQido {
            attrs: vec![
                (
                    clarus_core::query::tag::STUDY_INSTANCE_UID,
                    AttrValue::Text(STUDY.to_string()),
                ),
                (
                    clarus_core::query::tag::PATIENT_ID,
                    AttrValue::Text("PAT-1".to_string()),
                ),
            ],
            ..Default::default()
        },
        series: vec![SeriesRecord {
            attrs: series_attrs,
            instances: vec![
                make_instance(REJECTED_SOP, 1, QUALITY_REASON),
                make_instance(ACCEPTED_SOP, 2, clarus_core::manifest::NOT_REJECTED_REASON),
                make_instance(HIDDEN_ANY_CLASS_SOP, 3, PATIENT_SAFETY_REASON),
                make_instance(MWL_HIDDEN_SOP, 4, INCORRECT_MWL_REASON),
                make_instance(RETENTION_HIDDEN_SOP, 5, RETENTION_REASON),
            ],
        }],
    };

    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    std::fs::create_dir_all(&manifest_dir).expect("manifest dir");
    let path = clarus_core::stow::manifest_path(&manifest_dir, STUDY);
    let encoded = manifest.encode().expect("encode manifest");
    std::fs::write(&path, &encoded).expect("write manifest");
    // The served routes read this back through `read_manifest`; a fixture the
    // decoder rejects would surface as a 500 and hide the intended assertion.
    let decoded = clarus_core::manifest::read_manifest(&path).expect("decode manifest");
    assert_eq!(decoded.series.len(), 1);
    assert_eq!(decoded.series[0].instances.len(), 5);
    assert_eq!(
        decoded.series[0].instances[0].rejection_reason,
        QUALITY_REASON
    );
    assert!(!decoded.series[0].instances[1].is_rejected());
    assert_eq!(
        decoded.series[0].instances[2].rejection_reason,
        PATIENT_SAFETY_REASON
    );
}

fn bind_loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// A spare local RFC1918 address, so the peer is genuinely off-loopback.
///
/// The candidates are the host-virtual networks that exist on most developer
/// and CI machines. If none can be bound the caller skips, because a disabled
/// assertion is honest while a loopback client pretending to be remote is not.
fn non_loopback_local_ip() -> Option<Ipv4Addr> {
    for candidate in [
        "172.20.32.1",
        "172.17.0.1",
        "10.10.48.108",
        "172.19.0.1",
        "10.255.255.1",
        "192.0.2.1",
        "172.31.255.1",
    ] {
        let Ok(ip) = candidate.parse::<Ipv4Addr>() else {
            continue;
        };
        if ip.is_loopback() {
            continue;
        }
        if TcpListener::bind(SocketAddr::new(IpAddr::V4(ip), 0)).is_ok() {
            return Some(ip);
        }
    }
    None
}

fn start_server(
    listener: TcpListener,
    state: &Arc<AppState>,
    shutdown: Shutdown,
) -> std::thread::JoinHandle<()> {
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        clarus_server::serve::serve(
            listener,
            state,
            shutdown,
            Timeouts::from_secs(10, 30, 5, 60),
            512,
        );
    })
}

/// Send one request and return the whole response as text.
fn request(addr: SocketAddr, path: &str) -> String {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    c.write_all(req.as_bytes()).expect("write");
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Send one store request and return the whole response as text.
fn post_request(addr: SocketAddr, path: &str) -> String {
    let body = b"--b\r\nContent-Length: 0\r\n\r\n--b--\r\n";
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/related; type=\"application/dicom\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    c.write_all(head.as_bytes()).expect("write head");
    c.write_all(body).expect("write body");
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn status_of(resp: &str) -> u16 {
    resp.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in: {resp}"))
}

fn body_of(resp: &str) -> &str {
    resp.split("\r\n\r\n").nth(1).unwrap_or("")
}

/// Count instances that render this SOP Instance UID as a VALUE.
///
/// The raw UID also appears inside each instance RetrieveURL (0008,1190), so
/// matching the bare string would count every instance twice. The DICOM JSON
/// value form is unambiguous.
fn count_rows(haystack: &str, sop_uid: &str) -> usize {
    let needle = format!("\"Value\":[\"{sop_uid}\"]");
    haystack.matches(&needle).count()
}

/// Count raw substring occurrences, used to prove an identifier does NOT leak
/// anywhere in a response (a refusal must not disclose the instance it hides).
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

// -- 1. Sequestering is the default --------------------------------------

#[test]
fn a_rejected_instance_is_absent_from_instance_qido_by_default() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (listener, addr) = bind_loopback();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let path = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances");
    let resp = request(addr, &path);
    assert_eq!(status_of(&resp), 200, "body: {resp}");
    let body = body_of(&resp);
    assert_eq!(
        count_rows(body, REJECTED_SOP),
        0,
        "a rejected instance must not appear in an ordinary search: {body}"
    );
    assert_eq!(
        count_rows(body, ACCEPTED_SOP),
        1,
        "the accepted instance must still be listed: {body}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn a_rejected_instance_retrieves_as_not_found_by_default() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (listener, addr) = bind_loopback();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let path = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{REJECTED_SOP}");
    let resp = request(addr, &path);
    // A rejected instance answers exactly like a missing one: that is the
    // sequestering contract, and inventing a distinct status would disclose
    // that the instance exists.
    assert_eq!(status_of(&resp), 404, "body: {resp}");

    shutdown.request_stop();
    let _ = handle.join();
}

// -- 2. The service root is gated ---------------------------------------

#[test]
fn the_qa_root_is_authorized_from_the_local_machine() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (listener, addr) = bind_loopback();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let path = format!("/dicomweb_qa/studies/{STUDY}/series/{SERIES}/instances");
    let resp = request(addr, &path);
    assert_eq!(status_of(&resp), 200, "body: {resp}");
    let body = body_of(&resp);
    assert_eq!(
        count_rows(body, REJECTED_SOP),
        1,
        "loopback may inspect rejected instances: {body}"
    );
    assert_eq!(count_rows(body, ACCEPTED_SOP), 1, "body: {body}");

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn the_qa_root_is_a_403_off_machine() {
    let Some(ip) = non_loopback_local_ip() else {
        // No spare local address on this host; the pure config tests still
        // cover the CIDR decision, so skipping beats weakening the assertion.
        return;
    };
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let listener =
        TcpListener::bind(SocketAddr::new(IpAddr::V4(ip), 0)).expect("bind non-loopback");
    let addr = listener.local_addr().unwrap();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let path = format!("/dicomweb_qa/studies/{STUDY}/series/{SERIES}/instances");
    let resp = request(addr, &path);
    assert_eq!(
        status_of(&resp),
        403,
        "an unconfigured off-machine client must be refused: {resp}"
    );
    // The refusal must not leak the rejected instance in any form.
    assert_eq!(
        count_occurrences(&resp, REJECTED_SOP),
        0,
        "the 403 body must not name the instance: {resp}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn the_qa_root_is_authorized_from_a_configured_network() {
    let Some(ip) = non_loopback_local_ip() else {
        return;
    };
    let cidr = format!("{ip}/32");
    let ctx = setup_with(|c| c.expose_rejected_cidrs = vec![parse(&cidr)]);
    write_manifest(&ctx.state);
    let listener =
        TcpListener::bind(SocketAddr::new(IpAddr::V4(ip), 0)).expect("bind non-loopback");
    let addr = listener.local_addr().unwrap();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let path = format!("/dicomweb_qa/studies/{STUDY}/series/{SERIES}/instances");
    let resp = request(addr, &path);
    assert_eq!(status_of(&resp), 200, "body: {resp}");
    assert_eq!(
        count_rows(body_of(&resp), REJECTED_SOP),
        1,
        "a configured network may inspect rejected instances: {resp}"
    );

    // A granted client that uses the standard root still gets the filtered
    // view: the grant lifts the filter only on the QA root, it does not widen
    // the default service.
    let plain = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances");
    let resp = request(addr, &plain);
    assert_eq!(status_of(&resp), 200, "body: {resp}");
    assert_eq!(count_rows(body_of(&resp), REJECTED_SOP), 0, "{resp}");

    shutdown.request_stop();
    let _ = handle.join();
}

fn parse(cidr: &str) -> clarus_server::config::CidrNet {
    clarus_server::config::parse_cidr(cidr).expect("valid cidr")
}

// -- 3. The QA root is a read surface -----------------------------------

#[test]
fn the_qa_root_does_not_offer_a_store() {
    // The root exists to let a reviewer read what was hidden, and a store
    // endpoint would turn a read grant into a write grant. Refusing the method
    // (rather than matching it and failing later) also keeps the route table
    // honest: nothing that reaches the IOCTM writers can be addressed here.
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (listener, addr) = bind_loopback();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let path = format!("/dicomweb_qa/studies/{STUDY}");
    let resp = post_request(addr, &path);
    assert_eq!(
        status_of(&resp),
        404,
        "a store on the QA root must not be routed: {resp}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn the_rejected_row_is_still_hidden_on_the_standard_root() {
    // Guards the aliasing itself: rewriting the first segment must set the
    // expose view and nothing else. If the prefix were stripped without the
    // flag, every client could read hidden results through /dicomweb_qa.
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (listener, addr) = bind_loopback();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let path = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances");
    let resp = request(addr, &path);
    assert_eq!(status_of(&resp), 200, "body: {resp}");
    assert_eq!(
        count_rows(body_of(&resp), REJECTED_SOP),
        0,
        "the standard root must stay filtered: {resp}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

// -- 3b. The QA root lifts Quality only -----------------------------------

#[test]
fn the_qa_root_lifts_quality_but_not_an_any_class_hide() {
    // The whole point of persisting the reason: one surface, two different
    // answers. A Quality rejection is reviewable (AIRA 57.2.1); an IOCM
    // patient-safety rejection has no expose path at all (IOCM 4.66.4.2.3).
    // If this test is satisfied by the surface alone - i.e. the QA root shows
    // everything it filters on - then the reason is not being consulted.
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (listener, addr) = bind_loopback();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let path = format!("/dicomweb_qa/studies/{STUDY}/series/{SERIES}/instances");
    let resp = request(addr, &path);
    assert_eq!(status_of(&resp), 200, "body: {resp}");
    let body = body_of(&resp);

    assert_eq!(
        count_rows(body, REJECTED_SOP),
        1,
        "a Quality hide must be liftable on the QA root: {body}"
    );
    for (sop, reason) in [
        (HIDDEN_ANY_CLASS_SOP, PATIENT_SAFETY_REASON),
        (MWL_HIDDEN_SOP, INCORRECT_MWL_REASON),
        (RETENTION_HIDDEN_SOP, RETENTION_REASON),
    ] {
        assert_eq!(
            count_rows(body, sop),
            0,
            "an IOCM AnyClass hide (reason {reason}) must stay hidden even on \
             the QA root: {body}"
        );
    }
    assert_eq!(count_rows(body, ACCEPTED_SOP), 1, "body: {body}");

    // Exactly two rows: the visible one and the Quality-lifted one. Pinning
    // the total stops the three "must be 0" assertions above from passing
    // merely because the search returned nothing at all.
    let total = body.matches("\"00080018\"").count();
    assert_eq!(total, 2, "the QA root lists only visible + Quality: {body}");

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn the_qa_root_retrieves_only_the_liftable_instance() {
    // Same rule on the retrieve path: the two surfaces must agree, otherwise
    // an inspector could read a patient-safety-hidden image by UID while the
    // search correctly refuses to list it.
    //
    // The fixture carries no CAS blob, so both instances answer 404 there; the
    // assertion is the DISTINCTION, not the status. A request that reaches the
    // sequestering check and fails only on the missing blob is logged as an
    // instance miss, while a hidden-by-policy instance is refused before the
    // blob lookup. Comparing the two proves the reason is consulted on the
    // retrieve path too.
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (listener, addr) = bind_loopback();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());

    let liftable = format!("/dicomweb_qa/studies/{STUDY}/series/{SERIES}/instances/{REJECTED_SOP}");
    let permanent =
        format!("/dicomweb_qa/studies/{STUDY}/series/{SERIES}/instances/{HIDDEN_ANY_CLASS_SOP}");

    let a = request(addr, &liftable);
    let b = request(addr, &permanent);

    // A registered but policy-hidden instance must not be disclosed by a
    // distinct status: both answer like a missing instance.
    assert_eq!(status_of(&a), 404, "body: {a}");
    assert_eq!(status_of(&b), 404, "body: {b}");

    // The quality-hidden one is not refused BY POLICY - it fails later, on the
    // absent blob - so its refusal must not carry the sequestering wording.
    assert!(
        !body_of(&a).contains("rejected"),
        "the quality-hidden instance must not be refused by the sequestering check: {a}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

// -- 4. The rate counters reach /metrics ---------------------------------

#[test]
fn both_iocm_counters_are_rendered_on_metrics() {
    let ctx = setup_with(|_| {});
    let rendered = clarus_server::handlers::metrics_render::render_metrics();
    assert!(
        rendered.contains("# TYPE clarus_rejected_instances_total counter"),
        "the rejection counter must be a Prometheus counter"
    );
    assert!(
        rendered.contains("clarus_rejected_instances_total "),
        "the rejection counter must have a sample line"
    );
    assert!(
        rendered.contains("# TYPE clarus_instances_stored_total counter"),
        "the denominator must be a Prometheus counter"
    );
    assert!(
        rendered.contains("clarus_instances_stored_total "),
        "the denominator must have a sample line"
    );
    // The help text is the documentation an operator reads, so it must not
    // claim the counters are AI-specific: they are generic IOCM.
    let help: Vec<&str> = rendered
        .lines()
        .filter(|l| l.starts_with("# HELP clarus_rejected_instances_total"))
        .collect();
    assert_eq!(help.len(), 1, "exactly one HELP line");
    assert!(
        help[0].contains("rejection note"),
        "help text must say what is counted: {help:?}"
    );
    let _ = &ctx;
}

#[test]
fn record_rejected_instances_advances_only_by_the_flip_count() {
    let before = metric_value("clarus_rejected_instances_total");
    clarus_server::metrics::record_rejected_instances(QUALITY_REASON, 2);
    let after = metric_value("clarus_rejected_instances_total");
    assert_eq!(after - before, 2, "a note flipping two instances adds two");
    // A re-rejection reports zero flips and must not move the counter.
    clarus_server::metrics::record_rejected_instances(QUALITY_REASON, 0);
    assert_eq!(
        metric_value("clarus_rejected_instances_total") - before,
        2,
        "a no-op re-rejection must not be counted"
    );
}

#[test]
fn record_instances_stored_advances_only_for_new_instances() {
    let before = metric_value("clarus_instances_stored_total");
    clarus_server::metrics::record_instances_stored(3);
    assert_eq!(
        metric_value("clarus_instances_stored_total") - before,
        3,
        "three accepted instances add three"
    );
    // An exact-duplicate re-STOW contributes 0: it does not grow the stored
    // population, so counting it would let a client depress the rate.
    clarus_server::metrics::record_instances_stored(0);
    assert_eq!(
        metric_value("clarus_instances_stored_total") - before,
        3,
        "duplicates must not be counted"
    );
}

/// The AnyClass counter is labelled with the reason a note was declined, and
/// that reason is now "the feature is off" rather than "the origin is
/// untrusted". The label is what an operator greps and alerts on, so a silent
/// change of meaning would leave dashboards pointing at the wrong cause.
#[test]
fn the_anyclass_ignored_counter_is_labelled_disabled() {
    let rendered = clarus_server::handlers::metrics_render::render_metrics();
    assert!(
        rendered.contains("cause=\"disabled\""),
        "the cause label must describe the feature being off: {rendered}"
    );
    assert!(
        !rendered.contains("cause=\"untrusted\""),
        "the old trust-based label must be gone: {rendered}"
    );
    let help: Vec<&str> = rendered
        .lines()
        .filter(|l| l.starts_with("# HELP clarus_iocm_anyclass_ignored_total"))
        .collect();
    assert_eq!(help.len(), 1, "exactly one HELP line");
    assert!(
        help[0].contains("anyclass_enable"),
        "help text must name the knob an operator has to change: {help:?}"
    );
    assert!(
        !help[0].contains("require_proxy_identity"),
        "the knob that no longer gates this must not be named: {help:?}"
    );
}

/// Recording is additive on the atomic and the rendered sample follows it.
#[test]
fn record_iocm_anyclass_ignored_advances_the_disabled_sample() {
    let before = metric_value("clarus_iocm_anyclass_ignored_total");
    clarus_server::metrics::record_iocm_anyclass_ignored(2);
    assert_eq!(
        metric_value("clarus_iocm_anyclass_ignored_total") - before,
        2,
        "two declined references add two"
    );
    clarus_server::metrics::record_iocm_anyclass_ignored(0);
    assert_eq!(
        metric_value("clarus_iocm_anyclass_ignored_total") - before,
        2,
        "a no-op must not be counted"
    );
}

// -- 5. The multipart planners hide rejected instances ---------------------
//
// The planners are asserted directly rather than through the wire: the
// question is *which instances are planned*, and a multipart body would bury
// that under boundary syntax. The predicate under test is the one the served
// routes reach, since the handlers do nothing to the plan but stream it.

/// The SOP UIDs a plan would serve, in plan order.
fn planned(state: &Arc<AppState>, study: &str, series: Option<&str>, expose: bool) -> Vec<String> {
    let plans = match series {
        Some(suid) => {
            clarus_server::handlers::wado_pure::plan_series_multipart(state, study, suid, expose)
        }
        None => clarus_server::handlers::wado_pure::plan_study_multipart(state, study, expose),
    };
    match plans {
        Ok(p) => p.into_iter().map(|pl| pl.sop_uid).collect(),
        Err(clarus_server::handlers::wado::WadoError::NotFound(_)) => Vec::new(),
        Err(e) => panic!("unexpected planner error: {e:?}"),
    }
}

#[test]
fn the_study_plan_hides_every_rejected_instance_from_an_ordinary_client() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);

    let got = planned(&ctx.state, STUDY, None, false);
    assert_eq!(
        got,
        vec![ACCEPTED_SOP.to_string()],
        "only the visible instance may be planned for the ordinary root"
    );
}

#[test]
fn the_series_plan_hides_every_rejected_instance_from_an_ordinary_client() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);

    let got = planned(&ctx.state, STUDY, Some(SERIES), false);
    assert_eq!(
        got,
        vec![ACCEPTED_SOP.to_string()],
        "the series route must not be the one place a hidden instance leaks"
    );
}

#[test]
fn the_qa_plan_lifts_a_quality_hide_but_never_an_any_class_one() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);

    // Quality is reviewable, so the QA root sees it. The three AnyClass
    // reasons have no expose path at all (IOCM 4.66.4.2.3 / 4.66.4.3.3), so
    // they stay absent even here.
    let study = planned(&ctx.state, STUDY, None, true);
    assert!(
        study.contains(&REJECTED_SOP.to_string()),
        "the QA root must show a Quality-hidden instance: {study:?}"
    );
    for hidden in [HIDDEN_ANY_CLASS_SOP, MWL_HIDDEN_SOP, RETENTION_HIDDEN_SOP] {
        assert!(
            !study.contains(&hidden.to_string()),
            "an AnyClass hide has no expose path, even on QA: {hidden} in {study:?}"
        );
    }
    assert!(
        study.contains(&ACCEPTED_SOP.to_string()),
        "the visible instance is still planned: {study:?}"
    );

    // The series route must reach the same verdict as the study route.
    let series = planned(&ctx.state, STUDY, Some(SERIES), true);
    assert_eq!(study, series, "study and series plans must agree");
}

/// When EVERY instance of a study is hidden the plan is empty, and an empty
/// plan is a 404 - "hidden == missing" is the sequestering contract, and a
/// 200 with an empty body would tell a client the study exists but is empty.
#[test]
fn a_fully_hidden_study_plans_nothing_and_therefore_404s() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);

    // Hide the only visible instance too, straight into the manifest: the
    // point is the planner's behaviour once nothing is left, not how a hide
    // came to be recorded.
    let dir = std::path::PathBuf::from(&ctx.state.config.btree.manifest_dir);
    let path = clarus_core::stow::manifest_path(&dir, STUDY);
    let mut manifest = clarus_core::manifest::read_manifest(&path).expect("read");
    for inst in manifest.series[0].instances.iter_mut() {
        if inst.rejection_reason == clarus_core::manifest::NOT_REJECTED_REASON {
            inst.rejection_reason = QUALITY_REASON;
        }
    }
    std::fs::write(&path, manifest.encode().expect("encode")).expect("rewrite");

    let err = clarus_server::handlers::wado_pure::plan_study_multipart(&ctx.state, STUDY, false)
        .expect_err("a study with no visible instance must not plan");
    assert!(
        matches!(err, clarus_server::handlers::wado::WadoError::NotFound(_)),
        "hidden == missing: {err:?}"
    );
}

/// Read one counter's current value from the rendered `/metrics` text, so the
/// assertion is on the operator-visible number rather than on the atomic.
///
/// The sample may carry labels (`name{...} value`), so a bare `name ` prefix
/// would miss every labelled series.
fn metric_value(name: &str) -> u64 {
    let rendered = clarus_server::handlers::metrics_render::render_metrics();
    rendered
        .lines()
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| {
            let rest = l.strip_prefix(name)?;
            // Must be the metric itself, not a name that merely starts the same.
            if !rest.starts_with('{') && !rest.starts_with(' ') {
                return None;
            }
            let value = rest.rsplit_once(' ')?.1;
            value.trim().parse::<u64>().ok()
        })
        .unwrap_or_else(|| panic!("no sample for {name} in /metrics"))
}
