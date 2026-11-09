// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! cas->manifests rebuild wire test.
//!
//! `POST /admin/v1/recovery/rebuild` (explicit sources) must re-derive the
//! study manifests from live CAS blobs and drive the background worker to
//! `done`, after which QIDO sees the study again. Also covers:
//! - 409 while a rebuild is already running,
//!   - `dry_run` writes nothing (no manifest files created),
//!   - source validation (empty / unknown / unconfigured cold -> 400).
//!
//! This is a SEMANTIC test (studies become visible again), NOT byte-exact -
//! rebuild is enrichment-only for existing manifests: missing capture tags
//! are added, existing values and `arrived_at` are preserved, so the QIDO
//! bytes of an enriched study may differ from a live STOW.

#![forbid(unsafe_code)]

use clarus_server::config::Config;
use clarus_server::handlers::json_common::{JsonVal, parse_json};
use clarus_server::shadow::ShadowTier;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Test server harness (mirrors cas_audit_smoke.rs) -----------------

struct TestServer {
    addr: SocketAddr,
    cas_root: PathBuf,
    manifest_dir: PathBuf,
    _temp: tempfile::TempDir,
}

impl TestServer {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let cas_root = temp.path().join("cas");
        std::fs::create_dir_all(&cas_root).expect("cas dir");

        let btree_root = temp.path().join("btree");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let mut config = Config::test_default();
        config.storage_dir = cas_root.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.btree.audit_dir = btree_root.join("audit").to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        let state = AppState {
            cas: clarus_adapters::storage::ContentAddressedStorage::new(cas_root.clone())
                .expect("open cas"),
            codec_registry: clarus_adapters::codec::CodecRegistry::new(),
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
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir.clone(), idx_dir),
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

        Self {
            addr,
            cas_root,
            manifest_dir,
            _temp: temp,
        }
    }

    /// Write a DICOM blob into the hot CAS 2-2 layout, addressed by its BLAKE3.
    fn write_cas_blob(&self, dicom: &[u8]) -> String {
        let name = clarus_adapters::hex_encode(&clarus_core::hash::cas_hash_one(dicom));
        let bucket = self.cas_root.join(&name[0..2]).join(&name[2..4]);
        std::fs::create_dir_all(&bucket).expect("bucket");
        std::fs::write(bucket.join(format!("{name}.dcm")), dicom).expect("write cas blob");
        name
    }

    fn manifest_count(&self) -> usize {
        std::fs::read_dir(&self.manifest_dir)
            .map(|e| {
                e.flatten()
                    .filter(|p| p.path().extension().is_some_and(|x| x == "manifest"))
                    .count()
            })
            .unwrap_or(0)
    }

    /// True if the hot CAS blob addressed by `blake3_hex` still exists.
    fn cas_blob_exists(&self, blake3_hex: &str) -> bool {
        self.cas_root
            .join(&blake3_hex[0..2])
            .join(&blake3_hex[2..4])
            .join(format!("{blake3_hex}.dcm"))
            .exists()
    }

    fn post_json(&self, path: &str, body: &str) -> Response {
        self.request(
            "POST",
            path,
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        )
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path, &[], &[])
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
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

/// Poll GET rebuild status until terminal or timeout, returning the terminal JSON.
fn wait_done(srv: &TestServer, timeout: Duration) -> JsonVal {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let r = srv.get("/admin/v1/recovery/rebuild/status");
        r.assert_status(200);
        let j = r.json();
        let status = j.obj_get("status").and_then(|x| x.as_str()).unwrap_or("");
        if status == "done" || status.starts_with("error") {
            return j;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("rebuild did not reach done in {:?}", timeout);
}

/// Reset the process-global rebuild state so parallel tests start clean.
fn reset_rebuild_state() {
    let mut st = clarus_server::handlers::admin::pure::REBUILD_STATE
        .write()
        .unwrap();
    *st = clarus_server::handlers::admin::pure::RebuildState {
        status: "idle".to_string(),
        files_scanned: 0,
        manifests_written: 0,
        instances_indexed: 0,
        skipped: 0,
        errors: 0,
        started_at: None,
        finished_at: None,
        sources: Vec::new(),
        dry_run: false,
    };
}

// -- DICOM builder (Explicit VR LE) ------------------------------------

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
            b"CS" | b"LO" | b"SH" | b"PN" | b"DA" | b"TM" | b"IS" | b"UI"
        ) {
            0x20
        } else {
            0x00
        };
        buf.push(pad);
    }
}

