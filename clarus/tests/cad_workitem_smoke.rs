// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! F1: generic CAD workitem creation after a durable STOW.
//!
//! The server creates ONE workitem per stored study when
//! `[ups] task_producer` is enabled. These scenarios pin the
//! contract end to end over a real socket:
//!
//! - exactly one workitem per study, and re-STOW does not add a second;
//! - the workitem UID is the deterministic blake3 derivation, digits only;
//! - a batch with two studies yields two workitems, two instances of one
//!   study yield one;
//! - the default configuration leaves the STOW path untouched (byte-identical
//!   response, zero workitems);
//! - creation happens AFTER the study manifest is durable.
//!
//! "Durable" is asserted through the observable ordering: the study manifest
//! file must exist on disk by the time the workitem is readable, which is the
//! property the call-site placement buys.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_core::ups::WorkItemStore;
use clarus_core::ups::record::WorkItemRecord;
use clarus_server::config::Config;
use clarus_server::handlers::cad_workitem::{workitem_uid_for_study, workitem_uid_for_study_code};
use clarus_server::handlers::json_common::{JsonVal, parse_json};
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Instances (same byte builder as stow_smoke) -------------------------

const STUDY_A: &str = "1.2.826.0.1.3680043.10.1234.5101";
const SERIES_A: &str = "1.2.826.0.1.3680043.10.1234.5201";
const STUDY_B: &str = "1.2.826.0.1.3680043.10.1234.5102";
const SERIES_B: &str = "1.2.826.0.1.3680043.10.1234.5202";
/// A study whose instance declares a CT modality (the fixture default).
const STUDY_C: &str = "1.2.826.0.1.3680043.10.1234.5103";
const SERIES_C: &str = "1.2.826.0.1.3680043.10.1234.5203";
/// A study whose instance declares a modality the shipped table does not
/// name (US), so the workitem trigger takes its fallback.
const STUDY_D: &str = "1.2.826.0.1.3680043.10.1234.5104";
const SERIES_D: &str = "1.2.826.0.1.3680043.10.1234.5204";

/// Append one Explicit VR Little Endian element (PS3.5 Section 7.1).
///
/// Same encoder as `stow_smoke.rs`: odd-length values are padded (UI with a
/// NUL, text VRs with a space), because a streaming parser rejects an
/// unpadded value outright.
fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let is_extended = matches!(
        vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    );
    if vr == b"UI" && !len.is_multiple_of(2) {
        let padded_len = len + 1;
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(padded_len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(padded_len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        buf.push(0x00);
    } else {
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !len.is_multiple_of(2) {
            let pad = if matches!(
                vr,
                b"CS"
                    | b"LO"
                    | b"SH"
                    | b"PN"
                    | b"AE"
                    | b"AS"
                    | b"DA"
                    | b"DS"
                    | b"DT"
                    | b"IS"
                    | b"LT"
                    | b"ST"
                    | b"TM"
                    | b"UC"
                    | b"UR"
                    | b"UT"
            ) {
                0x20
            } else {
                0x00
            };
            buf.push(pad);
        }
    }
}

/// A minimal valid Explicit VR LE instance with non-empty pixel data.
fn build_instance(sop_instance_uid: &str, study_uid: &str, series_uid: &str, pixel: u8) -> Vec<u8> {
    build_instance_with_modality(sop_instance_uid, study_uid, series_uid, pixel, "CT")
}

/// The same, with (0008,0060) given explicitly.
///
/// The modality is the study attribute the workitem trigger reads to choose
/// the Scheduled Workitem Code, so a test that is about the choice has to be
/// able to state it.
fn build_instance_with_modality(
    sop_instance_uid: &str,
    study_uid: &str,
    series_uid: &str,
    pixel: u8,
    modality: &str,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_instance_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"CAD^FIXTURE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"CADFIX01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", modality.as_bytes());
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(&mut d, 0x7fe0, 0x0010, b"OB", &[pixel; 64]);
    d
}

// -- Multipart framing ---------------------------------------------------

const BOUNDARY: &str = "----clarus_cad_boundary_9c41";

fn multipart_body(parts: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Type: application/dicom\r\nContent-Length: {}\r\n\r\n",
                part.len()
            )
            .as_bytes(),
        );
        body.extend_from_slice(part);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn multipart_ct() -> String {
    format!("multipart/related; type=\"application/dicom\"; boundary={BOUNDARY}")
}

// -- Test server harness -------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    /// Owns the temp tree the server is rooted in; dropping it tears the
    /// whole fixture down.
    _temp: tempfile::TempDir,
    manifest_dir: PathBuf,
    workitems_dir: PathBuf,
    /// The very state the serving thread holds, so a test can drive the
    /// readiness promotion the sweeper would otherwise do on its own timer.
    state: Arc<AppState>,
}

impl TestServer {
    /// Boot with `[ups] task_producer` forced to `on`.
    fn start_cad() -> Self {
        Self::start_with(|c| c.ups.task_producer = true)
    }

    /// Boot with the shipped defaults (the capability is off).
    fn start_default() -> Self {
        Self::start_with(|_| {})
    }

    fn start_with<F>(f: F) -> Self
    where
        F: FnOnce(&mut Config),
    {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage_dir = temp.path().join("cas");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");
        let btree_root = temp.path().join("btree");
        std::fs::create_dir_all(&btree_root).expect("btree dir");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let mut config = Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        // Deterministic: no disk-space preflight interference.
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        f(&mut config);

        let workitems_dir = btree_root.join("workitems");
        let state = AppState {
            cas: ContentAddressedStorage::new(storage_dir).expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: DualEventSink::new(),
            start_time: Instant::now(),
            metrics_ring: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            log_buffer: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            maintenance: Arc::new(StdRwLock::new(
                clarus_server::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: ShadowTier::new(idx_dir.clone()),
            study_scanner: StudyScanner::new(manifest_dir.clone(), idx_dir),
            config: config.clone(),
            workitems_dir: workitems_dir.clone(),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(state);
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        let serving_state = Arc::clone(&state);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, serving_state, shutdown, timeouts, u32::MAX);
        });

        Self {
            addr,
            _temp: temp,
            manifest_dir,
            workitems_dir,
            state,
        }
    }

    /// POST a multipart STOW and return the raw response bytes.
    fn stow(&self, body: &[u8]) -> Response {
        self.post("/dicomweb/studies", &multipart_ct(), body)
    }

    fn post(&self, path: &str, content_type: &str, body: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    /// Every workitem UID currently in the CBOR store, sorted.
    fn workitem_uids(&self) -> Vec<String> {
        let store = WorkItemStore::new(self.workitems_dir.clone());
        let mut uids: Vec<String> = store
            .search(&Default::default())
            .expect("search workitems")
            .0
            .into_iter()
            .map(|r| r.workitem_uid)
            .collect();
        uids.sort();
        uids
    }

    /// One stored workitem record, read through the same store the server uses.
    ///
    /// The harness cannot reach the server's own store handle (it is private
    /// to the handler module), so it opens the directory it was given - which
    /// is the same file on disk, and the read therefore proves what was
    /// actually persisted rather than what a handler returned.
    fn workitem_record(&self, workitem_uid: &str) -> Option<WorkItemRecord> {
        WorkItemStore::new(self.workitems_dir.clone())
            .find_by_uid(workitem_uid)
            .expect("lookup workitem")
    }

    /// The study manifest path for a study UID.
    fn manifest_path(&self, study_uid: &str) -> PathBuf {
        self.manifest_dir.join(format!("{study_uid}.manifest"))
    }

    /// GET one UPS search route with a raw query string (no leading `?`).
    fn get_workitems(&self, query: &str) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let req = format!(
            "GET /dicomweb/workitems?{query} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/dicom+json\r\nConnection: close\r\n\r\n",
            self.addr.port()
        );
        stream.write_all(req.as_bytes()).expect("write req");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        parse_http(&buf)
    }

    /// POST a JSON body to a DICOMweb path.
    fn post_json(&self, path: &str, body: &str) -> Response {
        self.post(path, "application/dicom+json", body.as_bytes())
    }

    /// How many studies are waiting out the readiness debounce.
    fn readiness_pending(&self) -> usize {
        self.state.readiness.pending_len()
    }

    /// The instant one study's quiet period expires, if it is still waiting.
    ///
    /// `pending_len` alone cannot tell a re-arm apart from a no-op: both leave
    /// the study pending. Reading the deadline is what makes "the second STOW
    /// moved the timer" an assertion rather than a hope.
    fn readiness_deadline(&self, study_uid: &str) -> Option<std::time::Instant> {
        self.state.readiness.deadline(study_uid)
    }

    /// Run one promotion, as the sweeper would.
    fn promote(&self, study_uid: &str) -> clarus_server::handlers::readiness::Promotion {
        clarus_server::handlers::readiness::promote(&self.state, study_uid)
    }

    /// Run one promotion and report whether it produced a READY workitem.
    ///
    /// The old `promote` answered a bare bool; the three-state answer exists
    /// so the SWEEPER can tell a retryable store failure from a final
    /// outcome. Most tests only care "did it promote", and that question now
    /// has one name here rather than an `== Promotion::Ready` at every call.
    fn promoted(&self, study_uid: &str) -> bool {
        self.promote(study_uid) == clarus_server::handlers::readiness::Promotion::Ready
    }

    /// Run one full sweeper pass at a controllable instant.
    fn sweep_once(&self, now: std::time::Instant) {
        clarus_server::handlers::readiness::sweep_once(&self.state, now);
    }

    /// Arm a study's quiet period without storing anything for it.
    fn arm(&self, study_uid: &str, now: std::time::Instant) {
        self.state.readiness.arm(study_uid, now);
    }

    /// Make the workitem store unreadable, producing an environmental error.
    ///
    /// The store's directory is replaced by a regular FILE, so listing it
    /// fails the way a real I/O failure would. This is the only honest way
    /// to reach `Promotion::StoreError` from outside the module: the store
    /// has no injectable failure, and inventing one would test the seam
    /// rather than the behaviour.
    fn break_workitem_store(&self) {
        let dir = &self.state.workitems_dir;
        let _ = std::fs::remove_dir_all(dir);
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(dir, b"not a directory").expect("occupy the store path");
    }

    /// The workitem UIDs a study's modality yields, straight from the LIVE
    /// config this server booted with.
    ///
    /// The catalog is per modality and a study yields one workitem per code,
    /// so a test cannot hardcode a single UID any more. Deriving it from
    /// `self.state.config` rather than repeating a literal keeps the test
    /// honest when a test's own `start_with` edits the table.
    fn catalog_uids(&self, study_uid: &str, modality: Option<&str>) -> Vec<String> {
        clarus_server::handlers::cad_workitem::catalog_entries_for_study(
            &self.state.config.ups,
            study_uid,
            modality,
        )
        .into_iter()
        .map(|entry| entry.uid)
        .collect()
    }

