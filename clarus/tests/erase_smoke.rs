// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! lazy-erase smoke - tombstone + background erase-worker.
//!
//! Verifies the erase/recovery contract with the eight hard gates adapted to
//! an integration test (hotpath/criterion benches are out of scope here):
//!   1. Visibility is instant - the tombstone rename alone hides the study.
//!   2. Dedup safety - a shared blake3 survives while any study references it.
//!   3. Worker is idempotent / crash-replay safe.
//!   4. Resurrection-guard - STOW onto a tombstoned UID -> 409, then OK.
//!   6. Windows-style open-handle retry - worker retries, no crash, gauge stays.
//!   7. API - 200 + `erase:"pending"`; dry_run touches nothing.
//!   8. Audit survives - `delete` before tombstone, `erase_complete` after.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::manifest::read_manifest;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::erase_worker::{ERASE_PAUSE, erase_once};
use clarus_server::handlers::json_common::{JsonVal, parse_json};
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Minimal DICOM builder (Explicit VR LE, uncompressed) ---------------

fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let extended = matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UN" | b"UT"
    );
    if vr == b"UI" && !len.is_multiple_of(2) {
        if extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&((len + 1) as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&((len + 1) as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        buf.push(0x00);
    } else {
        if extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !len.is_multiple_of(2) {
            buf.push(0x00);
        }
    }
}

fn build_instance(sop: &str, study: &str, series: &str, pixel_byte: u8) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"ERASE^SMOKE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"ERASE");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &[pixel_byte; 64]);
    d
}

/// Build a minimal instance with EMPTY PatientID and PatientName so
/// `validate_and_coerce` must generate them (AUTOPAT_ / Anonymous^) and the
/// INSERT writes a COERCE alibi audit copy (W3, stow_pure).
fn build_coerced_instance(sop: &str, study: &str, series: &str, pixel_byte: u8) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop.as_bytes());
    // PatientName (0010,0010) and PatientID (0010,0020) are intentionally absent.
    append_element(&mut d, 0x0020, 0x000d, b"UI", study.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &[pixel_byte; 64]);
    d
}

// -- Test server harness (keeps AppState for direct worker calls) --------

struct TestServer {
    addr: SocketAddr,
    state: AppState,
    _temp: tempfile::TempDir,
}

impl TestServer {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");

        let storage_dir = temp.path().join("cas");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");
        let btree_root = temp.path().join("btree");
        std::fs::create_dir_all(&btree_root).expect("btree dir");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");

        let mut config = Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.btree.audit_dir = btree_root.join("audit").to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        let state = AppState {
            cas,
            codec_registry: clarus_adapters::codec::CodecRegistry::new(),
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
            study_scanner: StudyScanner::new(manifest_dir.clone(), idx_dir.clone()),
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let server_state = Arc::new(state.clone());
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let sd = shutdown.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, server_state, sd, timeouts, u32::MAX);
        });

        Self {
            addr,
            state,
            _temp: temp,
        }
    }

    fn manifests(&self) -> PathBuf {
        PathBuf::from(&self.state.config.btree.manifest_dir)
    }

    fn manifest(&self, study: &str) -> PathBuf {
        self.manifests().join(format!("{study}.manifest"))
    }

    fn tombstone(&self, study: &str) -> PathBuf {
        clarus_core::stow::tombstone_path(&self.manifests(), study)
    }

    fn audit_dir(&self) -> PathBuf {
        PathBuf::from(&self.state.config.btree.audit_dir)
    }

    fn post_dicom(&self, path: &str, body: &[u8]) -> Response {
        self.request("POST", path, "application/dicom", body)
    }

    fn post_json(&self, path: &str, body: &[u8]) -> Response {
        self.request("POST", path, "application/json", body)
    }

    fn request(&self, method: &str, path: &str, content_type: &str, body: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let ct = if content_type.is_empty() {
            String::new()
        } else {
            format!("Content-Type: {content_type}\r\n")
        };
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{ct}Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.addr.port(),
            body.len()
        );
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
        .expect("status");
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
            "status should be {expected} (body: {})",
            String::from_utf8_lossy(&self.body)
        );
    }
    fn json(&self) -> JsonVal {
        parse_json(&self.body).expect("json body")
    }
}

/// STOW one instance over HTTP; asserts 200 OK (full success).
/// Returns the study UID.
fn stow_one(srv: &TestServer, study: &str, series: &str, sop: &str) {
    let dcm = build_instance(sop, study, series, 0x42);
    let r = srv.post_dicom("/dicomweb/studies", &dcm);
    assert_eq!(
        r.status,
        200,
        "STOW single instance should be 200, got {} body {}",
        r.status,
        String::from_utf8_lossy(&r.body)
    );
}

/// A helper: run `erase_once` until all tombstones are drained or timeout.
fn drain_worker(srv: &TestServer, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let stats = erase_once(&srv.state);
        // Done when nothing is pending and not blocked on a retryable error.
        if stats.errors == 0 && (stats.lifted > 0 || stats.tombstones == 0) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("erase worker did not drain tombstones in {:?}", timeout);
}

fn audit_log_content(srv: &TestServer) -> String {
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(srv.audit_dir()) {
        for e in entries.flatten() {
            if let Ok(text) = std::fs::read_to_string(e.path()) {
                out.push_str(&text);
            }
        }
    }
    out
}

// -- Gate 7: API - 200 + erase:pending; dry_run touches nothing ----------

