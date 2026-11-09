// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Wire tests for the admin list / view / restore surface for sequestered
//! instances.
//!
//! Three claims, each needing a different kind of proof:
//!
//! 1. LIST shows what the rejection notes hid, with the reason attached. The
//!    reason is the point: without it the operator cannot tell a reviewable
//!    Quality rejection from a patient-safety one.
//! 2. RESTORE actually un-hides. The honest way to assert that is not "the
//!    manifest says visible" but "the read path shows it again", so the test
//!    goes through the ordinary DICOMweb root.
//! 3. Restoring is what re-opens re-STOW. An instance hidden by 113038 may not
//!    be stored again; after a restore the same UID is accepted again. That is
//!    the behaviour the whole feature exists for, and it is what the
//!    falsification targets.

#![forbid(unsafe_code)]

use clarus_core::http::conn::Timeouts;
use clarus_core::manifest::{
    AttrValue, InstanceRecord, Manifest, NOT_REJECTED_REASON, SeriesRecord, StudyQido, SysHeader,
    TIER_HOT,
};
use clarus_core::shutdown::Shutdown;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::{Duration, Instant};

const STUDY: &str = "1.2.840.10008.5.1.4.1.1.88.22.1";
const SERIES: &str = "1.2.840.10008.5.1.4.1.1.88.22.2";
const HIDDEN_SOP: &str = "1.2.840.10008.5.1.4.1.1.88.22.3";
const VISIBLE_SOP: &str = "1.2.840.10008.5.1.4.1.1.88.22.4";

const INCORRECT_MWL: u32 = 113_038;

struct TestCtx {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
}

fn setup_with(mutate: impl FnOnce(&mut Config)) -> TestCtx {
    let temp = tempfile::tempdir().expect("temp dir");
    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    let audit_dir = btree_root.join("audit");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&idx_dir).unwrap();
    std::fs::create_dir_all(&audit_dir).unwrap();

    let mut config = Config::test_default();
    mutate(&mut config);
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.btree.audit_dir = audit_dir.to_string_lossy().to_string();
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
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
        config,
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(8),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    };

    TestCtx {
        _temp: temp,
        state: Arc::new(state),
    }
}

/// A study manifest with one instance hidden by `reason` and one visible.
fn write_manifest(state: &AppState) {
    let make = |sop: &str, number: u64, reason: u32| InstanceRecord {
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
        rejection_reason: reason,
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
                (
                    clarus_core::query::tag::PATIENT_NAME,
                    AttrValue::Text("DOE^JANE".to_string()),
                ),
            ],
            ..Default::default()
        },
        series: vec![SeriesRecord {
            attrs: vec![
                (
                    clarus_core::query::tag::SERIES_INSTANCE_UID,
                    AttrValue::Text(SERIES.to_string()),
                ),
                (
                    clarus_core::query::tag::MODALITY,
                    AttrValue::Text("SEG".to_string()),
                ),
            ],
            instances: vec![
                make(HIDDEN_SOP, 1, INCORRECT_MWL),
                make(VISIBLE_SOP, 2, NOT_REJECTED_REASON),
            ],
        }],
    };

    let dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    let path = clarus_core::stow::manifest_path(&dir, STUDY);
    std::fs::write(&path, manifest.encode().expect("encode")).expect("write manifest");
    // The restore path resolves the owner study through the SOP index, which a
    // real STOW writes; without it every restore would look like "not found".
    state
        .shadow
        .dual_write([0u8; 32], "SEG", STUDY, HIDDEN_SOP)
        .expect("seed sop_owner");
    state
        .shadow
        .dual_write([0u8; 32], "SEG", STUDY, VISIBLE_SOP)
        .expect("seed sop_owner");
}

fn bind_loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    (listener, addr)
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