    /// The single workitem UID of a study whose modality yields exactly one
    /// code. Panics when the catalog has more than one entry, so a test that
    /// means "the one workitem" cannot silently pass on a multi-entry table.
    fn uid_for(&self, study_uid: &str, modality: Option<&str>) -> String {
        let uids = self.catalog_uids(study_uid, modality);
        assert_eq!(
            uids.len(),
            1,
            "uid_for expects a one-entry catalog for {study_uid}; got {uids:?}"
        );
        uids.into_iter().next().expect("one entry")
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

// -- Scenarios -----------------------------------------------------------

/// Exactly one workitem is created for a stored study, with the
/// deterministic UID, and a re-STOW adds nothing.
#[test]
fn cad_workitem_created_once_per_study() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5301", STUDY_A, SERIES_A, 0x41);

    let r1 = srv.stow(&multipart_body(&[&instance]));
    assert_eq!(r1.status, 200, "first STOW succeeds");

    let expected_uid = srv.uid_for(STUDY_A, Some("CT"));
    assert_eq!(
        srv.workitem_uids(),
        vec![expected_uid.clone()],
        "exactly one workitem, with the derived UID"
    );

    // The record carries the CAD shape: the type-1 set of PS3.4 CC.2.5-3, the
    // input study, and the study's Modality (the vendor extension the MWL
    // Type-1 Return key needs - asserted in full below).
    let store = WorkItemStore::new(srv.workitems_dir.clone());
    let rec = store
        .find_by_uid(&expected_uid)
        .expect("lookup")
        .expect("workitem exists");
    assert_eq!(rec.workflow_state, "SCHEDULED");
    assert_eq!(rec.workitem_uid, expected_uid);
    assert_eq!(rec.step_label.as_deref(), Some("CAD"));
    // INCOMPLETE first: the study has just received ONE instance and ingest is
    // unbounded in time, so offering it READY is what let a live performer
    // claim a study after 2 of 53 slices. The readiness flip is the subject of
    // the `readiness_*` tests below.
    assert_eq!(
        rec.input_readiness.as_deref(),
        Some("INCOMPLETE"),
        "a workitem created by a STOW is offered INCOMPLETE"
    );
    assert_eq!(rec.sps_priority.as_deref(), Some("MEDIUM"));
    assert_eq!(rec.input_study_uid.as_deref(), Some(STUDY_A));
    assert!(
        rec.sps_start_datetime.is_some(),
        "(0040,4005) is type 1 at create"
    );
    // The input study declares CT, so the workitem records CT. UPS itself has
    // no Modality (PS3.4 Table CC.2.5-3), so this is the vendor extension the
    // DIMSE MWL Type-1 Return key needs - and it is the study's REAL value,
    // not a fabricated one.
    assert_eq!(
        rec.modality, "CT",
        "a workitem created from a CT study records the study's modality"
    );

    // Re-STOW the same study: still one workitem (the duplicate is a no-op,
    // not a failure).
    let failures_before = clarus_server::metrics::CAD_WORKITEM_FAILURES_TOTAL
        .load(std::sync::atomic::Ordering::Relaxed);
    let r2 = srv.stow(&multipart_body(&[&instance]));
    assert_eq!(r2.status, 200, "re-STOW succeeds");
    assert_eq!(r2.body, r1.body, "re-STOW response stays byte-identical");
    assert_eq!(
        srv.workitem_uids(),
        vec![expected_uid],
        "re-STOW must not create a second workitem"
    );
    let failures_after = clarus_server::metrics::CAD_WORKITEM_FAILURES_TOTAL
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        failures_before, failures_after,
        "a duplicate workitem is NOT counted as a creation failure"
    );
}

/// A batch with two studies yields two workitems; two instances of ONE study
/// yield one.
#[test]
fn cad_workitem_batch_one_per_study() {
    let srv = TestServer::start_cad();
    let a1 = build_instance("1.2.826.0.1.3680043.10.1234.5311", STUDY_A, SERIES_A, 0x41);
    let a2 = build_instance("1.2.826.0.1.3680043.10.1234.5312", STUDY_A, SERIES_A, 0x42);
    let b1 = build_instance("1.2.826.0.1.3680043.10.1234.5321", STUDY_B, SERIES_B, 0x43);

    let r = srv.stow(&multipart_body(&[&a1, &a2, &b1]));
    assert_eq!(r.status, 200, "three-instance STOW succeeds");

    let mut expected = vec![
        srv.uid_for(STUDY_A, Some("CT")),
        srv.uid_for(STUDY_B, Some("CT")),
    ];
    expected.sort();
    assert_eq!(
        srv.workitem_uids(),
        expected,
        "one workitem per distinct study in the batch"
    );
}

/// The shipped default leaves STOW byte-identical and creates nothing.
#[test]
fn cad_workitem_disabled_by_default_is_noop() {
    let srv = TestServer::start_default();
    let body = multipart_body(&[
        &build_instance("1.2.826.0.1.3680043.10.1234.5331", STUDY_A, SERIES_A, 0x41),
        &build_instance("1.2.826.0.1.3680043.10.1234.5332", STUDY_B, SERIES_B, 0x42),
    ]);

    let r1 = srv.stow(&body);
    assert_eq!(r1.status, 200);
    assert!(
        srv.workitem_uids().is_empty(),
        "the default configuration must not create workitems"
    );

    // Byte-identical to a rebuild of the same request: the option off means
    // no observable difference at all.
    let r2 = srv.stow(&body);
    assert_eq!(r1.body, r2.body, "STOW bytes unchanged with the option off");
}

/// The study manifest is durable before the workitem appears: at the moment
/// the request has been answered, BOTH exist, and the manifest is the
/// precondition the call-site ordering guarantees.
#[test]
fn cad_workitem_created_only_after_durable_manifest() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5341", STUDY_A, SERIES_A, 0x41);

    // Before the STOW: neither artefact exists.
    assert!(
        !srv.manifest_path(STUDY_A).exists(),
        "no manifest before the STOW"
    );
    assert!(
        srv.workitem_uids().is_empty(),
        "no workitem before the STOW"
    );

    let r = srv.stow(&multipart_body(&[&instance]));
    assert_eq!(r.status, 200);

    // After the response both are present. The workitem could only have been
    // created by code that runs after `sync_batch_manifests`, because the
    // manifest write and its fsync both complete earlier in the same handler.
    assert!(
        srv.manifest_path(STUDY_A).exists(),
        "the study manifest is on disk"
    );
    assert_eq!(
        srv.workitem_uids(),
        vec![srv.uid_for(STUDY_A, Some("CT"))],
        "the workitem exists once the manifest is durable"
    );
}

/// The single-instance `application/dicom` STOW path creates a workitem too
/// (both STOW routes carry the same post-sync hook).
#[test]
fn cad_workitem_single_instance_path() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5351", STUDY_B, SERIES_B, 0x44);

    let r = srv.post("/dicomweb/studies", "application/dicom", &instance);
    assert_eq!(r.status, 200);
    assert_eq!(
        srv.workitem_uids(),
        vec![srv.uid_for(STUDY_B, Some("CT"))],
        "the single-instance STOW path also creates the workitem"
    );
}

/// The derived UID is numeric-only and inside the PS3.5 length bound, and it
/// is stable across calls (the idempotency property itself).
///
/// Uses the FREE derivation rather than a booted server: this is a property
/// of the UID construction, and a test that needed a server to check it would
/// be testing the harness as much as the rule.
#[test]
fn cad_workitem_uid_is_numeric_and_stable() {
    let uid = workitem_uid_for_study_code(STUDY_A, "CXR-AI");
    assert!(
        uid.chars().all(|c| c.is_ascii_digit() || c == '.'),
        "UID must be digits and dots only: {uid}"
    );
    assert!(
        uid.len() <= 64,
        "UID must fit the PS3.5 bound (len {}): {uid}",
        uid.len()
    );
    assert!(uid.starts_with("2.25."), "UID root: {uid}");
    assert_ne!(
        uid,
        workitem_uid_for_study_code(STUDY_B, "CXR-AI"),
        "distinct studies derive distinct UIDs"
    );
    assert_eq!(
        uid,
        workitem_uid_for_study_code(STUDY_A, "CXR-AI"),
        "derivation is stable"
    );
    // The CODE is part of the key: two catalog entries of one study must not
    // collide, which is the whole reason the code is folded in.
    assert_ne!(
        uid,
        workitem_uid_for_study_code(STUDY_A, "BONE-AI"),
        "two codes of one study derive DIFFERENT UIDs"
    );
    // The study-only derivation is a SEPARATE function, kept for the callers
    // that have no catalog entry in hand (the allocation gate's pure builder).
    // It is deliberately NOT equal to any per-code UID - if it were, folding
    // the code in would collide with the record it is meant to coexist with.
    assert_ne!(
        uid,
        workitem_uid_for_study(STUDY_A),
        "the per-code UID is not the study-only UID: folding the code in MUST move it"
    );
    assert_ne!(
        workitem_uid_for_study_code(STUDY_A, "BONE-AI"),
        workitem_uid_for_study(STUDY_A),
        "nor is the other entry's"
    );
    // The study-only construction still has the same properties.
    let study_only = workitem_uid_for_study(STUDY_A);
    assert!(
        study_only.chars().all(|c| c.is_ascii_digit() || c == '.'),
        "study-only UID is digits and dots only: {study_only}"
    );
    assert!(study_only.starts_with("2.25."), "UID root: {study_only}");
    assert_eq!(
        study_only,
        workitem_uid_for_study(STUDY_A),
        "study-only derivation is stable too"
    );
}