#[test]
fn api_delete_pending_and_dry_run() {
    let srv = TestServer::start();
    let study = "1.2.3.10";
    let series = "1.2.3.10.1";
    let sop = "1.2.3.10.1.1";
    let dcm = build_instance(sop, study, series, 0x42);
    srv.post_dicom("/dicomweb/studies", &dcm).assert_status(200);
    let cas_path = srv.state.cas.cas_path(&hex_of_blake3(&dcm));
    assert!(srv.manifest(study).is_file(), "study manifest present");

    // dry_run: preview only - nothing touched.
    let dry = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"test dry","dry_run":true}}"#).as_bytes(),
    );
    dry.assert_status(200);
    let dj = dry.json();
    assert_eq!(
        dj.obj_get("erase").and_then(|x| x.as_str()),
        Some("preview")
    );
    assert!(
        dj.obj_get("bytes_to_free")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
            > 0
    );
    assert!(srv.manifest(study).is_file(), "dry_run leaves manifest");
    assert!(
        !srv.tombstone(study).exists(),
        "dry_run leaves no tombstone"
    );
    assert!(cas_path.is_file(), "dry_run leaves CAS file");

    // 2808 delete-trail identity: the preview row carries WHO/WHEN/WHOSE.
    let trail = audit_log_content(&srv);
    assert!(
        trail.contains("\"event\":\"delete_preview\""),
        "dry-run writes a delete_preview row: {trail}"
    );
    assert!(
        trail.contains("\"patient_id\":\"ERASE\""),
        "preview row carries the study's PatientID: {trail}"
    );
    // Date-prefix follows TODAY's UTC date (a hardcoded calendar date
    // breaks the suite at midnight).
    let today = clarus_server::access_log::utc_ts_iso();
    assert!(
        trail.contains(&format!("\"ts\":\"{}", &today[..11])),
        "preview row carries a second-resolution UTC ts: {trail}"
    );

    // real delete: 200 + erase:pending, manifest gone, tombstone present.
    let real = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"test real"}}"#).as_bytes(),
    );
    real.assert_status(200);
    let rj = real.json();
    assert_eq!(
        rj.obj_get("erase").and_then(|x| x.as_str()),
        Some("pending")
    );
    assert!(
        rj.obj_get("bytes_to_free")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
            > 0
    );
    assert!(!srv.manifest(study).exists(), "manifest renamed away");
    assert!(srv.tombstone(study).is_file(), "tombstone present");

    // The delete row itself carries the same identity fields.
    let trail = audit_log_content(&srv);
    assert!(
        trail.contains("\"event\":\"delete\",\"batch_id\""),
        "delete row present: {trail}"
    );
    assert!(
        trail.contains("\"patient_id\":\"ERASE\""),
        "delete row carries the PatientID: {trail}"
    );

    // After the worker drains, the CAS file is gone and the tombstone lifted.
    drain_worker(&srv, Duration::from_secs(5));
    assert!(
        !srv.tombstone(study).exists(),
        "tombstone lifted after erase"
    );
    assert!(!cas_path.exists(), "CAS file removed by worker");
}

// -- Gate 8: audit survives - delete before tombstone, erase_complete after -

#[test]
fn audit_pair_same_batch_id() {
    let srv = TestServer::start();
    let study = "1.2.3.20";
    stow_one(&srv, study, "1.2.3.20.1", "1.2.3.20.1.1");

    let r = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"audit test"}}"#).as_bytes(),
    );
    r.assert_status(200);
    // The `delete` row must already be durably appended (before the worker
    // lifts the tombstone) and reference this study as its target.
    let before = audit_log_content(&srv);
    assert!(
        before.contains("\"event\":\"delete\"")
            && before.contains(&format!("\"target\":\"{study}\"")),
        "delete row written before erase: {before}"
    );
    // Date-prefix follows TODAY's UTC date (a hardcoded calendar date
    // breaks the suite at midnight).
    let today = clarus_server::access_log::utc_ts_iso();
    assert!(
        before.contains("\"patient_id\":\"ERASE\"")
            && before.contains(&format!("\"ts\":\"{}", &today[..11])),
        "delete row carries patient_id + second-resolution ts: {before}"
    );

    drain_worker(&srv, Duration::from_secs(5));

    let after = audit_log_content(&srv);
    assert!(
        after.contains("\"event\":\"erase_complete\""),
        "erase_complete written after erase: {after}"
    );
    // Both rows must share one batch_id.
    let deletes = json_events(&after, "delete")
        .into_iter()
        .map(|l| extract_batch(&l))
        .collect::<Vec<_>>();
    let completes = json_events(&after, "erase_complete")
        .into_iter()
        .map(|l| extract_batch(&l))
        .collect::<Vec<_>>();
    assert_eq!(deletes.len(), 1, "one delete row");
    assert_eq!(completes.len(), 1, "one erase_complete row");
    assert_eq!(deletes[0], completes[0], "same batch_id across the pair");
}

