// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Access-audit journal integration smoke (2808, ATNA-lite).
//!
//! Boots the real sync server over a TCP socket (same harness pattern as
//! wado_smoke), seeds one instance via STOW-RS, then proves:
//! - STOW produces a `110104` C line in `access/YYYYMMDD.log`;
//! - WADO-RS retrieve produces a `110103` R line;
//! - QIDO produces a `110112` R line with the query text;
//! - GET /admin/v1/access-log answers 200 and SELF-AUDITS (a `110101` line
//!   appears afterwards - GOST 27789 9.4);
//! - a 2808 update-in-place produces the U line (`event="attribute_update"`,
//!   `action="U"`, `atna="110103"`) with `changed_attrs` / `bytes_replaced`
//!   and NEVER the old attribute values.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

// -- Minimal DICOM builder (Explicit VR LE, uncompressed pixels) ---------

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

/// Minimal Explicit VR LE instance, 8x8 monochrome (same shape as wado_smoke).
fn build_instance(sop_uid: &str, patient_id: &str, study_uid: &str, series_uid: &str) -> Vec<u8> {
    build_instance_named(
        sop_uid,
        patient_id,
        "WADO^SMOKE",
        study_uid,
        series_uid,
        0x5a,
    )
}

/// Same builder with an overridable PatientName and pixel byte (W3 U-event
/// scenario sends OLD^NAME then NEW^NAME).
fn build_instance_named(
    sop_uid: &str,
    patient_id: &str,
    patient_name: &str,
    study_uid: &str,
    series_uid: &str,
    pixel_byte: u8,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", patient_name.as_bytes());
    append_element(&mut d, 0x0010, 0x0020, b"LO", patient_id.as_bytes());
    append_element(&mut d, 0x0020, 0x000d, b"UI", study_uid.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", series_uid.as_bytes());
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

// -- Test server harness ------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    access_dir: PathBuf,
    #[allow(dead_code)]
    _writer: clarus_server::access_log::AccessWriter,
    #[allow(dead_code)]
    pool: clarus_server::state::TranscodePool,
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
        let access_dir = btree_root.join("access");
        std::fs::create_dir_all(&access_dir).expect("access dir");

        let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");
        let mut config = Config::test_default();
        config.storage_dir = storage_dir.to_string_lossy().to_string();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.btree.access_dir = access_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

        let state = AppState {
            cas,
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
            study_scanner: StudyScanner::new(manifest_dir.clone(), idx_dir.clone()),
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(
                config.transcode.max_concurrent,
            ),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };
        let pool = state.transcode_pool.clone();

        // The journal writer MUST be installed before the first request -
        // the serve loop emits through the global channel.
        let writer = clarus_server::access_log::init(&access_dir, 0, false);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(state);
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let sd = shutdown.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, state, sd, timeouts, u32::MAX);
        });

        Self {
            addr,
            access_dir,
            _writer: writer,
            pool,
            _temp: temp,
        }
    }

    fn post(&self, path: &str, content_type: &str, body: &[u8]) -> Response {
        self.request("POST", path, Some(content_type), body)
    }

    fn get(&self, path: &str) -> Response {
        self.request("GET", path, None, &[])
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("timeout");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
            self.addr.port()
        );
        if let Some(ct) = content_type {
            head.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut req = head.into_bytes();
        req.extend_from_slice(body);
        stream.write_all(&req).expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read");
        parse_http(&buf)
    }

    /// Poll the journal file until it contains the wanted substring.
    fn wait_for_line(&self, want: &str) -> String {
        let dir = self.access_dir.clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut content = String::new();
        while Instant::now() < deadline {
            content.clear();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    if e.path().extension().and_then(|x| x.to_str()) == Some("log") {
                        content.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
                    }
                }
            }
            if content.contains(want) {
                return content;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("journal line missing: {want}\njournal so far:\n{content}");
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
    #[allow(dead_code)]
    body: Vec<u8>,
}

impl Response {
    fn assert_status(&self, expected: u16) {
        assert_eq!(self.status, expected, "status should be {expected}");
    }
}

// -- Fixed identity -----------------------------------------------------

const STUDY: &str = "1.2.826.0.1.3680043.10.5555.6001";
const SERIES: &str = "1.2.826.0.1.3680043.10.5555.6101";
const SOP: &str = "1.2.826.0.1.3680043.10.5555.6201";

// -- Scenario -----------------------------------------------------------