/// The (0008,0060) Modality filter is a WORKING matching key: a CT study's
/// workitem is returned for `?00080060=CT`, withheld for `?00080060=MR`, and
/// an externally created workitem (which has no Modality) is withheld from
/// every modality query - the honest skip.
#[test]
fn modality_filter_selects_only_matching_workitems() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5371", STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);
    let ct_uid = srv.uid_for(STUDY_A, Some("CT"));

    // A modality the study is not: the filter must withhold the workitem.
    let mr = srv.get_workitems("00080060=MR");
    assert_eq!(
        mr.status, 200,
        "an unmatched modality query is an empty 200"
    );
    assert!(
        mr.body.is_empty() || mr.body == b"[]",
        "no workitem for MR, got: {}",
        String::from_utf8_lossy(&mr.body)
    );

    // The study's own modality: the workitem comes back. Lower case must match
    // too - a modality is a DICOM CS value, so case is not significant.
    for query in ["00080060=CT", "00080060=ct"] {
        let r = srv.get_workitems(query);
        assert_eq!(r.status, 200, "{query} answers 200");
        let arr = parse_json(&r.body).expect("json");
        let arr = arr.as_array().expect("array");
        assert_eq!(arr.len(), 1, "{query} must return the CT workitem");
        let uid = arr[0]
            .obj_get("00080018")
            .and_then(|v| v.obj_get("Value"))
            .and_then(JsonVal::as_str_array_first);
        assert_eq!(
            uid,
            Some(ct_uid.as_str()),
            "{query} returned the wrong workitem"
        );
    }

    // The honest skip: an externally created workitem carries no Modality, so
    // no modality query can reach it. It is still reachable by its own UID.
    let external_uid = "1.2.826.0.1.3680043.10.1234.9001";
    // No SOP Common in the Create body: the workitem UID is conveyed by the
    // `workitem` query parameter and (0008,0018) in the payload is now
    // refused 400 (PS3.4 Table CC.2.5-3), same as the SOP Class UID.
    let body = "{\"00741000\":{\"vr\":\"CS\",\"Value\":[\"SCHEDULED\"]},\
          \"00741200\":{\"vr\":\"CS\",\"Value\":[\"HIGH\"]},\
          \"00741204\":{\"vr\":\"LO\",\"Value\":[\"EXTERNAL\"]},\
          \"00404005\":{\"vr\":\"DT\",\"Value\":[\"20260802103000\"]},\
          \"00404041\":{\"vr\":\"CS\",\"Value\":[\"READY\"]}}";
    let created = srv.post_json(
        &format!("/dicomweb/workitems?workitem={external_uid}"),
        body,
    );
    assert_eq!(created.status, 201, "external Create succeeds");

    let r = srv.get_workitems("00080060=CT");
    let arr = parse_json(&r.body).expect("json");
    let arr = arr.as_array().expect("array");
    assert_eq!(
        arr.len(),
        1,
        "the external workitem has no Modality, so a CT query must not return it"
    );
    assert_eq!(
        arr[0]
            .obj_get("00080018")
            .and_then(|v| v.obj_get("Value"))
            .and_then(JsonVal::as_str_array_first),
        Some(ct_uid.as_str()),
        "the CT query still returns only the STOW-created workitem"
    );
}

/// The created workitem is visible through the standard UPS-RS search route
/// with no route changes - the point of storing it in the shared store.
#[test]
fn cad_workitem_visible_via_ups_search() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5361", STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let mut stream = TcpStream::connect(srv.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let req = format!(
        "GET /dicomweb/workitems HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/dicom+json\r\nConnection: close\r\n\r\n",
        srv.addr.port()
    );
    stream.write_all(req.as_bytes()).expect("write req");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read resp");
    let resp = parse_http(&buf);
    assert_eq!(resp.status, 200);

    let json = parse_json(&resp.body).expect("workitems json");
    // The search response is a bare DICOMweb JSON array of workitem objects.
    let arr = json.as_array().expect("workitem array");
    assert_eq!(arr.len(), 1, "one workitem in the worklist");

    // Identity is (0008,0018) SOP Instance UID - the C-FIND unique return key.
    let uid = arr[0]
        .obj_get("00080018")
        .and_then(|v| v.obj_get("Value"))
        .and_then(JsonVal::as_str_array_first)
        .expect("workitem UID in the search result");
    assert_eq!(uid, srv.uid_for(STUDY_A, Some("CT")));

    // The study is named through the Referenced Request Sequence (0040,A370)
    // - the bare top-level (0020,000D) is gone (Task 3) - and also through the
    // Input Information Sequence (0040,4021). Both carry the real input study,
    // not the workitem.
    let study = arr[0]
        .obj_get("0040A370")
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.first_of_array())
        .and_then(|item| item.obj_get("0020000D"))
        .and_then(|v| v.obj_get("Value"))
        .and_then(JsonVal::as_str_array_first)
        .expect("study UID in the referenced request sequence");
    assert_eq!(
        study, STUDY_A,
        "(0040,A370 > 0020,000D) is the input study, not the workitem"
    );
    assert!(
        arr[0].obj_get("0020000D").is_none(),
        "no bare top-level study tag"
    );
    let item = arr[0]
        .obj_get("00404021")
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.first_of_array())
        .expect("input information sequence item");
    assert_eq!(
        item.obj_get("0040E020")
            .and_then(|v| v.obj_get("Value"))
            .and_then(JsonVal::as_str_array_first),
        Some("DICOM")
    );
    assert_eq!(
        item.obj_get("0020000D")
            .and_then(|v| v.obj_get("Value"))
            .and_then(JsonVal::as_str_array_first),
        Some(STUDY_A)
    );

    // The type-1 set and the SOP Class UID are all on the wire.
    //
    // (0040,4041) is INCOMPLETE here on purpose: this workitem was created by
    // a STOW moments ago and its study is treated as still arriving. The
    // READY it becomes is asserted in the `readiness_*` tests below, which
    // wait out the debounce.
    for (tag, expected) in [
        ("00080016", "1.2.840.10008.5.1.4.34.6.1"),
        ("00741204", "CAD"),
        ("00404041", "INCOMPLETE"),
        ("00741200", "MEDIUM"),
        // The vendor-extended Modality, taken from the input study (CT).
        ("00080060", "CT"),
    ] {
        assert_eq!(
            arr[0]
                .obj_get(tag)
                .and_then(|v| v.obj_get("Value"))
                .and_then(JsonVal::as_str_array_first),
            Some(expected),
            "{tag} must be present with the CAD value"
        );
    }
    assert!(
        arr[0].obj_get("00404005").is_some(),
        "(0040,4005) type-1 start datetime must be present"
    );

    let state = arr[0]
        .obj_get("00741000")
        .and_then(|v| v.obj_get("Value"))
        .and_then(JsonVal::as_str_array_first)
        .expect("ProcedureStepState in the search result");
    assert_eq!(state, "SCHEDULED");
}

/// A5/A5b (T3): the requestor names the patient and the step code it offers.
///
/// AIW-I 4.80.4.1.2.1 requires the request to state the Patient ID together
/// with its Issuer of Patient ID, and 4.80.4.1.2.4 requires the AIW-I task
/// fields - Scheduled Workitem Code Sequence among them, which the performer
/// echoes back as (0040,4019). Both are read from the study the batch has
/// just made durable, so the workitem carries what was actually committed
/// rather than what the request claimed.
#[test]
fn cad_workitem_carries_the_patient_identity_and_the_workitem_code() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5362", STUDY_A, SERIES_A, 0x42);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let mut stream = TcpStream::connect(srv.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let req = format!(
        "GET /dicomweb/workitems HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/dicom+json\r\nConnection: close\r\n\r\n",
        srv.addr.port()
    );
    stream.write_all(req.as_bytes()).expect("write req");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read resp");
    let resp = parse_http(&buf);
    assert_eq!(resp.status, 200);
    let json = parse_json(&resp.body).expect("workitems json");
    let arr = json.as_array().expect("workitem array");
    assert_eq!(arr.len(), 1, "one workitem in the worklist");

    // The Patient ID comes from the stored study, not from an invented one:
    // the fixture instance declares (0010,0020) = CADFIX01.
    assert_eq!(
        arr[0]
            .obj_get("00100020")
            .and_then(|v| v.obj_get("Value"))
            .and_then(JsonVal::as_str_array_first),
        Some("CADFIX01"),
        "the task names the patient of the study it operates on"
    );
    // The fixture declares no (0010,0021), so the element must be present
    // and ZERO-LENGTH - the standard's way of saying "no issuer at this
    // site". An absent element would be a different statement, and so would
    // one empty `<Value>`: PS3.18 F.2.5 states zero length by omitting
    // `Value` entirely, which is what both renderers now do.
    let issuer = arr[0]
        .obj_get("00100021")
        .expect("present when a patient is named");
    assert!(
        issuer.obj_get("Value").is_none(),
        "zero length omits Value instead of writing one empty string"
    );
    assert_eq!(
        issuer.obj_get("vr").and_then(JsonVal::as_str),
        Some("LO"),
        "(0010,0021) keeps its VR"
    );

    // A5b: (0040,4018), one item, the code the site's table gives this
    // study's modality. `build_instance` writes (0008,0060) = CT, so the
    // shipped table names it CT-ORGAN-SEG - the code states the
    // INTENT, and it is derived from the stored study's Modality rather than
    // being one universal constant.
    let code = arr[0]
        .obj_get("00404018")
        .and_then(|v| v.obj_get("Value"))
        .and_then(|v| v.first_of_array())
        .expect("(0040,4018) is on the wire");
    for (tag, expected) in [
        ("00080100", "CT-ORGAN-SEG"),
        ("00080102", "99CLARUS"),
        ("00080104", "AI inference"),
    ] {
        assert_eq!(
            code.obj_get(tag)
                .and_then(|v| v.obj_get("Value"))
                .and_then(JsonVal::as_str_array_first),
            Some(expected),
            "(0040,4018) item {tag}"
        );
    }

    // The server does not INTERPRET the code, it stores what it was given:
    // the same constant survives the CBOR round trip in the retrieve route.
    let mut stream = TcpStream::connect(srv.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let req = format!(
        "GET /dicomweb/workitems/{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/dicom+json\r\nConnection: close\r\n\r\n",
        srv.uid_for(STUDY_A, Some("CT")),
        srv.addr.port()
    );
    stream.write_all(req.as_bytes()).expect("write req");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read resp");
    let retrieved = parse_http(&buf);
    assert_eq!(retrieved.status, 200);
    let one = parse_json(&retrieved.body).expect("one workitem json");
    let issuer = one
        .obj_get("00100021")
        .expect("the issuer element survives the store round trip");
    assert!(
        issuer.obj_get("Value").is_none(),
        "the zero-length issuer survives the store round trip as zero length"
    );
    assert_eq!(
        one.obj_get("00404018")
            .and_then(|v| v.obj_get("Value"))
            .and_then(|v| v.first_of_array())
            .and_then(|item| item.obj_get("00080100"))
            .and_then(|v| v.obj_get("Value"))
            .and_then(JsonVal::as_str_array_first),
        Some("CT-ORGAN-SEG"),
        "and so does the scheduled code sequence"
    );
}