/// m9: `erase_complete` reports the tombstone's OWN file count and bytes.
///
/// Two erases drained by one worker pass each write a row naming their own
/// batch. Reporting the pass totals made every row but the last describe
/// another batch's files - a number that cannot be reconciled against the
/// `delete` row it is correlated with. Distinct file counts make the mix-up
/// visible.
#[test]
fn erase_complete_reports_per_tombstone_figures() {
    let srv = TestServer::start();
    // Study A: two instances, study B: one. Both tombstoned before the worker
    // runs, so a single pass drains them together.
    let (a, b) = ("1.2.3.30", "1.2.3.40");
    stow_one(&srv, a, "1.2.3.30.1", "1.2.3.30.1.1");
    stow_one(&srv, a, "1.2.3.30.1", "1.2.3.30.1.2");
    stow_one(&srv, b, "1.2.3.40.1", "1.2.3.40.1.1");

    for study in [a, b] {
        srv.post_json(
            "/admin/v1/instances/delete",
            format!(r#"{{"study_uid":"{study}","reason":"per-tombstone"}}"#).as_bytes(),
        )
        .assert_status(200);
    }
    assert!(srv.tombstone(a).is_file() && srv.tombstone(b).is_file());

    drain_worker(&srv, Duration::from_secs(5));

    let log = audit_log_content(&srv);
    let completes = json_events(&log, "erase_complete");
    assert_eq!(completes.len(), 2, "one row per tombstone: {log}");

    // Map batch_id -> files, then match each row against the study whose delete
    // row it correlates with.
    let mut files_by_batch = std::collections::HashMap::new();
    for line in &completes {
        let v = parse_json(line.as_bytes()).expect("row json");
        let files = v.obj_get("files").and_then(|f| f.as_u64()).unwrap_or(0);
        files_by_batch.insert(extract_batch(line), files);
    }
    let mut seen = Vec::new();
    for line in json_events(&log, "delete") {
        let v = parse_json(line.as_bytes()).expect("row json");
        if let Some(target) = v.obj_get("target").and_then(|t| t.as_str()) {
            let files = files_by_batch
                .get(&extract_batch(&line))
                .copied()
                .unwrap_or(0);
            seen.push((target.to_string(), files));
        }
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![(a.to_string(), 2u64), (b.to_string(), 1u64)],
        "each row must report its own tombstone's files, not the pass total"
    );
}

// -- Gate 2: dedup - shared blake3 survives while a live study references it -

#[test]
fn dedup_shared_hash_not_removed_until_all_gone() {
    let srv = TestServer::start();
    // Two studies artificially share one blake3 (identical CAS bytes). We
    // write both manifests directly instead of STOWing (which would differ by
    // SOP UID) so the shared-hash invariant is exact.
    let shared: [u8; 32] = [0xAB; 32];
    let shared_hex = clarus_adapters::hex_encode(&shared);
    let cas_path = srv.state.cas.cas_path(&shared_hex);
    if let Some(parent) = cas_path.parent() {
        std::fs::create_dir_all(parent).expect("cas bucket");
    }
    std::fs::write(&cas_path, b"shared payload").expect("write shared cas");

    let study_a = "1.2.3.31";
    let study_b = "1.2.3.32";
    write_manifest_direct(&srv, study_a, "1.2.3.31.1", "1.2.3.31.1.1", &shared);
    write_manifest_direct(&srv, study_b, "1.2.3.31.2", "1.2.3.31.2.1", &shared);
    assert!(srv.manifest(study_a).is_file());
    assert!(srv.manifest(study_b).is_file());

    // Delete study_a only -> the shared file must survive (study_b refs it).
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study_a}","reason":"dedupA"}}"#).as_bytes(),
    )
    .assert_status(200);
    drain_worker(&srv, Duration::from_secs(5));
    // A's reference was cleared; the file is retained (shared with B) and A's
    // tombstone is lifted because A owns nothing exclusive.
    assert!(
        !srv.tombstone(study_a).exists(),
        "A lifted after its ref moved to B"
    );
    assert!(
        cas_path.is_file(),
        "shared file retained while study_b lives"
    );

    // Delete study_b too -> now unreferenced, the worker unlinks.
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study_b}","reason":"dedupB"}}"#).as_bytes(),
    )
    .assert_status(200);
    drain_worker(&srv, Duration::from_secs(5));
    assert!(!cas_path.exists(), "shared file unlinked once unreferenced");
    assert!(!srv.tombstone(study_a).exists(), "A's tombstone lifted");
    assert!(!srv.tombstone(study_b).exists(), "B's tombstone lifted");
}

/// Write a study manifest (`{study}.manifest`) for one instance directly into
/// the server's manifests dir, pointing at a fabricated CAS hash.
fn write_manifest_direct(
    srv: &TestServer,
    study: &str,
    series: &str,
    sop: &str,
    blake3: &[u8; 32],
) {
    let mut attrs = clarus_core::stow::StudyManifestAttrs {
        patient_name: Some("ERASE^DIRECT".into()),
        patient_id: Some("ERASE".into()),
        study_instance_uid: study.to_string(),
        modality: "CT".into(),
        ..Default::default()
    };
    let instance = clarus_core::stow::InstanceModel {
        sop_instance_uid: sop.to_string(),
        blake3: *blake3,
        storage_tier: clarus_core::manifest::TIER_HOT,
        transfer_syntax_uid: "1.2.840.10008.1.2.1".to_string(),
        file_size: 64,
        ..Default::default()
    };
    let series_model = [clarus_core::stow::SeriesModel {
        series_instance_uid: series.to_string(),
        modality: "CT".into(),
        series_number: None,
        instances: vec![instance],
        ..Default::default()
    }];
    clarus_core::stow::write_study_manifest_full(&srv.manifests(), &mut attrs, &series_model)
        .expect("write direct manifest");
}

// -- Gate 3: worker idempotent / crash-replay ----------------------------

#[test]
fn worker_is_idempotent() {
    let srv = TestServer::start();
    let study = "1.2.3.40";
    stow_one(&srv, study, "1.2.3.40.1", "1.2.3.40.1.1");
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"replay"}}"#).as_bytes(),
    )
    .assert_status(200);

    // First pass erases and lifts.
    let s1 = erase_once(&srv.state);
    assert!(s1.lifted >= 1, "first pass lifts: {s1:?}");
    assert!(!srv.tombstone(study).exists());

    // Second pass: nothing to do, no crash, idempotent.
    let s2 = erase_once(&srv.state);
    assert_eq!(s2.tombstones, 0, "no tombstones left: {s2:?}");
    assert!(s2.errors == 0, "no errors on replay: {s2:?}");
}

// -- Gate 9: an undecodable tombstone is never lifted -------------------

#[test]
fn undecodable_tombstone_is_kept_and_counted_as_error() {
    let srv = TestServer::start();
    let study = "1.2.3.45";
    let series = "1.2.3.45.1";
    let sop = "1.2.3.45.1.1";
    let dcm = build_instance(sop, study, series, 0x45);
    srv.post_dicom("/dicomweb/studies", &dcm).assert_status(200);
    let cas_path = srv.state.cas.cas_path(&hex_of_blake3(&dcm));
    assert!(cas_path.is_file(), "blob stored");

    // Simulate a durable but corrupt `.del`: the tombstone exists, its manifest
    // bytes do not decode, so its file list is unknowable.
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"corrupt","dry_run":true}}"#).as_bytes(),
    )
    .assert_status(200);
    let del = srv.tombstone(study);
    // A dry run touches nothing, so move the manifest aside ourselves to model
    // a tombstone whose bytes are unreadable.
    std::fs::rename(srv.manifest(study), &del).expect("rename manifest to tombstone");
    assert!(del.is_file(), "tombstone written");
    std::fs::write(&del, b"not a manifest").expect("corrupt the tombstone");

    let stats = erase_once(&srv.state);
    assert!(
        stats.errors > 0,
        "undecodable tombstone must be an error: {stats:?}"
    );
    assert_eq!(stats.lifted, 0, "must not be lifted: {stats:?}");
    assert!(del.is_file(), "tombstone stays for the next pass");
    assert!(cas_path.is_file(), "blob kept - its record is unknown");
}