fn post_json(addr: SocketAddr, path: &str, body: &str) -> String {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
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

/// Count instances that render this SOP Instance UID as a DICOM JSON VALUE.
///
/// The raw UID also appears inside each instance's RetrieveURL (0008,1190), so
/// a bare substring count sees every instance twice and cannot distinguish
/// "present" from "present plus its URL".
fn count_rows(haystack: &str, sop_uid: &str) -> usize {
    let needle = format!("\"Value\":[\"{sop_uid}\"]");
    haystack.matches(&needle).count()
}

/// Start a server and return the pieces the tests need to shut it down.
fn serve(ctx: &TestCtx) -> (SocketAddr, Shutdown, std::thread::JoinHandle<()>) {
    let (listener, addr) = bind_loopback();
    let shutdown = Shutdown::new();
    let handle = start_server(listener, &ctx.state, shutdown.clone());
    std::thread::sleep(Duration::from_millis(50));
    (addr, shutdown, handle)
}

// -- 1. LIST -------------------------------------------------------------

#[test]
fn the_hidden_list_reports_the_reason_and_only_hidden_instances() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (addr, shutdown, handle) = serve(&ctx);

    let resp = request(addr, "/admin/v1/instances/hidden");
    assert_eq!(status_of(&resp), 200, "body: {resp}");
    let body = body_of(&resp);

    assert_eq!(
        body.matches(HIDDEN_SOP).count(),
        1,
        "the hidden instance is listed: {body}"
    );
    assert_eq!(
        body.matches(VISIBLE_SOP).count(),
        0,
        "a visible instance must not appear in the hidden list: {body}"
    );
    // The reason is the whole point of the past Stage-2 work: an operator has
    // to be able to tell a reviewable Quality hide from a patient-safety one.
    assert!(
        body.contains("\"reason_code\":113038"),
        "the reason code travels with the row: {body}"
    );
    assert!(
        body.contains("Incorrect Modality Worklist Entry"),
        "the reason is rendered in human terms: {body}"
    );
    // PatientID is the identifier and is always present; names are secondary.
    assert!(body.contains("PAT-1"), "the identifier is carried: {body}");

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn the_hidden_list_filters_by_reason() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (addr, shutdown, handle) = serve(&ctx);

    let matching = request(addr, "/admin/v1/instances/hidden?reason=113038");
    assert_eq!(status_of(&matching), 200, "body: {matching}");
    assert_eq!(body_of(&matching).matches(HIDDEN_SOP).count(), 1);

    let other = request(addr, "/admin/v1/instances/hidden?reason=113001");
    assert_eq!(status_of(&other), 200, "body: {other}");
    assert_eq!(
        body_of(&other).matches(HIDDEN_SOP).count(),
        0,
        "a different reason must not match: {other}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

// -- 2. RESTORE ----------------------------------------------------------

#[test]
fn restore_requires_an_operator_reason() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (addr, shutdown, handle) = serve(&ctx);

    // No reason at all.
    let resp = post_json(
        addr,
        "/admin/v1/instances/restore",
        r#"{"sop_instance_uids":["1.2.840.10008.5.1.4.1.1.88.22.3"]}"#,
    );
    assert_eq!(status_of(&resp), 400, "body: {resp}");

    // A reason that is only whitespace is not a reason.
    let blank = post_json(
        addr,
        "/admin/v1/instances/restore",
        r#"{"sop_instance_uids":["1.2.840.10008.5.1.4.1.1.88.22.3"],"reason":"   "}"#,
    );
    assert_eq!(status_of(&blank), 400, "body: {blank}");

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn restore_makes_the_instance_visible_on_the_ordinary_root() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (addr, shutdown, handle) = serve(&ctx);

    // Hidden before: absent from the ordinary search.
    let before = request(
        addr,
        &format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances"),
    );
    assert_eq!(status_of(&before), 200, "body: {before}");
    assert_eq!(
        count_rows(body_of(&before), HIDDEN_SOP),
        0,
        "the instance starts hidden: {before}"
    );

    let restored = post_json(
        addr,
        "/admin/v1/instances/restore",
        r#"{"sop_instance_uids":["1.2.840.10008.5.1.4.1.1.88.22.3"],"reason":"QA confirmed correct patient"}"#,
    );
    assert_eq!(status_of(&restored), 200, "body: {restored}");
    assert!(
        body_of(&restored).contains("\"restored_instances\":1"),
        "exactly one instance is restored: {restored}"
    );

    // Visible after: this is the claim that matters - the read path, not the
    // manifest field, is what an operator actually observes.
    let after = request(
        addr,
        &format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances"),
    );
    assert_eq!(status_of(&after), 200, "body: {after}");
    assert_eq!(
        count_rows(body_of(&after), HIDDEN_SOP),
        1,
        "the restored instance is searchable again: {after}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

#[test]
fn restoring_an_already_visible_instance_reports_nothing_to_do() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (addr, shutdown, handle) = serve(&ctx);

    let resp = post_json(
        addr,
        "/admin/v1/instances/restore",
        r#"{"sop_instance_uids":["1.2.840.10008.5.1.4.1.1.88.22.4"],"reason":"no-op check"}"#,
    );
    assert_eq!(status_of(&resp), 200, "body: {resp}");
    // Restore is idempotent, but it must not CLAIM a state change - that is
    // what keeps clarus_restored_total honest.
    assert!(
        body_of(&resp).contains("\"restored_instances\":0"),
        "a visible instance is not counted as restored: {resp}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

// -- 2b. Re-STOW after restore -------------------------------------------

#[test]
fn restore_reopens_the_re_stow_path_that_the_rejection_closed() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (addr, shutdown, handle) = serve(&ctx);

    // The refusal predicate is the one STOW consults, so asserting on it
    // proves the policy flip without standing up a second multipart client.
    let dir = std::path::PathBuf::from(&ctx.state.config.btree.manifest_dir);
    assert_eq!(
        clarus_core::stow::hidden_reason_of(&dir, STUDY, HIDDEN_SOP),
        INCORRECT_MWL,
        "an incorrect-MWL rejection must refuse a re-STOW"
    );

    let restored = post_json(
        addr,
        "/admin/v1/instances/restore",
        r#"{"sop_instance_uids":["1.2.840.10008.5.1.4.1.1.88.22.3"],"reason":"MWL corrected, re-accepting"}"#,
    );
    assert_eq!(status_of(&restored), 200, "body: {restored}");
    assert!(
        body_of(&restored).contains("\"restored_instances\":1"),
        "the sequester is lifted: {restored}"
    );

    assert_eq!(
        clarus_core::stow::hidden_reason_of(&dir, STUDY, HIDDEN_SOP),
        NOT_REJECTED_REASON,
        "after restore a re-STOW must no longer be refused"
    );
    // The untouched instance keeps its own state: a restore is per instance,
    // never a study-wide un-reject.
    assert_eq!(
        clarus_core::stow::hidden_reason_of(&dir, STUDY, VISIBLE_SOP),
        NOT_REJECTED_REASON
    );

    shutdown.request_stop();
    let _ = handle.join();
}

// -- 3. The audit row ----------------------------------------------------

#[test]
fn restore_writes_an_immutable_audit_row_with_the_reason_it_cleared() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (addr, shutdown, handle) = serve(&ctx);

    let resp = post_json(
        addr,
        "/admin/v1/instances/restore",
        r#"{"sop_instance_uids":["1.2.840.10008.5.1.4.1.1.88.22.3"],"reason":"wrong worklist confirmed, images correct"}"#,
    );
    assert_eq!(status_of(&resp), 200, "body: {resp}");

    // The delete trail is the record: a restore without an audit line would be
    // an unexplained un-sequester, which is worse than no restore at all.
    let audit_dir = std::path::PathBuf::from(&ctx.state.config.btree.audit_dir);
    let mut found = String::new();
    for entry in std::fs::read_dir(&audit_dir).expect("audit dir").flatten() {
        found.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
    }
    assert!(
        found.contains("\"event\":\"restore\""),
        "a restore row is appended: {found}"
    );
    assert!(
        found.contains("\"reason_code\":113038"),
        "the row names the reason being cleared: {found}"
    );
    assert!(
        found.contains("wrong worklist confirmed"),
        "the operator's reason is preserved: {found}"
    );
    assert!(
        found.contains(HIDDEN_SOP),
        "the target instance is named: {found}"
    );
    assert!(
        found.contains("PAT-1"),
        "PatientID is carried as the identifier: {found}"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

// -- 4. VIEW -------------------------------------------------------------

#[test]
fn raw_view_404s_for_an_unknown_instance() {
    let ctx = setup_with(|_| {});
    write_manifest(&ctx.state);
    let (addr, shutdown, handle) = serve(&ctx);

    let resp = request(
        addr,
        "/admin/v1/instances/raw?sop_instance_uid=1.2.3.4.5.6.7.8",
    );
    assert_eq!(status_of(&resp), 404, "body: {resp}");

    // A missing parameter is a client error, not a 404: the route exists and
    // the caller simply did not say which instance.
    let missing = request(addr, "/admin/v1/instances/raw");
    assert_eq!(status_of(&missing), 400, "body: {missing}");

    shutdown.request_stop();
    let _ = handle.join();
}