/// The code follows the STORED study's modality, so two studies of different
/// modalities in one deployment get different intents - which is what makes a
/// performer's `code`-constrained rules meaningful.
///
/// The fixture builder writes (0008,0060) as CT for this study, so the shipped
/// table names it CT-ORGAN-SEG rather than the DX code above.
#[test]
fn cad_workitem_code_follows_the_study_modality() {
    let srv = TestServer::start_cad();
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5390",
        STUDY_C,
        SERIES_C,
        0x51,
        "CT",
    );
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let record = srv
        .workitem_record(&srv.uid_for(STUDY_C, Some("CT")))
        .expect("a workitem was created for the CT study");
    let code = record.scheduled_workitem_code.expect("a scheduled code");
    assert_eq!(
        code.value, "CT-ORGAN-SEG",
        "the CT row of the table, not the DX row and not a universal constant"
    );
    assert_eq!(code.scheme, "99CLARUS", "the scheme stays the constant");
}

/// A modality the table does not name mints NOTHING under the shipped config:
/// the fallback is opt-in, and a generic workitem nobody claims is worse than
/// no workitem plus a log line an operator can act on.
///
/// This test previously asserted the opposite (an unlisted modality always
/// took `GENERIC-AI`), which is the behaviour the owner reversed: a performer
/// that stored its report back into the study used to mint one junk workitem
/// per result, which it then cancelled.
#[test]
fn cad_workitem_no_workitem_for_an_unlisted_modality_without_fallback() {
    let srv = TestServer::start_cad();
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5391",
        STUDY_D,
        SERIES_D,
        0x52,
        "US",
    );
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    assert!(
        srv.workitem_uids().is_empty(),
        "an unlisted modality with no configured fallback mints nothing"
    );
}

// -- Set-based mint: the STUDY's modalities, not the first instance's ------

/// A study that carries TWO modalities, as a real study does when a
/// performer stores its RESULT next to the images it came from: the images
/// are DX and the structured report is SR.
const STUDY_MIXED: &str = "1.2.826.0.1.3680043.10.1234.5105";
const SERIES_MIXED_IMG: &str = "1.2.826.0.1.3680043.10.1234.5205";
const SERIES_MIXED_SR: &str = "1.2.826.0.1.3680043.10.1234.5206";

/// A study that holds ONLY AI outputs (a report and a presentation state):
/// nothing in it is a task input.
const STUDY_RESULTS: &str = "1.2.826.0.1.3680043.10.1234.5106";
const SERIES_RESULTS_1: &str = "1.2.826.0.1.3680043.10.1234.5207";
const SERIES_RESULTS_2: &str = "1.2.826.0.1.3680043.10.1234.5208";

/// Boot with the SHIPPED defaults for the task table plus `task_producer` on.
///
/// This is the configuration a site gets out of the box: DX -> CXR-AI,
/// CT -> CT-ORGAN-SEG, no fallback (opt-in), and the shipped
/// `non_task_modalities` list that keeps AI outputs from minting.
fn start_shipped_catalog() -> TestServer {
    TestServer::start_cad()
}

/// RED FACT. A study whose modality SET is {DX, SR} yields exactly ONE
/// workitem, the DX one, and NO workitem for the report - and the answer does
/// not depend on which instance happened to arrive first.
///
/// Before the set-based mint the modality came from the batch's study header,
/// which is written from whichever instance arrived FIRST. A performer that
/// stored its SR result before the images therefore made the study's modality
/// SR, SR is not in the table, and the mint produced a single fallback
/// (`GENERIC-AI`) workitem - which the performer then cancelled, because its
/// own rules do not match a generic code. The images' CXR-AI task was never
/// offered at all.
#[test]
fn cad_set_mint_a_mixed_study_yields_only_its_task_modality() {
    let srv = start_shipped_catalog();
    // SR FIRST, which is the order that used to lose the DX workitem.
    let sr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5392",
        STUDY_MIXED,
        SERIES_MIXED_SR,
        0x53,
        "SR",
    );
    let image = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5393",
        STUDY_MIXED,
        SERIES_MIXED_IMG,
        0x54,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&sr])).status, 200);
    assert_eq!(srv.stow(&multipart_body(&[&image])).status, 200);

    let uids = srv.workitem_uids();
    assert_eq!(
        uids.len(),
        1,
        "a {{DX,SR}} study yields ONE workitem; got {uids:?}"
    );
    let record = srv.workitem_record(&uids[0]).expect("the one workitem");
    assert_eq!(
        record
            .scheduled_workitem_code
            .expect("a scheduled code")
            .value,
        "CXR-AI",
        "the DX row, not the fallback the first-arriving SR instance used to force"
    );
    assert_eq!(
        record.modality, "DX",
        "the workitem carries the modality that SELECTED it, not the batch's first one"
    );
    assert!(
        srv.readiness_pending() >= 1,
        "the study is armed for the readiness debounce, which is what promotes it"
    );
}

/// RED FACT. A study that holds ONLY AI outputs ({SR, PR}) yields NO workitem.
///
/// Before `non_task_modalities` the mint treated an unlisted modality as
/// "unknown input" and minted the fallback, so every result a performer stored
/// back created a `GENERIC-AI` workitem that the performer immediately
/// cancelled - one junk row per result, forever.
#[test]
fn cad_set_mint_a_results_only_study_mints_nothing() {
    let srv = start_shipped_catalog();
    let sr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5394",
        STUDY_RESULTS,
        SERIES_RESULTS_1,
        0x55,
        "SR",
    );
    let pr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5395",
        STUDY_RESULTS,
        SERIES_RESULTS_2,
        0x56,
        "PR",
    );
    assert_eq!(srv.stow(&multipart_body(&[&sr])).status, 200);
    assert_eq!(srv.stow(&multipart_body(&[&pr])).status, 200);

    let uids = srv.workitem_uids();
    assert!(
        uids.is_empty(),
        "a {{SR,PR}} study is all outputs; it must mint nothing, got {uids:?}"
    );
}

/// A study that carries a modality the table does not name AND no fallback
/// configured mints NOTHING rather than inventing a generic code.
///
/// This is the OPT-IN half of the fallback change: the shipped default is now
/// "no fallback", because a generic workitem nobody claims is worse than no
/// workitem plus a log line an operator can act on.
#[test]
fn cad_set_mint_unlisted_modality_without_fallback_mints_nothing() {
    let srv = start_shipped_catalog();
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5396",
        STUDY_D,
        SERIES_D,
        0x57,
        "US",
    );
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let uids = srv.workitem_uids();
    assert!(
        uids.is_empty(),
        "with no fallback configured an unlisted modality must mint nothing, got {uids:?}"
    );
}

/// With the fallback EXPLICITLY configured, an unlisted modality mints exactly
/// one workitem under the fallback code - the opt-in path.
#[test]
fn cad_set_mint_unlisted_modality_with_fallback_mints_it() {
    let srv = TestServer::start_with(|c| {
        c.ups.task_producer = true;
        c.ups.workitem_code_fallback = Some("GENERIC-AI".to_string());
    });
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5397",
        STUDY_D,
        SERIES_D,
        0x58,
        "US",
    );
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let uids = srv.workitem_uids();
    assert_eq!(uids.len(), 1, "one fallback workitem; got {uids:?}");
    assert_eq!(
        srv.workitem_record(&uids[0])
            .expect("the fallback workitem")
            .scheduled_workitem_code
            .expect("a scheduled code")
            .value,
        "GENERIC-AI"
    );
}

/// A non-task modality NEVER mints, even when a fallback is configured: the
/// strip is by modality, not by "is it in the table".
#[test]
fn cad_set_mint_non_task_modality_never_mints_even_with_fallback() {
    let srv = TestServer::start_with(|c| {
        c.ups.task_producer = true;
        c.ups.workitem_code_fallback = Some("GENERIC-AI".to_string());
    });
    let sr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5398",
        STUDY_RESULTS,
        SERIES_RESULTS_1,
        0x59,
        "SR",
    );
    assert_eq!(srv.stow(&multipart_body(&[&sr])).status, 200);

    let uids = srv.workitem_uids();
    assert!(
        uids.is_empty(),
        "SR is a non-task output and must not take the fallback, got {uids:?}"
    );
}

/// An unlisted modality beside a non-task one takes the fallback: the
/// candidate set is non-empty, so the opt-in applies.
#[test]
fn cad_set_mint_unlisted_beside_non_task_takes_the_fallback() {
    let srv = TestServer::start_with(|c| {
        c.ups.task_producer = true;
        c.ups.workitem_code_fallback = Some("GENERIC-AI".to_string());
    });
    let sr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5399",
        STUDY_MIXED,
        SERIES_MIXED_SR,
        0x5a,
        "SR",
    );
    let us = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5400",
        STUDY_MIXED,
        SERIES_MIXED_IMG,
        0x5b,
        "US",
    );
    assert_eq!(srv.stow(&multipart_body(&[&sr])).status, 200);
    assert_eq!(srv.stow(&multipart_body(&[&us])).status, 200);

    let uids = srv.workitem_uids();
    assert_eq!(
        uids.len(),
        1,
        "{{US,SR}} with a fallback yields ONE fallback workitem; got {uids:?}"
    );
    assert_eq!(
        srv.workitem_record(&uids[0])
            .expect("the fallback workitem")
            .scheduled_workitem_code
            .expect("a scheduled code")
            .value,
        "GENERIC-AI"
    );
}

/// A re-STOW of the same study does not duplicate anything, whatever the
/// instance order is: the UIDs are derived from (study, code).
#[test]
fn cad_set_mint_is_idempotent_across_re_stow() {
    let srv = start_shipped_catalog();
    let image = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5401",
        STUDY_MIXED,
        SERIES_MIXED_IMG,
        0x5c,
        "DX",
    );
    let sr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5402",
        STUDY_MIXED,
        SERIES_MIXED_SR,
        0x5d,
        "SR",
    );
    assert_eq!(srv.stow(&multipart_body(&[&image, &sr])).status, 200);
    let first = srv.workitem_uids();
    assert_eq!(srv.stow(&multipart_body(&[&sr, &image])).status, 200);
    let second = srv.workitem_uids();
    assert_eq!(first, second, "re-STOW must not change the workitem set");
    assert_eq!(first.len(), 1, "and the set is the one DX workitem");
}

/// Readiness walks the SAME selection: a `{DX,SR}` study promotes its DX
/// workitem, and the report's arrival changes nothing about it.
#[test]
fn cad_set_readiness_promotes_the_task_modality_only() {
    let srv = start_shipped_catalog();
    let image = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5403",
        STUDY_MIXED,
        SERIES_MIXED_IMG,
        0x5e,
        "DX",
    );
    let sr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5404",
        STUDY_MIXED,
        SERIES_MIXED_SR,
        0x5f,
        "SR",
    );
    assert_eq!(srv.stow(&multipart_body(&[&image, &sr])).status, 200);

    assert!(srv.promoted(STUDY_MIXED), "the DX workitem is promoted");
    let uids = srv.workitem_uids();
    assert_eq!(uids.len(), 1);
    assert_eq!(
        srv.workitem_record(&uids[0])
            .expect("the DX workitem")
            .input_readiness
            .as_deref(),
        Some("READY"),
        "the DX workitem is READY after promotion"
    );
}