// -- Gate 1: visibility is instant (tombstone hides before files erased) -

#[test]
fn tombstone_hides_study_immediately() {
    let srv = TestServer::start();
    let study = "1.2.3.50";
    let series = "1.2.3.50.1";
    let sop = "1.2.3.50.1.1";
    let dcm = build_instance(sop, study, series, 0x50);
    srv.post_dicom("/dicomweb/studies", &dcm).assert_status(200);

    // Pause the worker so files persist during the visibility check.
    ERASE_PAUSE.store(true, Ordering::Relaxed);
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"vis"}}"#).as_bytes(),
    )
    .assert_status(200);
    // Tombstone present; the CAS file is still on disk (nothing erased yet).
    assert!(srv.tombstone(study).is_file(), "tombstone present");
    let hash = hex_of_blake3(&dcm);
    let cas_path = srv.state.cas.cas_path(&hash);
    assert!(cas_path.is_file(), "file not yet erased (worker paused)");

    // QIDO no longer lists the study (manifest is the source of truth, and the
    // `.manifest` file is gone - readers filter the suffix).
    assert!(
        !srv.manifest(study).exists(),
        "manifest hidden by tombstone"
    );

    // Release the worker; the file is erased and the tombstone lifted.
    ERASE_PAUSE.store(false, Ordering::Relaxed);
    drain_worker(&srv, Duration::from_secs(5));
    assert!(!srv.tombstone(study).exists(), "tombstone lifted");
    assert!(!cas_path.exists(), "file erased after worker release");
}

// -- Gate 4: resurrection-guard - STOW on tombstoned UID -> 409, then OK --

#[test]
fn stow_on_tombstoned_study_is_409_then_reingest() {
    let srv = TestServer::start();
    let study = "1.2.3.60";
    let series = "1.2.3.60.1";
    stow_one(&srv, study, series, "1.2.3.60.1.1");
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"resurrect"}}"#).as_bytes(),
    )
    .assert_status(200);

    // While tombstoned, a STOW with the same study UID must be refused 409.
    ERASE_PAUSE.store(true, Ordering::Relaxed);
    let dcm = build_instance("1.2.3.60.2.1", study, series, 0x60);
    let r = srv.post_dicom("/dicomweb/studies", &dcm);
    r.assert_status(409);
    assert!(
        r.json()
            .obj_get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .contains("deleted")
    );
    ERASE_PAUSE.store(false, Ordering::Relaxed);

    // After erasure the UID is free again.
    drain_worker(&srv, Duration::from_secs(5));
    assert!(!srv.tombstone(study).exists(), "tombstone lifted");
    stow_one(&srv, study, series, "1.2.3.60.3.1");
    assert!(
        srv.manifest(study).is_file(),
        "re-ingest recreates manifest"
    );
}

// -- Gate 6: open-handle retry - worker doesn't crash, gauge stays high --

#[test]
fn open_handle_retries_without_crashing() {
    let srv = TestServer::start();
    let study = "1.2.3.70";
    let series = "1.2.3.70.1";
    let sop = "1.2.3.70.1.1";
    let dcm = build_instance(sop, study, series, 0x70);
    srv.post_dicom("/dicomweb/studies", &dcm).assert_status(200);
    let hash = hex_of_blake3(&dcm);
    let cas_path = srv.state.cas.cas_path(&hash);
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"handle"}}"#).as_bytes(),
    )
    .assert_status(200);

    // Hold the CAS file open with a handle (open-handle semantics). On Windows
    // an open handle blocks unlink (retry); on Unix an open read handle does
    // not block unlink, so there we only assert the non-crash behaviour.
    let held = std::fs::File::open(&cas_path).expect("open cas file");

    // Pass 1: must not panic; the tombstone/file are kept (Windows) or the
    // unlink already succeeded (Unix).
    let s1 = erase_once(&srv.state);
    assert!(
        srv.tombstone(study).exists() || !cas_path.exists() || s1.errors == 0,
        "worker does not crash on unlink contention: {s1:?}"
    );

    // Release the handle, then drain: files erased, tombstone lifted, gauge 0.
    drop(held);
    drain_worker(&srv, Duration::from_secs(5));
    assert!(!cas_path.exists(), "file erased after handle released");
    assert!(!srv.tombstone(study).exists(), "tombstone lifted");
    assert_eq!(
        clarus_server::metrics::ERASE_PENDING.load(Ordering::Relaxed),
        0,
        "gauge back to 0"
    );
}

// -- instance/series SUB-delete (manifest-rewrite) --------

/// Return the set of SOPInstanceUIDs present in a study's live manifest.
fn manifest_sops(srv: &TestServer, study: &str) -> Vec<String> {
    let m = read_manifest(&srv.manifest(study)).expect("read manifest");
    let mut out = Vec::new();
    for s in &m.series {
        for i in &s.instances {
            for (t, v) in &i.attrs {
                if *t == 0x0008_0018
                    && let clarus_core::manifest::AttrValue::Text(u) = v
                {
                    out.push(u.clone());
                }
            }
        }
    }
    out
}

/// GET a single instance via WADO; returns the HTTP status.
fn wado_get(srv: &TestServer, study: &str, series: &str, sop: &str) -> u16 {
    let path = format!("/dicomweb/studies/{study}/series/{series}/instances/{sop}");
    srv.request("GET", &path, "", &[]).status
}