/// STOW + WADO + QIDO + admin access-log read all produce journal lines;
/// the read self-audits; plain traffic emits NO U lines; a 2808 update then
/// produces the U lines (identifiers only, changed_attrs/bytes_replaced).
#[test]
fn stow_wado_qido_and_selfaudit_produce_journal_lines() {
    let srv = TestServer::start();

    // STOW one instance -> 200, journal gets a 110104 C line.
    let dcm = build_instance(SOP, "ACCESSPAT01", STUDY, SERIES);
    srv.post("/dicomweb/studies", "application/dicom", &dcm)
        .assert_status(200);
    let journal = srv.wait_for_line("\"atna\":\"110104\"");
    assert!(
        journal.contains("\"action\":\"C\""),
        "STOW line is a C action"
    );
    assert!(
        journal.contains("\"count\":1"),
        "STOW line carries the accepted count"
    );
    // Plain STOW/WADO/QIDO traffic never emits U lines (updates only).
    assert!(
        !journal.contains("\"event\":\"attribute_update\"")
            && !journal.contains("\"atna\":\"110103\""),
        "no U lines from plain traffic"
    );

    // WADO instance retrieve -> 110103 R line with the SOP UID.
    srv.get(&format!(
        "/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP}"
    ))
    .assert_status(200);
    let journal = srv.wait_for_line("\"atna\":\"110103\"");
    assert!(journal.contains("\"action\":\"R\""));
    assert!(journal.contains(&format!("\"sop\":\"{SOP}\"")));
    assert!(journal.contains("\"obj_type\":\"110180\""));

    // QIDO studies -> 110112 R line carrying the query text.
    srv.get("/dicomweb/studies?Modality=CT").assert_status(200);
    let journal = srv.wait_for_line("\"atna\":\"110112\"");
    assert!(journal.contains("\"q\":\"Modality=CT\""));

    // Admin access-log read -> 200 AND a self-audit 110101 line appears.
    srv.get("/admin/v1/access-log").assert_status(200);
    let journal = srv.wait_for_line("\"atna\":\"110101\"");
    assert!(journal.contains("\"action\":\"R\""));

    // The reader returns the events (newest-first).
    let r = srv.get("/admin/v1/access-log");
    r.assert_status(200);
    let body = String::from_utf8_lossy(&r.body).into_owned();
    assert!(body.contains("\"total\":"), "reader envelope present");
    assert!(body.contains("110101"), "reader sees its own read");

    // 2808 update-in-place: the U line lands in the journal with identifiers
    // only - changed_attrs counts the (0400,0561) items, bytes_replaced is 1,
    // and the OLD attribute values never enter the journal. A dedicated
    // study keeps the update on the same PatientID (same-study + same-patient
    // is what makes a re-STOW an update, not a conflict).
    let upd_study = "1.2.826.0.1.3680043.10.5555.6002";
    let upd_series = "1.2.826.0.1.3680043.10.5555.6102";
    let upd_sop = "1.2.826.0.1.3680043.10.5555.6202";
    let old_dcm = build_instance_named(
        upd_sop,
        "UPDATEPAT01",
        "OLD^NAME",
        upd_study,
        upd_series,
        0x5a,
    );
    srv.post("/dicomweb/studies", "application/dicom", &old_dcm)
        .assert_status(200);

    // Corrected PatientName -> update: U line with changed_attrs=1.
    let new_dcm = build_instance_named(
        upd_sop,
        "UPDATEPAT01",
        "NEW^NAME",
        upd_study,
        upd_series,
        0x5a,
    );
    srv.post("/dicomweb/studies", "application/dicom", &new_dcm)
        .assert_status(202);
    let journal = srv.wait_for_line("\"event\":\"attribute_update\"");
    assert!(journal.contains("\"action\":\"U\""), "U action");
    assert!(
        journal.contains("\"atna\":\"110103\""),
        "110103 DICOM Instances Accessed"
    );
    assert!(
        journal.contains("\"changed_attrs\":1"),
        "PN correction = one 0561 item"
    );
    assert!(
        journal.contains("\"bytes_replaced\":1"),
        "replacement flag set"
    );
    assert!(
        journal.contains(&format!("\"sop\":\"{upd_sop}\"")),
        "sop UID carried"
    );
    assert!(
        journal.contains("\"patient\":\"UPDATEPAT01\""),
        "PatientID carried"
    );
    // Minimization: the OLD value lives only inside the stored file's 0561.
    assert!(
        !journal.contains("OLD^NAME"),
        "old attribute values never enter the journal"
    );

    // Pixel-only replacement -> update with changed_attrs=0.
    let new_px = build_instance_named(
        upd_sop,
        "UPDATEPAT01",
        "NEW^NAME",
        upd_study,
        upd_series,
        0x5b,
    );
    srv.post("/dicomweb/studies", "application/dicom", &new_px)
        .assert_status(202);
    let journal = srv.wait_for_line("\"changed_attrs\":0");
    let last_u = journal
        .lines()
        .rev()
        .find(|l| l.contains("\"event\":\"attribute_update\""))
        .expect("U line present");
    assert!(
        last_u.contains("\"changed_attrs\":0"),
        "pixel-only update embeds no 0561 items: {last_u}"
    );
    assert!(
        last_u.contains("\"bytes_replaced\":1"),
        "pixels replaced: {last_u}"
    );
}