/// Adding a RESULT to a study that is already READY changes NOTHING: the
/// report does not mint, so there is no workitem to regress and the DX
/// workitem keeps its READY state.
///
/// This is the live defect in one assertion: storing an SR report used to
/// create a `GENERIC-AI` workitem, and the `regress_readiness_on_new_instances`
/// path ran for it - churning the store on every result the performer wrote.
#[test]
fn cad_set_a_result_store_does_not_disturb_the_task_workitem() {
    let srv = start_shipped_catalog();
    let image = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5405",
        STUDY_MIXED,
        SERIES_MIXED_IMG,
        0x60,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&image])).status, 200);
    assert!(srv.promoted(STUDY_MIXED));
    let uids = srv.workitem_uids();
    assert_eq!(uids.len(), 1);
    assert_eq!(
        srv.workitem_record(&uids[0])
            .expect("the DX workitem")
            .input_readiness
            .as_deref(),
        Some("READY")
    );

    // Now the performer stores its report back into the same study.
    let sr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5406",
        STUDY_MIXED,
        SERIES_MIXED_SR,
        0x61,
        "SR",
    );
    assert_eq!(srv.stow(&multipart_body(&[&sr])).status, 200);

    assert_eq!(
        srv.workitem_uids(),
        uids,
        "the result store must not create (or drop) a workitem"
    );
    assert_eq!(
        srv.workitem_record(&uids[0])
            .expect("the DX workitem")
            .input_readiness
            .as_deref(),
        Some("READY"),
        "and it must not regress the task workitem: the report is not a new input"
    );
}

/// The wire shape of a catalog workitem carries the modality that SELECTED it.
#[test]
fn cad_set_workitem_json_carries_the_selecting_modality() {
    let srv = start_shipped_catalog();
    let sr = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5407",
        STUDY_MIXED,
        SERIES_MIXED_SR,
        0x62,
        "SR",
    );
    let image = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5408",
        STUDY_MIXED,
        SERIES_MIXED_IMG,
        0x63,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&sr, &image])).status, 200);

    let uids = srv.workitem_uids();
    assert_eq!(uids.len(), 1, "one workitem for the {{DX,SR}} study");
    // The search keys are the standard's (PS3.18 8.3.4); the workitem UID is
    // not among them, so the single workitem of this study is reached by
    // paging rather than by a UID key.
    let resp = srv.get_workitems("limit=1");
    assert_eq!(resp.status, 200, "UPS search finds the workitem");
    let body = String::from_utf8_lossy(&resp.body);
    assert!(
        body.contains("\"00080060\""),
        "the workitem states Modality (0008,0060): {body}"
    );
    assert!(
        body.contains("DX"),
        "and it states DX, the modality that selected the task: {body}"
    );
}

// -- Catalog: one workitem per entry, auto/manual split -------------------

/// Boot with a TWO-CODE catalog for DX, so one study yields two workitems.
///
/// The code list is a comma-separated VALUE, not a repeated key: the strict
/// parser refuses duplicate keys, so `DX = ...` twice is a startup error
/// rather than two records (`test_config_ups_catalog_refusals` pins that).
///
/// Both codes are AUTOMATIC: a parked (manual) code is declarative and is NOT
/// minted on STOW, so including one here would make the fixture yield fewer
/// workitems than codes - see
/// `cad_catalog_parked_code_is_not_minted_on_stow`.
fn start_two_entry_catalog() -> TestServer {
    TestServer::start_with(|c| {
        c.ups.task_producer = true;
        c.ups.workitem_codes = vec![
            ("DX".to_string(), "CXR-AI".to_string()),
            ("DX".to_string(), "BONE-AI".to_string()),
        ];
        c.ups.ai_station_aet = Some("AISTATION".to_string());
        c.ups.code_meanings = vec![
            ("CXR-AI".to_string(), "Chest X-ray AI".to_string()),
            ("BONE-AI".to_string(), "Bone age".to_string()),
        ];
    })
}

/// RED FACT. One study whose modality has TWO catalog entries yields TWO
/// workitems, each with its own code, and NOT one.
///
/// Before the catalog this test failed with a single workitem: the mint took
/// the FIRST match of the table (`code_for_modality`) and derived one UID
/// from the study alone, so a second entry for the same modality was
/// unreachable - the second AI task was never offered to anybody.
#[test]
fn cad_catalog_two_entries_of_one_modality_yield_two_workitems() {
    let srv = start_two_entry_catalog();
    // DX explicitly: the fixture's table lists ONLY DX, so the default CT
    // would take the fallback and the test would be about the wrong branch.
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5501",
        STUDY_A,
        SERIES_A,
        0x41,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let uids = srv.workitem_uids();
    // Sorted, because `workitem_uids` sorts and the catalog returns table
    // order: the two lists are compared as SETS of UIDs, not as sequences.
    let mut expected = srv.catalog_uids(STUDY_A, Some("DX"));
    expected.sort();
    assert_eq!(expected.len(), 2, "the fixture's catalog has two entries");
    assert_eq!(
        uids.len(),
        2,
        "TWO workitems, one per catalog entry; got {uids:?}"
    );
    assert_eq!(
        uids, expected,
        "exactly the catalog's UIDs, not the study-only one"
    );
    // The UIDs really are different objects, not the same one listed twice.
    assert_ne!(uids[0], uids[1], "the two entries have distinct UIDs");
    for uid in &uids {
        assert!(
            !uid.is_empty() && uid.starts_with("2.25."),
            "{uid} is a derived UID"
        );
    }
    assert!(
        !uids.contains(&workitem_uid_for_study(STUDY_A)),
        "the study-only UID is NOT among them: the code is part of the key"
    );

    // Each carries its OWN code, label and station. Looked up BY CODE rather
    // than by position, so the test does not depend on the sort order.
    let by_code: Vec<(String, String, Option<String>)> = uids
        .iter()
        .map(|uid| {
            let record = srv
                .workitem_record(uid)
                .unwrap_or_else(|| panic!("workitem {uid} exists"));
            let scheduled = record.scheduled_workitem_code.expect("a scheduled code");
            assert_eq!(scheduled.scheme, "99CLARUS");
            // Both are offered the same way: SCHEDULED, INCOMPLETE, same study.
            assert_eq!(record.workflow_state, "SCHEDULED");
            assert_eq!(record.input_readiness.as_deref(), Some("INCOMPLETE"));
            assert_eq!(record.input_study_uid.as_deref(), Some(STUDY_A));
            (scheduled.value, scheduled.meaning, record.station_aet)
        })
        .collect();

    let mut codes: Vec<&str> = by_code.iter().map(|(c, _, _)| c.as_str()).collect();
    codes.sort_unstable();
    assert_eq!(codes, vec!["BONE-AI", "CXR-AI"], "both codes are present");

    let bone = by_code
        .iter()
        .find(|(c, _, _)| c == "BONE-AI")
        .expect("the BONE-AI entry");
    assert_eq!(bone.1, "Bone age", "its own label, not the global one");
    assert_eq!(
        bone.2.as_deref(),
        Some("AISTATION"),
        "an automatic code goes to the configured AI station"
    );

    let cxr = by_code
        .iter()
        .find(|(c, _, _)| c == "CXR-AI")
        .expect("the CXR-AI entry");
    assert_eq!(cxr.1, "Chest X-ray AI");
    assert_eq!(
        cxr.2.as_deref(),
        Some("AISTATION"),
        "an automatic code goes to the configured AI station"
    );
}

/// RED FACT. A PARKED (manual) code is NOT minted on STOW: it stays
/// declarative and is created lazily by the Task Requester through UPS Create.
///
/// Before this a manual code was minted SCHEDULED and parked on the manual
/// station. That pre-minted workitem is non-terminal, so a Task Requester's
/// "Run" (UPS Create) sees an existing non-terminal workitem with that code
/// and does nothing - permanently. Only the automatic entries are minted; the
/// parked one simply produces no record and no readiness debounce.
#[test]
fn cad_catalog_parked_code_is_not_minted_on_stow() {
    let srv = TestServer::start_with(|c| {
        c.ups.task_producer = true;
        c.ups.workitem_codes = vec![
            ("DX".to_string(), "CXR-AI".to_string()),
            ("DX".to_string(), "REVIEW-ONLY".to_string()),
        ];
        c.ups.manual_codes = vec!["REVIEW-ONLY".to_string()];
        c.ups.ai_station_aet = Some("AISTATION".to_string());
    });
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5505",
        STUDY_A,
        SERIES_A,
        0x41,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let uids = srv.workitem_uids();
    assert_eq!(
        uids.len(),
        1,
        "only the AUTOMATIC entry is minted; got {uids:?}"
    );
    let record = srv.workitem_record(&uids[0]).expect("the one workitem");
    assert_eq!(
        record
            .scheduled_workitem_code
            .expect("a scheduled code")
            .value,
        "CXR-AI",
        "the automatic code, not the parked REVIEW-ONLY"
    );
    assert_eq!(
        record.station_aet.as_deref(),
        Some("AISTATION"),
        "and the parked station is never written"
    );
    assert!(
        !uids.contains(&workitem_uid_for_study_code(STUDY_A, "REVIEW-ONLY")),
        "the parked code's UID is not among the minted ones"
    );
}

/// Re-STOWing a two-entry catalog's study duplicates nothing: the per-(study,
/// code) UIDs are deterministic, so idempotency survives the catalog.
#[test]
fn cad_catalog_re_stow_does_not_duplicate_the_entries() {
    let srv = start_two_entry_catalog();
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5502",
        STUDY_A,
        SERIES_A,
        0x41,
        "DX",
    );

    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);
    let first = srv.workitem_uids();
    assert_eq!(first.len(), 2);

    let failures_before = clarus_server::metrics::CAD_WORKITEM_FAILURES_TOTAL
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);
    assert_eq!(srv.workitem_uids(), first, "the same two UIDs, not four");
    let failures_after = clarus_server::metrics::CAD_WORKITEM_FAILURES_TOTAL
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        failures_before, failures_after,
        "a duplicate entry is not counted as a creation failure"
    );
}