fn build_instance(study_uid: &str, series_uid: &str, sop_uid: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0008, 0x0020, b"DA", b"20240101");
    append_element(&mut d, 0x0008, 0x0030, b"TM", b"120000");
    append_element(&mut d, 0x0008, 0x0050, b"SH", b"ACC-REBUILD");
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"REBUILD^TEST");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"PID-REBUILD");
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x0010, b"SH", b"REBUILD-1");
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x0011, b"IS", b"1");
    append_element(&mut d, 0x0020, 0x0013, b"IS", b"1");
    append_element(&mut d, 0x0008, 0x0021, b"DA", b"20240101");
    append_element(&mut d, 0x0008, 0x1010, b"SH", b"STATION1");
    append_element(&mut d, 0x0008, 0x103e, b"LO", b"REBUILD SERIES");
    append_element(&mut d, 0x0018, 0x0015, b"CS", b"CHEST");
    // 08.09 Stage B capture: the seven attributes must survive rebuild.
    append_element(&mut d, 0x0008, 0x0070, b"LO", b"ACME"); // Manufacturer
    append_element(&mut d, 0x0008, 0x0080, b"LO", b"CITY HOSPITAL"); // InstitutionName
    append_element(&mut d, 0x0008, 0x1040, b"LO", b"RADIOLOGY"); // InstitutionalDepartmentName
    append_element(&mut d, 0x0008, 0x1070, b"PN", b"OPS^ANNA"); // OperatorsName
    append_element(&mut d, 0x0018, 0x1030, b"LO", b"CHEST_PROTO"); // ProtocolName
    append_element(&mut d, 0x0032, 0x1032, b"PN", b"REQ^PAUL"); // RequestingPhysician
    append_element(&mut d, 0x0032, 0x1060, b"LO", b"CT CHEST WO"); // RequestedProcedureDescription
    d
}

// -- Scenarios ---------------------------------------------------------

// A single test: the rebuild worker writes the process-global REBUILD_STATE,
// so I must NOT run these scenarios on parallel threads (they would race the
// shared static). This mirrors `cas_audit_smoke`, which is also a single test.