#[test]
fn subdelete_instance_rewrites_manifest_and_frees_orphan_blob() {
    let srv = TestServer::start();
    let study = "1.2.3.100";
    let series = "1.2.3.100.1";
    let sop1 = "1.2.3.100.1.1";
    let sop2 = "1.2.3.100.1.2";
    let dcm1 = build_instance(sop1, study, series, 0x31);
    let dcm2 = build_instance(sop2, study, series, 0x32);
    srv.post_dicom("/dicomweb/studies", &dcm1)
        .assert_status(200);
    srv.post_dicom("/dicomweb/studies", &dcm2)
        .assert_status(200);
    let hash1 = hex_of_blake3(&dcm1);
    let cas1 = srv.state.cas.cas_path(&hash1);
    assert!(cas1.is_file(), "sop1 blob present before delete");

    let r = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"sop_instance_uids":["{sop1}"],"reason":"sub-instance"}}"#).as_bytes(),
    );
    r.assert_status(200);
    let rj = r.json();
    assert_eq!(
        rj.obj_get("erase").and_then(|x| x.as_str()),
        Some("sync"),
        "sub-delete stays sync: {rj:?}"
    );
    assert_eq!(
        rj.obj_get("deleted_instances").and_then(|x| x.as_u64()),
        Some(1),
        "one instance deleted: {rj:?}"
    );
    assert_eq!(
        rj.obj_get("files_deleted").and_then(|x| x.as_u64()),
        Some(1),
        "orphan blob unlinked: {rj:?}"
    );

    // Study stays live (no tombstone); sop1 gone from manifest, sop2 remains.
    assert!(srv.manifest(study).is_file(), "study manifest stays live");
    let sops = manifest_sops(&srv, study);
    assert!(!sops.contains(&sop1.to_string()), "sop1 removed: {sops:?}");
    assert!(sops.contains(&sop2.to_string()), "sop2 kept: {sops:?}");
    assert!(!cas1.exists(), "sop1 CAS file freed");

    // WADO: sop1 now 404, sop2 still served.
    assert_eq!(wado_get(&srv, study, series, sop1), 404, "sop1 gone");
    assert_eq!(wado_get(&srv, study, series, sop2), 200, "sop2 served");

    // Evidence: the file audit row grew and targets the instance.
    let after = audit_log_content(&srv);
    assert!(
        after.contains("\"target\":\"1.2.3.100.1.1\""),
        "instance delete audited: {after}"
    );
    assert!(
        after.contains("\"patient_id\":\"ERASE\""),
        "SOP-level delete row carries the owning study's PatientID: {after}"
    );
}

#[test]
fn subdelete_series_removes_all_instances_keeps_other_series() {
    let srv = TestServer::start();
    let study = "1.2.3.110";
    let series_a = "1.2.3.110.1";
    let series_b = "1.2.3.110.2";
    let a1 = "1.2.3.110.1.1";
    let a2 = "1.2.3.110.1.2";
    let b1 = "1.2.3.110.2.1";
    srv.post_dicom(
        "/dicomweb/studies",
        &build_instance(a1, study, series_a, 0x41),
    )
    .assert_status(200);
    srv.post_dicom(
        "/dicomweb/studies",
        &build_instance(a2, study, series_a, 0x42),
    )
    .assert_status(200);
    srv.post_dicom(
        "/dicomweb/studies",
        &build_instance(b1, study, series_b, 0x43),
    )
    .assert_status(200);

    let r = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"series_uid":"{series_a}","reason":"sub-series"}}"#).as_bytes(),
    );
    r.assert_status(200);
    let rj = r.json();
    assert_eq!(
        rj.obj_get("erase").and_then(|x| x.as_str()),
        Some("sync"),
        "series sub-delete sync: {rj:?}"
    );
    assert_eq!(
        rj.obj_get("deleted_series").and_then(|x| x.as_u64()),
        Some(1),
        "one series deleted: {rj:?}"
    );
    assert_eq!(
        rj.obj_get("deleted_instances").and_then(|x| x.as_u64()),
        Some(2),
        "two instances in series: {rj:?}"
    );

    // Series A gone, series B intact (study still live).
    let sops = manifest_sops(&srv, study);
    assert!(!sops.contains(&a1.to_string()), "a1 removed: {sops:?}");
    assert!(!sops.contains(&a2.to_string()), "a2 removed: {sops:?}");
    assert!(sops.contains(&b1.to_string()), "b1 kept: {sops:?}");

    // WADO: A gone (404), B served.
    assert_eq!(wado_get(&srv, study, series_a, a1), 404, "a1 gone");
    assert_eq!(wado_get(&srv, study, series_b, b1), 200, "b1 served");

    // Evidence audited with the series as target.
    let after = audit_log_content(&srv);
    assert!(
        after.contains("\"target\":\"1.2.3.110.1\""),
        "series delete audited: {after}"
    );
}

// -- Helpers -------------------------------------------------------------

fn hex_of_blake3(bytes: &[u8]) -> String {
    clarus_adapters::hex_encode(blake3::hash(bytes).as_bytes())
}

/// Count every file under the hot CAS root (both levels of the 2-level
/// layout) - the acceptance-criterion "files_total" for the new tests.
fn cas_file_count(srv: &TestServer) -> usize {
    let mut n = 0;
    let mut dirs = vec![srv.state.cas.root_dir().to_path_buf()];
    while let Some(d) = dirs.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else {
                    n += 1;
                }
            }
        }
    }
    n
}

/// Decode a 64-hex blake3 render into its 32 raw bytes (no decoder in
/// `clarus_adapters`; the sidecar stores hex).
fn decode_hex32(hex: &str) -> [u8; 32] {
    let bytes = hex.as_bytes();
    assert_eq!(bytes.len(), 64, "64-hex blake3 render: {hex}");
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = (bytes[2 * i] as char).to_digit(16).expect("hex") as u8;
        let lo = (bytes[2 * i + 1] as char).to_digit(16).expect("hex") as u8;
        out[i] = (hi << 4) | lo;
    }
    out
}

/// Parse each JSON line with `"event":"<kind>"` and return the raw line.
fn json_events(log: &str, kind: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains(&format!("\"event\":\"{kind}\"")))
        .map(|l| l.to_string())
        .collect()
}