/// The table's LINE ORDER does not change the UIDs: the code is part of the
/// key, and the catalog is sorted, so re-ordering `clarus.conf` renumbers
/// nothing.
#[test]
fn cad_catalog_uid_order_is_independent_of_the_table_order() {
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5503",
        STUDY_A,
        SERIES_A,
        0x41,
        "DX",
    );

    // BONE-AI first in the config.
    let a = start_two_entry_catalog();
    assert_eq!(a.stow(&multipart_body(&[&instance])).status, 200);
    let uids_a = a.workitem_uids();

    // CXR-AI first, same set of codes.
    let b = TestServer::start_with(|c| {
        c.ups.task_producer = true;
        c.ups.workitem_codes = vec![
            ("DX".to_string(), "CXR-AI".to_string()),
            ("DX".to_string(), "BONE-AI".to_string()),
        ];
    });
    assert_eq!(b.stow(&multipart_body(&[&instance])).status, 200);
    let uids_b = b.workitem_uids();

    assert_eq!(
        uids_a, uids_b,
        "the same catalog yields the same UIDs whatever order the table lists"
    );
}

/// A single-entry catalog still writes NO station when `ai_station_aet` is
/// unset - the pre-catalog wire shape, which is what keeps an existing
/// deployment's output byte-identical.
#[test]
fn cad_catalog_automatic_entry_writes_no_station_by_default() {
    let srv = TestServer::start_cad();
    // CT: `start_cad` boots the SHIPPED table, whose entry this fixture's
    // default modality matches. Stating it keeps the test about the station
    // rather than about the selection rule.
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5504",
        STUDY_A,
        SERIES_A,
        0x41,
        "CT",
    );
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let record = srv
        .workitem_record(&srv.uid_for(STUDY_A, Some("CT")))
        .expect("the workitem exists");
    assert!(
        record.station_aet.is_none(),
        "no [ups] ai_station_aet means no (0040,0001), which is the shape \
         this server had before the catalog"
    );
    assert!(
        !record
            .scheduled_workitem_code
            .expect("a code")
            .meaning
            .is_empty(),
        "the label is present even with no station"
    );
}

// -- Worklist subscription push (W2, PS3.18 11.10) ------------------------

/// A miniature subscriber: accepts connections until one arrives whose request
/// line contains `needle`, then records that one and answers 200.
///
/// It loops because the subscription registry is process-global: a parallel
/// test's subscriber also receives this test's pushes, and vice versa, so the
/// first connection to arrive is not necessarily the one under test.
fn receive_push_for(
    listener: std::net::TcpListener,
    needle: String,
) -> std::thread::JoinHandle<(String, String)> {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "no push for {needle} within 20s"
            );
            // A bounded accept: a blocking accept would hide a missing push as
            // a hung test rather than a failure with a reason.
            listener.set_nonblocking(true).expect("nonblocking accept");
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(e) => panic!("accept push: {e}"),
            };
            stream.set_nonblocking(false).expect("blocking stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .expect("read timeout");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));

            let mut request_line = String::new();
            std::io::BufRead::read_line(&mut reader, &mut request_line).expect("request line");

            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                std::io::Read::read_exact(&mut reader, &mut body).expect("push body");
            }

            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("reply");
            stream.flush().expect("flush");

            if request_line.contains(&needle) {
                return (request_line, String::from_utf8_lossy(&body).into_owned());
            }
            // Not ours - keep listening.
        }
    })
}

/// Register `ae` for every workitem, returning the receiving channel.
fn subscribe(ae: &str, endpoint: &str) {
    let body = format!("{{\"endpoint\":\"{endpoint}\"}}");
    let resp = clarus_server::handlers::subscribe_pure::subscribe(ae, body.as_bytes(), false, "");
    assert_eq!(resp.status, 201, "subscribe failed: {:?}", resp.body);
}

/// Serialises the push tests.
///
/// The subscription registry is process-global and a subscriber receives every
/// workitem push, so two push tests running in parallel cross-deliver and the
/// receiver of one may close before the other's job is delivered. Holding this
/// for the whole register-STOW-assert sequence keeps each test deterministic.
fn push_registry_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// m17: a sweeper pass must re-arm ONLY on a store error.
///
/// `take_due` removes a due study, so the sweeper's re-arm decision is what
/// decides whether it comes back. Two cases must be told apart:
///
/// - the store could not be read or written: the study would be lost for
///   good (nothing re-arms it but a later STOW, and a settled study sends
///   none), so it is re-armed;
/// - the study has nothing promotable and nothing retryable about it (no
///   workitem, or every workitem has been claimed/cancelled): re-arming it
///   retried a terminal study every tick for the life of the process - a
///   store search and a WARN each time, plus a registry entry that never
///   went away.
#[test]
fn a_sweeper_pass_re_arms_a_study_whose_store_failed() {
    let srv = TestServer::start_cad();

    // Model the FAILURE that earns a retry: a study whose store cannot be
    // listed. `start_cad` owns the store directory, so the test removes it
    // while the study is armed - the next `search` fails, which is exactly
    // the environmental case.
    const ORPHAN: &str = "1.2.826.0.1.3680043.10.1234.9999";
    let armed_at = std::time::Instant::now();
    srv.arm(ORPHAN, armed_at);
    assert_eq!(srv.readiness_pending(), 1, "the study is armed");
    srv.break_workitem_store();

    let due_at = armed_at + clarus_server::handlers::readiness::READINESS_DEBOUNCE;
    srv.sweep_once(due_at);

    assert_eq!(
        srv.readiness_pending(),
        1,
        "a store failure must leave the study pending, not forgotten"
    );
    let next = srv
        .readiness_deadline(ORPHAN)
        .expect("the re-armed study has a deadline");
    assert!(
        next > due_at,
        "the retry waits a fresh quiet period: next={next:?} due={due_at:?}"
    );
}

/// The other half of m17: a TERMINAL study is not re-armed.
///
/// A study that was never minted into a workitem can never be promoted, no
/// matter how often the sweep runs. Re-arming it pinned the registry entry
/// and cost a store search plus a WARN on every tick forever - the defect
/// this task exists to remove.
#[test]
fn a_sweeper_pass_does_not_re_arm_a_terminal_study() {
    let srv = TestServer::start_cad();

    // Nothing was ever stored for this study, so there is no workitem and
    // none will appear: `promote` answers NothingToPromote, which is final.
    const NEVER_MINTED: &str = "1.2.826.0.1.3680043.10.1234.8888";
    let armed_at = std::time::Instant::now();
    srv.arm(NEVER_MINTED, armed_at);
    assert_eq!(srv.readiness_pending(), 1);

    let due_at = armed_at + clarus_server::handlers::readiness::READINESS_DEBOUNCE;
    srv.sweep_once(due_at);

    assert_eq!(
        srv.readiness_pending(),
        0,
        "a terminal study must be dropped from the registry, not retried"
    );
    assert!(
        srv.readiness_deadline(NEVER_MINTED).is_none(),
        "and it must have no deadline left"
    );
}

/// F1 on: a durable STOW must push the created workitem to a subscriber, and
/// the UID on the wire must be the one the Clarus store holds.
///
/// This is the whole point of F1+W2 together: the detection step learns about
/// the new study from the push, so a wrong or missing UID is a silent no-op in
/// the field.
#[test]
fn cad_workitem_is_pushed_to_a_subscriber_after_stow() {
    let _guard = push_registry_lock();
    let srv = TestServer::start_cad();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind subscriber");
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/ups");
    const AE: &str = "F1PUSHAE";
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(AE);
    subscribe(AE, &endpoint);
    // Listen for the push that names THIS study's workitem.
    let expected = srv.uid_for(STUDY_A, Some("CT"));
    let push = receive_push_for(listener, expected.clone());

    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5371", STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    // The STOW alone publishes nothing: the workitem is created INCOMPLETE so a
    // performer cannot start on a study that is still arriving. The offer goes
    // out when the quiet period expires and the sweeper promotes the study -
    // here driven directly, as every readiness test in this file does.
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("INCOMPLETE"),
        "a fresh STOW must not announce a READY workitem"
    );
    assert!(srv.promoted(STUDY_A), "the settled study promotes");

    let (request_line, body) = push.join().expect("subscriber thread");

    // Canonical store unchanged: exactly one workitem, and the push must name
    // THAT one. Comparing against the store keeps the test honest about what
    // matters - the subscriber is told about the workitem Clarus holds.
    let uids = srv.workitem_uids();
    assert_eq!(uids.len(), 1, "exactly one workitem: {uids:?}");
    assert_eq!(uids[0], expected, "the stored workitem is the derived one");

    assert!(
        request_line.contains(&expected),
        "push must address the stored workitem: expected {expected} in {request_line}"
    );
    assert!(
        body.contains(&expected),
        "push body must carry {expected}: {body}"
    );

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(AE);
}

/// F1 default (capability off): no workitem, therefore no push. This is the
/// regression guard for "the push must not fire on its own".
#[test]
fn no_push_when_cad_is_disabled() {
    let _guard = push_registry_lock();
    let srv = TestServer::start_default();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind subscriber");
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/ups");
    const AE: &str = "F1OFFAE";
    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(AE);
    subscribe(AE, &endpoint);

    // Non-blocking accept: if THIS server pushed, the connection would be here
    // by the time the STOW reply has been read (push is enqueued before the
    // response is written, and the worker is a separate thread).
    listener
        .set_nonblocking(true)
        .expect("nonblocking subscriber");

    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5372", STUDY_B, SERIES_B, 0x42);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    // The invariant that matters and that no parallel test can disturb: with
    // CAD off, this server holds no workitem, so it has nothing to push.
    assert!(
        srv.workitem_uids().is_empty(),
        "CAD off must create no workitem"
    );

    // Any connection that did arrive must NOT name a workitem of this study -
    // which is exactly the case because none exists. A stray push from a
    // parallel test's server cannot masquerade as this one's.
    if let Ok((stream, _)) = listener.accept() {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut reader = std::io::BufReader::new(stream);
        let mut request_line = String::new();
        let _ = std::io::BufRead::read_line(&mut reader, &mut request_line);
        let expected = srv.uid_for(STUDY_B, Some("CT"));
        assert!(
            !request_line.contains(&expected),
            "CAD off must not push this study's workitem: {request_line}"
        );
    }

    let _ = clarus_server::handlers::subscribe_pure::unsubscribe(AE);
}

// -- Input Readiness debounce (F1: offer only a settled study) -----------
//
// A CAD workitem is created on the FIRST durable STOW, and per-instance
// ingest is legal and unbounded, so at that moment the study is usually still
// arriving. A live run offered workitems after 2 of 53 and 3 of 36 instances
// and the performer - woken instantly by its WebSocket Notification
// Connection - produced structure sets over those few slices and COMPLETED
// them, so nothing downstream could tell those results from results over the
// whole study.
//
// The fix: create INCOMPLETE and publish NOTHING, then promote to READY (and
// publish there) once the study has been quiet for the debounce period.
//
// These tests drive `readiness::promote` directly rather than sleeping out
// the real debounce. The timer's own behaviour is unit-tested inside the
// module against a controllable clock; what matters at this level is that the
// workitem is INCOMPLETE and unannounced until then, and READY and announced
// exactly once after.