/// One rebuild may run at a time: `REBUILD_STATE` is process-global, so the
/// tests that drive it must not overlap whatever thread they run on.
static REBUILD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn rebuild_smoke_scenarios() {
    let _guard = REBUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_rebuild_state();
    let srv = TestServer::start();
    let study_a = "1.2.840.113619.2.55.3.380.7777.1";
    let dicom_a = build_instance(
        study_a,
        "1.2.840.113619.2.55.3.380.7777.1.1",
        "1.2.840.113619.2.55.3.380.7777.1.1.1",
    );

    // -- 1. Source validation (status idle) ----------------------------
    let empty = srv.post_json("/admin/v1/recovery/rebuild", r#"{"sources":[]}"#);
    empty.assert_status(400);
    let unknown = srv.post_json("/admin/v1/recovery/rebuild", r#"{"sources":["tape"]}"#);
    unknown.assert_status(400);
    let cold = srv.post_json("/admin/v1/recovery/rebuild", r#"{"sources":["cold"]}"#);
    cold.assert_status(400);

    // -- 2. dry_run writes nothing (status idle) -----------------------
    let a_blake = srv.write_cas_blob(&dicom_a);
    let dry = srv.post_json(
        "/admin/v1/recovery/rebuild?dry_run",
        r#"{"sources":["hot"]}"#,
    );
    dry.assert_status(202);
    assert_eq!(
        dry.json().obj_get("dry_run").and_then(|x| x.as_bool()),
        Some(true)
    );
    let dry_done = wait_done(&srv, Duration::from_secs(10));
    assert_eq!(
        dry_done.obj_get("dry_run").and_then(|x| x.as_bool()),
        Some(true)
    );
    assert_eq!(srv.manifest_count(), 0, "dry_run must not write manifests");
    // CAS blob is untouched (invariant: rebuild never deletes CAS).
    assert!(srv.cas_blob_exists(&a_blake), "dry_run must not delete CAS");

    // -- 3. Real rebuild restores QIDO --------------------------------
    let pre = srv.get("/dicomweb/studies");
    pre.assert_status(200);
    assert!(
        !String::from_utf8_lossy(&pre.body).contains(study_a),
        "QIDO must NOT see the study before rebuild"
    );

    let r = srv.post_json("/admin/v1/recovery/rebuild", r#"{"sources":["hot"]}"#);
    r.assert_status(202);
    assert_eq!(
        r.json().obj_get("status").and_then(|x| x.as_str()),
        Some("running")
    );

    let done = wait_done(&srv, Duration::from_secs(10));
    assert_eq!(
        done.obj_get("status").and_then(|x| x.as_str()),
        Some("done")
    );
    assert_eq!(
        done.obj_get("manifests_written").and_then(|x| x.as_u64()),
        Some(1)
    );
    assert_eq!(
        done.obj_get("instances_indexed").and_then(|x| x.as_u64()),
        Some(1)
    );
    assert_eq!(done.obj_get("errors").and_then(|x| x.as_u64()), Some(0));

    assert_eq!(srv.manifest_count(), 1, "rebuild must write a manifest");
    assert!(srv.cas_blob_exists(&a_blake), "rebuild must not delete CAS");

    let post = srv.get("/dicomweb/studies");
    post.assert_status(200);
    assert!(
        String::from_utf8_lossy(&post.body).contains(study_a),
        "QIDO must see the rebuilt study"
    );

    // -- 3b. 0809 ENRICHMENT fix (red-fact): a manifest written the OLD way
    // (no new capture tags) must be enriched by a rebuild whose CAS blobs DO
    // carry those tags. The previous 0809 scenario seeded through the CURRENT
    // STOW writer, so the manifest already carried the new tags and the
    // rebuild had nothing to add - vacuous. This scenario cannot pass
    // without `write_study_manifest_rebuild` enrichment.
    reset_rebuild_state();
    let study_c = "1.2.840.113619.2.55.3.380.9999.1";
    let series_c = "1.2.840.113619.2.55.3.380.9999.1.1";
    let sop_c = "1.2.840.113619.2.55.3.380.9999.1.1.1";
    let dicom_c = build_instance(study_c, series_c, sop_c);
    srv.write_cas_blob(&dicom_c);

    // OLD capture shape: the new fields are None; StationName carries a
    // STALE value the enrichment must NOT overwrite.
    let mut old_attrs = clarus_core::stow::StudyManifestAttrs {
        patient_name: Some("OLD^PATIENT".into()),
        patient_id: Some("PID-OLD".into()),
        study_instance_uid: study_c.to_string(),
        study_date: Some("20240101".into()),
        modality: "CT".into(),
        ..Default::default()
    };
    let old_series = [clarus_core::stow::SeriesModel {
        series_instance_uid: series_c.to_string(),
        modality: "CT".into(),
        series_number: Some("1".into()),
        station_name: Some("OLDSTATION".into()),
        series_description: Some("OLD DESC".into()),
        instances: vec![clarus_core::stow::InstanceModel {
            sop_instance_uid: sop_c.to_string(),
            blake3: clarus_core::hash::cas_hash_one(&dicom_c),
            storage_tier: clarus_core::manifest::TIER_HOT,
            transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
            file_size: 1234,
            ..Default::default()
        }],
        ..Default::default()
    }];
    clarus_core::stow::write_study_manifest_full(&srv.manifest_dir, &mut old_attrs, &old_series)
        .expect("write old-shaped manifest");
    let old_path = srv.manifest_dir.join(format!("{study_c}.manifest"));
    let before = clarus_core::manifest::read_manifest(&old_path).expect("read old");
    let arrived_before = before.sys.arrived_at;
    let bytes_before = before.sys.total_pixel_bytes;
    assert_eq!(bytes_before, 1234, "old manifest reports the old byte sum");

    // RED fact: the old capture shape is NOT searchable before the rebuild.
    let r = srv.get(&format!(
        "/dicomweb/studies/{study_c}/series?Manufacturer=ACME"
    ));
    r.assert_status(200);
    assert_eq!(
        parse_json(&r.body)
            .expect("json")
            .as_array()
            .expect("array")
            .len(),
        0,
        "no enrichment before the rebuild"
    );

    let r3 = srv.post_json("/admin/v1/recovery/rebuild", r#"{"sources":["hot"]}"#);
    r3.assert_status(202);
    let done3 = wait_done(&srv, Duration::from_secs(10));
    assert_eq!(
        done3.obj_get("status").and_then(|x| x.as_str()),
        Some("done")
    );

    // GREEN: the new capture keys are searchable + includefield-visible.
    let r = srv.get(&format!(
        "/dicomweb/studies/{study_c}/series?Manufacturer=ACME"
    ));
    r.assert_status(200);
    assert_eq!(
        parse_json(&r.body)
            .expect("json")
            .as_array()
            .expect("array")
            .len(),
        1,
        "rebuild must enrich Manufacturer"
    );
    let r = srv.get("/dicomweb/studies?includefield=00080080");
    r.assert_status(200);
    let body = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        body.contains("CITY HOSPITAL"),
        "rebuild must enrich 00080080"
    );

    // Existing values win: the stale StationName survives enrichment.
    let r = srv.get(&format!(
        "/dicomweb/studies/{study_c}/series?includefield=00081010"
    ));
    r.assert_status(200);
    let body = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        body.contains("OLDSTATION"),
        "existing StationName must win, got: {body}"
    );
    assert!(
        !body.contains("STATION1"),
        "incoming StationName must not overwrite, got: {body}"
    );

    // sys preserved verbatim (E-014 arrival order + byte accounting).
    let after = clarus_core::manifest::read_manifest(&old_path).expect("read enriched");
    assert_eq!(
        after.sys.arrived_at, arrived_before,
        "arrived_at must survive enrichment"
    );
    assert_eq!(
        after.sys.total_pixel_bytes, bytes_before,
        "total_pixel_bytes must survive enrichment"
    );

    // -- 4. 409 while already running ----------------------------------
    {
        let mut st = clarus_server::handlers::admin::pure::REBUILD_STATE
            .write()
            .unwrap();
        st.status = "running".to_string();
    }
    let conflict = srv.post_json("/admin/v1/recovery/rebuild", r#"{"sources":["hot"]}"#);
    conflict.assert_status(409);
    assert_eq!(
        conflict.json().obj_get("status").and_then(|x| x.as_str()),
        Some("running")
    );

    // -- 5. Dedup + malformed skip ------------------------------------
    reset_rebuild_state();
    let study_b = "1.2.840.113619.2.55.3.380.8888.1";
    let dicom_b = build_instance(
        study_b,
        "1.2.840.113619.2.55.3.380.8888.1.1",
        "1.2.840.113619.2.55.3.380.8888.1.1.1",
    );
    // Duplicate: same DICOM bytes stored twice -> same blake3 filename (the
    // worker dedups by SOPInstanceUID, so only one instance is indexed).
    let b_blake = srv.write_cas_blob(&dicom_b);
    srv.write_cas_blob(&dicom_b); // identical bytes, overwrites same path
    // Malformed blob: not valid DICOM, must be skipped.
    let mut junk = vec![0u8; 512];
    junk[..4].copy_from_slice(b"junk");
    srv.write_cas_blob(&junk);

    let r2 = srv.post_json("/admin/v1/recovery/rebuild", r#"{"sources":["hot"]}"#);
    r2.assert_status(202);
    let done2 = wait_done(&srv, Duration::from_secs(10));
    assert_eq!(
        done2.obj_get("status").and_then(|x| x.as_str()),
        Some("done")
    );
    // Study A rebuilt again (still 1), study B indexed once, malformed skipped.
    assert!(
        done2
            .obj_get("instances_indexed")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
            >= 2,
        "both valid studies must be indexed"
    );
    assert!(
        done2
            .obj_get("skipped")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
            >= 1,
        "malformed blob must be skipped"
    );
    // Both CAS blobs remain (invariant).
    assert!(srv.cas_blob_exists(&b_blake), "rebuild must not delete CAS");

    reset_rebuild_state();
}

/// A manifest write that fails must be an ERROR, and a pass with errors must
/// not clear the degraded gauges.
///
/// The counters used to advance before the write and the flags cleared on
/// every non-dry run, so a rebuild that restored nothing reported "done" with
/// a green store - exactly the signal an operator uses to decide the store is
/// healthy.
#[test]
fn a_failed_manifest_write_is_an_error_and_keeps_the_degraded_flags() {
    use std::sync::atomic::Ordering as AtomicOrdering;
    // `REBUILD_STATE` is process-global and only one rebuild can run at a
    // time, so this test takes the same serialising guard the scenarios use.
    let _guard = REBUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_rebuild_state();
    let srv = TestServer::start();

    // One valid blob with no manifest, so the rebuild must write one.
    let study = "1.2.840.113619.2.55.3.380.8888.1";
    let dicom = build_instance(
        study,
        "1.2.840.113619.2.55.3.380.8888.1.1",
        "1.2.840.113619.2.55.3.380.8888.1.1.1",
    );
    srv.write_cas_blob(&dicom);

    // Raise the degraded flags so a wrongful clear is observable.
    clarus_server::metrics::MANIFEST_DEGRADED.store(1, AtomicOrdering::Relaxed);
    clarus_server::metrics::MANIFEST_MISSING.store(1, AtomicOrdering::Relaxed);

    // Make the manifests directory unwritable by replacing it with a FILE of
    // the same name: every create/rename inside it then fails.
    let manifest_dir = srv.manifest_dir.clone();
    std::fs::remove_dir_all(&manifest_dir).expect("drop manifest dir");
    std::fs::write(&manifest_dir, b"not a directory").expect("blocker");

    let r = srv.post_json("/admin/v1/recovery/rebuild", r#"{"sources":["hot"]}"#);
    assert!(
        r.status == 200 || r.status == 202,
        "rebuild accepted: {}",
        r.status
    );
    let done = wait_done(&srv, Duration::from_secs(20));

    let errors = done.obj_get("errors").and_then(|x| x.as_u64()).unwrap_or(0);
    assert!(
        errors > 0,
        "a manifest write that cannot land must be counted as an error: {done:?}"
    );
    let indexed = done
        .obj_get("instances_indexed")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    assert_eq!(
        indexed, 0,
        "an instance is not indexed before its write lands"
    );
    assert_eq!(
        clarus_server::metrics::MANIFEST_DEGRADED.load(AtomicOrdering::Relaxed),
        1,
        "a failed pass must not clear the degraded gauge"
    );

    // Clean up the blocker so the temp dir can be removed.
    let _ = std::fs::remove_file(&manifest_dir);
}