/// Extract the `batch_id` field from an audit JSON line.
fn extract_batch(line: &str) -> String {
    parse_json(line.as_bytes())
        .and_then(|v| {
            v.obj_get("batch_id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

/// 2808 delete-trail identity: a study WITHOUT a PatientID in its manifest
/// writes `patient_id":""` - the field is present but NEVER invented.
#[test]
fn delete_rows_empty_patient_id_defaults_to_empty() {
    let srv = TestServer::start();
    let study = "1.2.3.140";
    let series = "1.2.3.140.1";
    let sop = "1.2.3.140.1.1";

    // Plant a manifest with NO PatientID (and no name) - the honest "device
    // sent nothing / never coerced" shape.
    let mut attrs = clarus_core::stow::StudyManifestAttrs {
        patient_id: None,
        patient_name: None,
        study_instance_uid: study.to_string(),
        modality: "CT".into(),
        ..Default::default()
    };
    let blake3 = clarus_core::hash::cas_hash_one(b"empty-patient-study");
    let instance = clarus_core::stow::InstanceModel {
        sop_instance_uid: sop.to_string(),
        blake3,
        storage_tier: clarus_core::manifest::TIER_HOT,
        transfer_syntax_uid: "1.2.840.10008.1.2.1".to_string(),
        file_size: 64,
        ..Default::default()
    };
    let series_model = [clarus_core::stow::SeriesModel {
        series_instance_uid: series.to_string(),
        modality: "CT".into(),
        instances: vec![instance],
        ..Default::default()
    }];
    clarus_core::stow::write_study_manifest_full(&srv.manifests(), &mut attrs, &series_model)
        .expect("write manifest without PatientID");

    let r = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"empty-pid"}}"#).as_bytes(),
    );
    r.assert_status(200);
    let trail = audit_log_content(&srv);
    assert!(
        trail.contains("\"patient_id\":\"\""),
        "missing PatientID renders as empty string, never invented: {trail}"
    );
    // Date-prefix follows TODAY's UTC date (a hardcoded calendar date
    // breaks the suite at midnight).
    let today = clarus_server::access_log::utc_ts_iso();
    assert!(
        trail.contains(&format!("\"ts\":\"{}", &today[..11])),
        "delete row carries the ts field: {trail}"
    );
}

/// Scenario 15 (2808 update-in-place): update then erase - the study erase
/// removes the OLD and the NEW blob (both CAS files), and the update itself
/// added NOTHING to the delete trail (an update is not a delete; the old
/// blob's disappearance at erase time is recorded by the existing delete
/// flow).
#[test]
fn update_then_erase_reclaims_old_and_new_blobs() {
    let srv = TestServer::start();
    let study = "1.2.3.120";
    let series = "1.2.3.120.1";
    let sop = "1.2.3.120.1.1";

    let dcm_old = build_instance(sop, study, series, 0x42);
    srv.post_dicom("/dicomweb/studies", &dcm_old)
        .assert_status(200);
    let hash_old = hex_of_blake3(&dcm_old);
    let cas_old = srv.state.cas.cas_path(&hash_old);
    assert!(cas_old.is_file(), "old blob present after insert");

    // Pixel-only replacement (attrs unchanged -> no 0561, plain new bytes).
    let dcm_new = build_instance(sop, study, series, 0x43);
    let upd = srv.post_dicom("/dicomweb/studies", &dcm_new);
    // 202: the instance was replaced in place, which PS3.18 10.5.2 reports as
    // "stored with warnings" - the archive no longer holds what was sent.
    upd.assert_status(202);
    let hash_new = hex_of_blake3(&dcm_new);
    let cas_new = srv.state.cas.cas_path(&hash_new);
    assert!(cas_new.is_file(), "new blob present after update");
    // The manifest re-pointed; WADO serves the new bytes.
    assert_eq!(
        wado_get(&srv, study, series, sop),
        200,
        "updated instance stays WADO-addressable"
    );

    // The update is NOT a delete: the trail has no new lines after it.
    let trail_after_update = audit_log_content(&srv);
    assert!(
        !trail_after_update.contains("\"target\":\"1.2.3.120.1.1\""),
        "an update must not write delete-trail lines"
    );

    // Study delete -> erase worker drains -> BOTH blobs reclaimed.
    let del = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"update-erase"}}"#).as_bytes(),
    );
    del.assert_status(200);
    drain_worker(&srv, Duration::from_secs(10));
    assert!(!cas_old.exists(), "old blob reclaimed at erase");
    assert!(!cas_new.exists(), "new blob reclaimed at erase");

    // The erase is audited exactly once by the existing delete flow.
    let trail_after_erase = audit_log_content(&srv);
    assert!(
        trail_after_erase.contains("\"target\":\"1.2.3.120\""),
        "study erase is audited"
    );
}

// -- 3008: hidden-PHI alibi blobs are reclaimed when the study is erased ---

/// Build a hidden-PHI fixture: a coerced instance (empty patient fields)
/// whose OWN top-level (0400,0561) carries a non-empty PatientName.
fn build_hidden_phi_instance(sop: &str, study: &str, series: &str, pixel_byte: u8) -> Vec<u8> {
    let mut d = build_coerced_instance(sop, study, series, pixel_byte);
    let items = [clarus_server::validation::OriginalAttributeItem {
        group: 0x0010,
        element: 0x0010,
        old_value: "HIDDEN^NAME".to_string(),
        reason: "CORRECT",
    }];
    d.extend_from_slice(
        &clarus_server::validation::build_original_attributes_sequence(
            &items,
            "STOW-RS",
            "CLARUS_SERVER:8024",
        ),
    );
    d
}