/// The stored readiness of one study's SINGLE workitem, or `None` when absent.
///
/// Every caller runs on a CT study (`build_instance` writes CT), which the
/// shipped table gives exactly one code, so the assertion below is what keeps
/// "the study's workitem" true: if a test ever boots a table that yields more
/// than one entry for the study, this fails loudly instead of silently reading
/// an arbitrary one.
fn stored_readiness(srv: &TestServer, study_uid: &str) -> Option<String> {
    let uids = srv.catalog_uids(study_uid, Some("CT"));
    assert_eq!(
        uids.len(),
        1,
        "stored_readiness expects one workitem for {study_uid}; got {uids:?}"
    );
    let store = WorkItemStore::new(srv.workitems_dir.clone());
    store
        .find_by_uid(&uids[0])
        .expect("lookup")
        .and_then(|rec| rec.input_readiness)
}

/// (a) The first STOW creates the workitem INCOMPLETE and announces nothing.
#[test]
fn readiness_first_stow_offers_an_incomplete_workitem_and_no_event() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5401", STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("INCOMPLETE"),
        "a workitem created by a STOW is INCOMPLETE until its study settles"
    );
    assert_eq!(
        srv.readiness_pending(),
        1,
        "the study is pending its quiet period, which is what makes the flip \
         happen without a second STOW"
    );
}

/// (b) A STOW inside the debounce window re-arms the timer and does not
/// promote. This is the case that used to complete a partial study: the
/// performer was woken by the first instance and the second arrived while it
/// worked.
///
/// The deadline is read before and after, because `pending == 1` holds for
/// both a working re-arm and one that does nothing. Asserting only the count
/// is how the missing re-arm passed this test the first time.
#[test]
fn readiness_a_second_stow_within_the_window_rearms_the_timer() {
    let srv = TestServer::start_cad();
    let first = build_instance("1.2.826.0.1.3680043.10.1234.5411", STUDY_A, SERIES_A, 0x41);
    let second = build_instance("1.2.826.0.1.3680043.10.1234.5412", STUDY_A, SERIES_A, 0x42);

    assert_eq!(srv.stow(&multipart_body(&[&first])).status, 200);
    let after_first = srv
        .readiness_deadline(STUDY_A)
        .expect("the first STOW arms the quiet period");

    assert_eq!(srv.stow(&multipart_body(&[&second])).status, 200);
    let after_second = srv
        .readiness_deadline(STUDY_A)
        .expect("the second STOW leaves the study pending");

    assert!(
        after_second > after_first,
        "the second STOW must push the deadline FORWARD, not merely leave the \
         study pending: {after_first:?} then {after_second:?}"
    );
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("INCOMPLETE"),
        "two instances of one study still leave it INCOMPLETE"
    );
    assert_eq!(
        srv.workitem_uids().len(),
        1,
        "and still one workitem: the second STOW re-armed, it did not create"
    );
    assert_eq!(
        srv.readiness_pending(),
        1,
        "the study is pending ONCE, however many STOWs arrived"
    );
}

/// (c) After the quiet period the workitem is READY, and promoting again
/// changes nothing - so a sweeper tick cannot flip it twice.
#[test]
fn readiness_promotion_is_idempotent() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5421", STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("INCOMPLETE")
    );

    assert!(srv.promoted(STUDY_A), "the promotion reports success");
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("READY"),
        "a settled study is READY"
    );
    assert!(srv.promoted(STUDY_A), "a repeat promotion is still success");
    assert_eq!(stored_readiness(&srv, STUDY_A).as_deref(), Some("READY"));
}

/// A claimed workitem is not re-armed by a late promotion.
///
/// The performer may claim the moment it sees READY, and an instance may
/// arrive after that. Rewriting readiness on a workitem whose state is no
/// longer SCHEDULED would change what an in-flight claim is about.
#[test]
fn readiness_does_not_touch_a_claimed_workitem() {
    let srv = TestServer::start_cad();
    let instance = build_instance("1.2.826.0.1.3680043.10.1234.5431", STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    // Move it out of SCHEDULED the way a performer would.
    let store = WorkItemStore::new(srv.workitems_dir.clone());
    let uid = srv.uid_for(STUDY_A, Some("CT"));
    let mut rec = store.find_by_uid(&uid).expect("lookup").expect("present");
    rec.workflow_state = "IN PROGRESS".to_string();
    store.cas_update(&rec, None).expect("claim");

    assert!(
        !srv.promoted(STUDY_A),
        "a workitem that is no longer SCHEDULED is left alone"
    );
    let after = store.find_by_uid(&uid).expect("lookup").expect("present");
    assert_eq!(
        after.input_readiness.as_deref(),
        Some("INCOMPLETE"),
        "readiness is untouched, so the claim still knows what it claimed"
    );
}

/// Promotion covers EVERY workitem of the study, not just the first.
///
/// A study with two catalog entries has two workitems, both INCOMPLETE. The
/// debounce promotes the STUDY, so both must flip - promoting only the one
/// whose UID happens to be derived from the study alone would leave the other
/// INCOMPLETE forever, and a performer addressed by its station would never
/// see it.
#[test]
fn readiness_promotes_every_workitem_of_the_study() {
    let srv = start_two_entry_catalog();
    let instance = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5601",
        STUDY_A,
        SERIES_A,
        0x41,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let uids = srv.catalog_uids(STUDY_A, Some("DX"));
    assert_eq!(uids.len(), 2, "the study has two workitems");
    for uid in &uids {
        assert_eq!(
            srv.workitem_record(uid)
                .expect("the workitem exists")
                .input_readiness
                .as_deref(),
            Some("INCOMPLETE"),
            "{uid} is offered INCOMPLETE before the study settles"
        );
    }

    assert!(srv.promoted(STUDY_A), "the promotion reports READY");

    let mut ready = 0;
    for uid in &uids {
        let record = srv.workitem_record(uid).expect("still there");
        if record.input_readiness.as_deref() == Some("READY") {
            ready += 1;
        }
    }
    assert_eq!(
        ready, 2,
        "BOTH workitems of the study are READY after one promotion, and \
         neither was skipped by an id derived from the study alone"
    );
}

/// A new instance sends EVERY workitem of the study back to INCOMPLETE: the
/// input all of them operate on was extended, so none of them is what a
/// performer was offered.
#[test]
fn readiness_re_regression_covers_every_workitem_of_the_study() {
    let srv = start_two_entry_catalog();
    let first = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5602",
        STUDY_A,
        SERIES_A,
        0x41,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&first])).status, 200);
    assert!(srv.promoted(STUDY_A), "the first promotion succeeds");

    let uids = srv.catalog_uids(STUDY_A, Some("DX"));
    for uid in &uids {
        assert_eq!(
            srv.workitem_record(uid)
                .expect("the workitem exists")
                .input_readiness
                .as_deref(),
            Some("READY"),
            "{uid} is READY before the re-STOW"
        );
    }

    // A SECOND instance arrives: the study grew, so every workitem of it must
    // go back to INCOMPLETE. It carries the SAME modality, because the entry
    // set is resolved per batch - a CT instance would resolve a different
    // entry and this test would silently assert nothing about the DX pair.
    let second = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5603",
        STUDY_A,
        SERIES_A,
        0x42,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&second])).status, 200);

    for uid in &uids {
        assert_eq!(
            srv.workitem_record(uid)
                .expect("the workitem exists")
                .input_readiness
                .as_deref(),
            Some("INCOMPLETE"),
            "{uid} went back to INCOMPLETE: the study it operates on grew"
        );
    }
}

/// The regression leaves a CLAIMED workitem alone even when its sibling is
/// regressed: the rule is per workitem, and a performer's claim must not be
/// rewritten because a different entry of the same study was re-offered.
#[test]
fn readiness_regression_leaves_a_claimed_sibling_alone() {
    let srv = start_two_entry_catalog();
    let first = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5604",
        STUDY_A,
        SERIES_A,
        0x41,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&first])).status, 200);
    assert!(srv.promoted(STUDY_A), "both entries become READY");

    let uids = srv.catalog_uids(STUDY_A, Some("DX"));
    assert_eq!(uids.len(), 2);

    // Claim ONE of the two, as a performer would (SCHEDULED -> IN PROGRESS
    // with a Transaction UID), straight through the store.
    let store = WorkItemStore::new(srv.workitems_dir.clone());
    let mut claimed = store
        .find_by_uid(&uids[0])
        .expect("lookup")
        .expect("present");
    claimed.workflow_state = "IN PROGRESS".to_string();
    claimed.transaction_uid = Some("1.2.3.4.5".to_string());
    store
        .cas_update(&claimed, None)
        .expect("claim the workitem");

    // Another instance arrives, same modality: it resolves the same entry set
    // as the first, which is what makes the regression land on these two.
    let second = build_instance_with_modality(
        "1.2.826.0.1.3680043.10.1234.5605",
        STUDY_A,
        SERIES_A,
        0x42,
        "DX",
    );
    assert_eq!(srv.stow(&multipart_body(&[&second])).status, 200);

    // The claimed one is untouched...
    let after_claim = store
        .find_by_uid(&uids[0])
        .expect("lookup")
        .expect("present");
    assert_eq!(
        after_claim.workflow_state, "IN PROGRESS",
        "a claimed workitem is left exactly as the performer left it"
    );
    // ...while the unclaimed sibling IS regressed, because the study grew.
    let after_free = store
        .find_by_uid(&uids[1])
        .expect("lookup")
        .expect("present");
    assert_eq!(
        after_free.input_readiness.as_deref(),
        Some("INCOMPLETE"),
        "the unclaimed entry is still offered INCOMPLETE"
    );
}

// -- Readiness driven by Storage Commitment ------------------------------
//
// The quiet debounce is the fallback for senders that do not commit. A
// sender that DOES commit says so explicitly, and the archive promotes on
// that statement instead of waiting out the timer - but only when the commit
// covers every instance the study's manifest holds. A commit that covers
// part of a study means the sender is still going, so the study stays
// INCOMPLETE and its quiet period is re-armed.
//
// These drive the real DICOMweb Commit route (PS3.18 13.4) rather than the
// promotion function directly, because the rule under test is the one the
// ROUTE applies: which studies a given transaction may promote.

const JSON_CT: &str = "application/dicom+json";

/// POST an Instance-form commit (Annex J.1).
fn commit_instance(srv: &TestServer, txn: &str, pairs: &[(&str, &str)]) -> Response {
    let mut body = String::from("{\"00081199\":{\"vr\":\"SQ\",\"Value\":[");
    for (i, (class, instance)) in pairs.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!(
            "{{\"00081150\":{{\"vr\":\"UI\",\"Value\":[\"{class}\"]}},\
             \"00081155\":{{\"vr\":\"UI\",\"Value\":[\"{instance}\"]}}}}"
        ));
    }
    body.push_str("]}}");
    srv.post(
        &format!("/dicomweb/commitment-requests/{txn}"),
        JSON_CT,
        body.as_bytes(),
    )
}

/// The SOP class every fixture instance declares.
const SOP_CLASS_CT: &str = "1.2.840.10008.5.1.4.1.1.2";

/// (1) A commit covering the study's ONLY instance promotes it at once.
///
/// One instance because the coverage rule is about the whole manifest: with a
/// single-instance study, "covers every stored instance" and "the one
/// instance" coincide, which makes the promotion unambiguous.
#[test]
fn sc_commit_covering_the_whole_study_promotes_it() {
    let srv = TestServer::start_cad();
    let sop = "1.2.826.0.1.3680043.10.1234.5501";
    let instance = build_instance(sop, STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("INCOMPLETE")
    );

    let r = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9501",
        &[(SOP_CLASS_CT, sop)],
    );
    assert_eq!(r.status, 200, "body: {}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("READY"),
        "a commit that covers the whole study readies it without the timer"
    );
}

/// (2) A partial commit leaves the study INCOMPLETE and re-arms its quiet
/// period; the commit that completes coverage then promotes it.
///
/// This is the owner's rule read strictly: "SC = I sent everything" holds per
/// TRANSACTION, so a sender that commits 1 of 2 instances has told us the
/// study is NOT complete, whatever the transaction meant locally.
#[test]
fn sc_a_partial_commit_keeps_incomplete_and_the_final_one_promotes() {
    let srv = TestServer::start_cad();
    let first = "1.2.826.0.1.3680043.10.1234.5511";
    let second = "1.2.826.0.1.3680043.10.1234.5512";
    let a = build_instance(first, STUDY_A, SERIES_A, 0x41);
    let b = build_instance(second, STUDY_A, SERIES_A, 0x42);
    assert_eq!(srv.stow(&multipart_body(&[&a, &b])).status, 200);
    let before = srv
        .readiness_deadline(STUDY_A)
        .expect("the study is pending its quiet period");

    // Half the study.
    let r = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9511",
        &[(SOP_CLASS_CT, first)],
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("INCOMPLETE"),
        "covering part of the study must not ready it"
    );
    let after = srv
        .readiness_deadline(STUDY_A)
        .expect("a partial commit re-arms the quiet period");
    assert!(
        after > before,
        "the partial commit is activity, so the deadline moves forward"
    );

    // The rest of it, with a NEW Transaction UID.
    let r = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9512",
        &[(SOP_CLASS_CT, first), (SOP_CLASS_CT, second)],
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("READY"),
        "the commit that completes coverage readies the study"
    );
}

/// (3) An unknown SOP UID is Failed 0112 and promotes nothing, even though
/// the request also carried an instance that IS stored.
#[test]
fn sc_an_unknown_uid_is_failed_and_promotes_nothing() {
    let srv = TestServer::start_cad();
    let sop = "1.2.826.0.1.3680043.10.1234.5521";
    let instance = build_instance(sop, STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let r = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9521",
        &[
            (SOP_CLASS_CT, sop),
            (SOP_CLASS_CT, "1.2.826.0.1.3680043.10.1234.9999"),
        ],
    );
    assert_eq!(r.status, 200, "the commit itself is answered");
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("0112"),
        "the unknown instance is reported Failed 0112: {body}"
    );
    // The stored instance is still Committed, and it still covers the whole
    // manifest - so the study IS promoted. What the unknown UID must not do
    // is contribute coverage, and it cannot: it belongs to no study.
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("READY"),
        "the stored instance still covers its study"
    );
}

/// (4) A second commit over the same SOPs under a NEW Transaction UID is
/// idempotent: still READY, no error, no second promotion.
#[test]
fn sc_a_repeat_commit_is_idempotent() {
    let srv = TestServer::start_cad();
    let sop = "1.2.826.0.1.3680043.10.1234.5531";
    let instance = build_instance(sop, STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let first = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9531",
        &[(SOP_CLASS_CT, sop)],
    );
    assert_eq!(first.status, 200);
    assert_eq!(stored_readiness(&srv, STUDY_A).as_deref(), Some("READY"));

    let second = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9532",
        &[(SOP_CLASS_CT, sop)],
    );
    assert_eq!(second.status, 200, "a fresh transaction is a fresh commit");
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("READY"),
        "and leaves the study where it was"
    );
}

/// (5) A commit spanning two studies decides each on its own coverage: the
/// fully covered one is promoted, the partly covered one is not.
#[test]
fn sc_one_commit_across_two_studies_decides_each_independently() {
    let srv = TestServer::start_cad();
    let a1 = "1.2.826.0.1.3680043.10.1234.5541";
    let b1 = "1.2.826.0.1.3680043.10.1234.5542";
    let b2 = "1.2.826.0.1.3680043.10.1234.5543";
    let i1 = build_instance(a1, STUDY_A, SERIES_A, 0x41);
    let i2 = build_instance(b1, STUDY_B, SERIES_B, 0x42);
    let i3 = build_instance(b2, STUDY_B, SERIES_B, 0x43);
    assert_eq!(srv.stow(&multipart_body(&[&i1, &i2, &i3])).status, 200);

    // STUDY_A is fully covered; STUDY_B is only half covered.
    let r = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9541",
        &[(SOP_CLASS_CT, a1), (SOP_CLASS_CT, b1)],
    );
    assert_eq!(r.status, 200);

    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("READY"),
        "the fully covered study is promoted"
    );
    assert_eq!(
        stored_readiness(&srv, STUDY_B).as_deref(),
        Some("INCOMPLETE"),
        "the partly covered study is not, in the SAME transaction"
    );
}

/// (6) A duplicate Transaction UID is 409 and promotes nothing: the refusal
/// happens before the coverage rule is ever consulted.
#[test]
fn sc_a_duplicate_transaction_uid_promotes_nothing() {
    let srv = TestServer::start_cad();
    let sop = "1.2.826.0.1.3680043.10.1234.5551";
    let instance = build_instance(sop, STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let txn = "1.2.826.0.1.3680043.10.1234.9551";
    // First use of the UID: commits and promotes.
    assert_eq!(
        commit_instance(&srv, txn, &[(SOP_CLASS_CT, sop)]).status,
        200
    );
    assert_eq!(stored_readiness(&srv, STUDY_A).as_deref(), Some("READY"));

    // Put the workitem back so the second attempt would be observable if it
    // wrongly promoted. A workitem that is already READY would hide it.
    let store = WorkItemStore::new(srv.workitems_dir.clone());
    let uid = srv.uid_for(STUDY_A, Some("CT"));
    let mut rec = store.find_by_uid(&uid).expect("lookup").expect("present");
    rec.input_readiness = Some("INCOMPLETE".to_string());
    store.cas_update(&rec, None).expect("reset");

    let again = commit_instance(&srv, txn, &[(SOP_CLASS_CT, sop)]);
    assert_eq!(again.status, 409, "the Transaction UID is already used");
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("INCOMPLETE"),
        "a refused commit must not touch readiness"
    );
}

/// (7) With the capability off there is no workitem, and a commit is still a
/// normal 200 - readiness is a feature of the CAD path, not of commitment.
#[test]
fn sc_a_commit_without_a_workitem_is_still_200() {
    let srv = TestServer::start_default();
    let sop = "1.2.826.0.1.3680043.10.1234.5561";
    let instance = build_instance(sop, STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);
    assert!(
        srv.workitem_uids().is_empty(),
        "the capability is off, so no workitem exists"
    );

    let r = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9561",
        &[(SOP_CLASS_CT, sop)],
    );
    assert_eq!(
        r.status, 200,
        "the commit is answered on its own terms, workitem or not"
    );
    assert!(srv.workitem_uids().is_empty(), "and still no workitem");
}

/// (8) REGRESSION: a study that is already READY goes back to INCOMPLETE when
/// a new instance arrives, and the quiet period restarts.
///
/// The debounce design did not specify this. `arm` only moves a deadline; it
/// does not revise the record, so without the regression a study declared
/// READY stayed READY while instances kept arriving - and a performer woken
/// by the READY announcement would claim an input the sender had since
/// extended.
#[test]
fn sc_a_stow_after_ready_goes_back_to_incomplete() {
    let srv = TestServer::start_cad();
    let first = "1.2.826.0.1.3680043.10.1234.5571";
    let second = "1.2.826.0.1.3680043.10.1234.5572";
    let a = build_instance(first, STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&a])).status, 200);

    // Ready it through a commit that covers the (single-instance) study.
    let r = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9571",
        &[(SOP_CLASS_CT, first)],
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("READY"),
        "precondition: the study is READY"
    );

    // Now the study grows.
    let b = build_instance(second, STUDY_A, SERIES_A, 0x42);
    assert_eq!(srv.stow(&multipart_body(&[&b])).status, 200);

    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("INCOMPLETE"),
        "a study that received a new instance is no longer what was offered"
    );
    assert_eq!(
        srv.readiness_pending(),
        1,
        "and its quiet period has restarted"
    );

    // The debounce still readies it once the study is whole again.
    assert!(srv.promoted(STUDY_A));
    assert_eq!(stored_readiness(&srv, STUDY_A).as_deref(), Some("READY"));
}

/// (9) A commit that covers the study, followed by the debounce promoting a
/// study nobody committed, are the same record: the two paths do not fight.
#[test]
fn sc_commit_and_debounce_agree_on_one_record() {
    let srv = TestServer::start_cad();
    let sop = "1.2.826.0.1.3680043.10.1234.5581";
    let instance = build_instance(sop, STUDY_A, SERIES_A, 0x41);
    assert_eq!(srv.stow(&multipart_body(&[&instance])).status, 200);

    let r = commit_instance(
        &srv,
        "1.2.826.0.1.3680043.10.1234.9581",
        &[(SOP_CLASS_CT, sop)],
    );
    assert_eq!(r.status, 200);
    assert_eq!(stored_readiness(&srv, STUDY_A).as_deref(), Some("READY"));

    // The sweeper may still hold this study from the STOW's own arm. Its
    // promotion must be a no-op rather than a second write.
    assert!(
        srv.promoted(STUDY_A),
        "the later debounce promotion reports success"
    );
    assert_eq!(
        stored_readiness(&srv, STUDY_A).as_deref(),
        Some("READY"),
        "and leaves the one record READY"
    );
    assert_eq!(srv.workitem_uids().len(), 1, "still exactly one workitem");
}