/// Acceptance pair: a hidden-PHI insert keeps the original as the alibi
/// (recorded in `{study}.audited`) and serves the sanitized blob; a repeat
/// upload is Updated again with append-dedup (sidecar stays at one line);
/// a study erase reclaims ALL blobs (alibi + sanitized + update blobs) and
/// removes the sidecars.
#[test]
fn hidden_phi_alibi_erased_with_study() {
    let srv = TestServer::start();
    let study = "1.2.3.150";
    let series = "1.2.3.150.1";
    let sop = "1.2.3.150.1.1";
    let hidden = build_hidden_phi_instance(sop, study, series, 0x15);

    let r = srv.post_dicom("/dicomweb/studies", &hidden);
    // 202: the empty Type 2 fields were coerced (Warn Reason B000), which is
    // "stored with warnings".
    r.assert_status(202);
    assert!(
        String::from_utf8_lossy(&r.body).contains("45056"),
        "coerced insert carries the B000 warn reason"
    );
    // Original (alibi) + sanitized (served) blobs.
    assert_eq!(cas_file_count(&srv), 2, "alibi + sanitized");

    // 3008 linkage: the alibi is recorded in the per-study sidecar.
    let sidecar = srv.manifests().join(format!("{study}.audited"));
    let text = std::fs::read_to_string(&sidecar).expect("audited sidecar present");
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "exactly one alibi line: {lines:?}");
    let (hex, tier) = lines[0].split_once(' ').expect("hex tier");
    assert_eq!(hex.len(), 64, "blake3 hex render");
    assert_eq!(
        tier,
        &clarus_core::manifest::TIER_HOT.to_string(),
        "alibi is hot tier"
    );
    assert!(
        srv.state.cas.cas_path(hex).is_file(),
        "alibi exists at the sidecar address"
    );

    // Repeat upload -> Updated again (incoming != sanitized), sidecar still
    // one line (append dedup), the replaced sanitized blob is orphaned.
    srv.post_dicom("/dicomweb/studies", &hidden)
        .assert_status(202);
    let text2 = std::fs::read_to_string(&sidecar).expect("sidecar still there");
    assert_eq!(
        text2.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "append dedup: sidecar did not grow"
    );
    assert!(
        srv.manifests().join(format!("{study}.orphans")).is_file(),
        "update orphans the replaced sanitized blob"
    );

    // Study erase: ALL blobs reclaimed, sidecars removed with the tombstone.
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study}","reason":"alibi-erase"}}"#).as_bytes(),
    )
    .assert_status(200);
    drain_worker(&srv, Duration::from_secs(10));
    assert!(!srv.tombstone(study).exists(), "tombstone lifted");
    assert_eq!(
        cas_file_count(&srv),
        0,
        "original AND sanitized AND update blobs reclaimed"
    );
    assert!(!sidecar.exists(), "audit sidecar removed with the study");
}

/// Refcount safety: when a LIVE manifest of another study references the same
/// hash as a study's alibi, the erase of the first study must NOT unlink that
/// blob (the `referenced` guard shared with the orphan drain).
#[test]
fn hidden_phi_alibi_shared_with_live_manifest_survives_erase() {
    let srv = TestServer::start();
    let study_a = "1.2.3.151";
    let series_a = "1.2.3.151.1";
    let sop_a = "1.2.3.151.1.1";
    let hidden = build_hidden_phi_instance(sop_a, study_a, series_a, 0x15);
    srv.post_dicom("/dicomweb/studies", &hidden)
        .assert_status(202);

    let sidecar = srv.manifests().join(format!("{study_a}.audited"));
    let text = std::fs::read_to_string(&sidecar).expect("sidecar");
    let hex = text
        .lines()
        .next()
        .expect("line")
        .trim()
        .split_once(' ')
        .expect("hex tier")
        .0;
    let alibi_blake3 = decode_hex32(hex);
    let alibi_path = srv.state.cas.cas_path(hex);
    assert!(alibi_path.is_file(), "alibi present before erase");

    // A LIVE manifest of study B references the SAME hash (cross-study dedup).
    let study_b = "1.2.3.152";
    write_manifest_direct(&srv, study_b, "1.2.3.152.1", "1.2.3.152.1.1", &alibi_blake3);
    assert!(srv.manifest(study_b).is_file(), "B manifest present");

    // Erase A: A's sanitized blob is unlinked, the shared alibi SURVIVES.
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study_a}","reason":"shared-alibi-A"}}"#).as_bytes(),
    )
    .assert_status(200);
    drain_worker(&srv, Duration::from_secs(10));
    assert!(!srv.tombstone(study_a).exists(), "A lifted after erase");
    assert!(
        alibi_path.is_file(),
        "alibi retained while study B references it"
    );
    assert_eq!(cas_file_count(&srv), 1, "only the shared alibi remains");

    // Erase B: now unreferenced, the alibi is unlinked.
    srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"study_uid":"{study_b}","reason":"shared-alibi-B"}}"#).as_bytes(),
    )
    .assert_status(200);
    drain_worker(&srv, Duration::from_secs(10));
    assert!(
        !alibi_path.exists(),
        "alibi unlinked once no live manifest references it"
    );
    assert_eq!(cas_file_count(&srv), 0, "CAS empty after both erases");
}

// -- Gate 9: TOCTOU - delete re-reads under the
// lease. A STOW commit landing during the delete's audit-append window must
// survive the rewrite (the pre-fix code wrote a stale pre-lease copy and
// silently lost those instances).

#[test]
fn toctou_delete_stow_race_keeps_all_instances() {
    let srv = TestServer::start();
    let study = "1.2.3.130";
    let series = "1.2.3.130.1";
    // The victim is NOT the study's only instance: a second instance stays
    // behind, so the delete cannot empty the study and the tombstone race
    // that emptying introduces is not what this test is about.
    //
    // Since W24 an emptied study IS tombstoned by the instance delete, and a
    // STOW that arrives after that tombstone is refused with 409 by the
    // resurrection guard. That is correct behaviour, but it would make this
    // gate measure the guard instead of the lease: the property here is that
    // a delete re-reads the manifest UNDER the lease, so concurrent commits
    // are not lost by a stale rewrite.
    let victim = "1.2.3.130.1.0";
    let keeper = "1.2.3.130.1.99";
    stow_one(&srv, study, series, victim);
    stow_one(&srv, study, series, keeper);

    let n = 30;
    std::thread::scope(|scope| {
        let bg = scope.spawn(|| {
            // Continuous STOW commits into the SAME study while the main
            // thread deletes the victim - the commits race the delete's
            // audit-append (fsync) window before the lease is taken.
            for i in 1..=n {
                stow_one(&srv, study, series, &format!("1.2.3.130.1.{i}"));
            }
        });
        // Give the background thread a head start so its commits land
        // inside the delete window.
        std::thread::sleep(Duration::from_millis(5));
        let del = srv.post_json(
            "/admin/v1/instances/delete",
            format!(r#"{{"sop_instance_uids":["{victim}"],"reason":"toctou race"}}"#).as_bytes(),
        );
        del.assert_status(200);
        bg.join().expect("background STOW thread");
    });

    // The victim is gone, EVERY concurrent STOW instance survived.
    let m = read_manifest(&srv.manifest(study)).expect("manifest readable");
    let sops: Vec<&str> = m
        .series
        .iter()
        .flat_map(|s| s.instances.iter())
        .filter_map(|i| {
            i.attrs.iter().find_map(|(t, v)| match (t, v) {
                (0x0008_0018, clarus_core::manifest::AttrValue::Text(u)) => Some(u.as_str()),
                _ => None,
            })
        })
        .collect();
    assert!(!sops.contains(&victim), "victim deleted");
    assert!(sops.contains(&keeper), "the untouched instance survived");
    assert_eq!(
        sops.len(),
        n + 1,
        "all {n} concurrent STOW instances plus the keeper survived the delete (TOCTOU)"
    );
}

/// S44: the instance scope honours `dry_run`.
///
/// The contract stated on `DeleteResponse::erase` is that `"preview"` means
/// nothing was touched, and only the study scope implemented it. The instance
/// and series scopes parsed the flag and deleted anyway, so an operator
/// rehearsing a sub-study delete performed it.
#[test]
fn a_dry_run_instance_delete_touches_nothing() {
    let srv = TestServer::start();
    let study = "1.2.3.210";
    let series = "1.2.3.210.1";
    let sop = "1.2.3.210.1.1";
    let dcm = build_instance(sop, study, series, 0x51);
    srv.post_dicom("/dicomweb/studies", &dcm).assert_status(200);

    let cas_path = srv.state.cas.cas_path(&hex_of_blake3(&dcm));
    assert!(cas_path.is_file(), "the blob is stored before the preview");

    let dry = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"sop_instance_uids":["{sop}"],"reason":"rehearsal","dry_run":true}}"#)
            .as_bytes(),
    );
    dry.assert_status(200);
    let dj = dry.json();
    assert_eq!(
        dj.obj_get("erase").and_then(|x| x.as_str()),
        Some("preview"),
        "an instance dry run must report erase=preview"
    );
    assert_eq!(
        dj.obj_get("deleted_instances").and_then(|x| x.as_u64()),
        Some(1),
        "the preview must count what WOULD go"
    );
    assert!(
        dj.obj_get("bytes_to_free")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
            > 0,
        "the preview must size what WOULD be freed"
    );

    // Nothing was touched: the instance is still in the manifest and the blob
    // is still on disk.
    let m = read_manifest(&srv.manifest(study)).expect("manifest readable");
    let present = m.series.iter().flat_map(|s| s.instances.iter()).any(|i| {
        i.attrs.iter().any(|(t, v)| {
            *t == 0x0008_0018 && matches!(v, clarus_core::manifest::AttrValue::Text(u) if u == sop)
        })
    });
    assert!(present, "a preview must not drop the instance");
    assert!(cas_path.is_file(), "a preview must not unlink the blob");
}

/// S44: the series scope honours `dry_run` too.
#[test]
fn a_dry_run_series_delete_touches_nothing() {
    let srv = TestServer::start();
    let study = "1.2.3.211";
    let series = "1.2.3.211.1";
    let a1 = "1.2.3.211.1.1";
    let a2 = "1.2.3.211.1.2";
    srv.post_dicom(
        "/dicomweb/studies",
        &build_instance(a1, study, series, 0x61),
    )
    .assert_status(200);
    srv.post_dicom(
        "/dicomweb/studies",
        &build_instance(a2, study, series, 0x62),
    )
    .assert_status(200);

    let dry = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"series_uid":"{series}","reason":"rehearsal","dry_run":true}}"#).as_bytes(),
    );
    dry.assert_status(200);
    let dj = dry.json();
    assert_eq!(
        dj.obj_get("erase").and_then(|x| x.as_str()),
        Some("preview"),
        "a series dry run must report erase=preview"
    );
    assert_eq!(
        dj.obj_get("deleted_instances").and_then(|x| x.as_u64()),
        Some(2),
        "the preview must count every instance of the series"
    );

    let m = read_manifest(&srv.manifest(study)).expect("manifest readable");
    let count = m
        .series
        .iter()
        .flat_map(|s| s.instances.iter())
        .filter(|i| {
            i.attrs.iter().any(|(t, v)| {
                *t == 0x0008_0018
                    && matches!(v, clarus_core::manifest::AttrValue::Text(u) if u == a1 || u == a2)
            })
        })
        .count();
    assert_eq!(count, 2, "a preview must not remove the series instances");
}

/// The mirror case: without `dry_run` the delete really happens, so the tests
/// above cannot pass on a server that simply never deletes.
#[test]
fn a_real_instance_delete_still_removes_the_instance() {
    let srv = TestServer::start();
    let study = "1.2.3.212";
    let series = "1.2.3.212.1";
    let sop = "1.2.3.212.1.1";
    srv.post_dicom(
        "/dicomweb/studies",
        &build_instance(sop, study, series, 0x71),
    )
    .assert_status(200);

    let real = srv.post_json(
        "/admin/v1/instances/delete",
        format!(r#"{{"sop_instance_uids":["{sop}"],"reason":"for real"}}"#).as_bytes(),
    );
    real.assert_status(200);
    assert_eq!(
        real.json().obj_get("erase").and_then(|x| x.as_str()),
        Some("sync"),
        "a real sub-study delete reports erase=sync"
    );

    // The only instance is gone, so its study is tombstoned: the manifest is
    // removed and a tombstone takes its place. That is the delete really
    // happening - the mirror of the preview above.
    assert!(
        !srv.manifest(study).is_file(),
        "a real delete of the last instance removes the manifest"
    );
    assert!(
        srv.tombstone(study).exists(),
        "a real delete of the last instance tombstones the study"
    );
}
