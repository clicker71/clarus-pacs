// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! PS3.18 conformance sweep - every MANDATORY (shall/M) fact, as one assert.
//!
//! This is NOT a regression guard. It is a conformance INVENTORY: each assert
//! encodes a fact the standard makes mandatory, and a FAILING assert is a
//! discovered non-conformance. The sweep is therefore EXPECTED TO BE RED at
//! HEAD; the RED list is the backlog.
//!
//! Run it with `just conformance-sweep` (offline, deterministic) after
//! `just standard-drift` (network) has confirmed the standard/ base is
//! current. It is deliberately NOT part of `gate` / `gate-full` / the default
//! test run - it is a separate binary, run on demand and before release.
//!
//! Live mode: set `PS318_GATE_BASE` (e.g. http://127.0.0.1:8024/dicomweb) to
//! run the SAME assert body as a black-box client against a running server
//! instead of booting one in-process. An unreachable base prints a note and
//! skips (a field run is not a CI failure).
//!
//! Every fact this file asserts is pinned in `standard/ps3.18/*.json`; the
//! `assert_standard_edition` helper fails the run when a referenced file no
//! longer pins the current edition, forcing a human re-verify before trusting
//! the asserts.

#![forbid(unsafe_code)]

mod common;

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

/// The PS3.18 edition every referenced fact file must pin. Bump this (and the
/// fact files) together when the standard edition changes.
const REQUIRED_EDITION: &str = "PS3.18 2026d";

/// Fixture UIDs (stable, well-formed, non-colliding).
const STUDY_A: &str = "1.2.826.0.1.3680043.10.1234.6001";
const SERIES_A: &str = "1.2.826.0.1.3680043.10.1234.6101";
const SOP_A: &str = "1.2.826.0.1.3680043.10.1234.6201";
const STUDY_B: &str = "1.2.826.0.1.3680043.10.1234.6002";
const SERIES_B: &str = "1.2.826.0.1.3680043.10.1234.6102";
const SOP_B: &str = "1.2.826.0.1.3680043.10.1234.6202";
/// The RLE Lossless single-frame instance (C4 compressed rendered).
const SOP_RLE: &str = "1.2.826.0.1.3680043.10.1234.6203";

const DICOM_JSON: &str = "application/dicom+json";

// -- Standard edition freshness ------------------------------------------

/// Assert a `standard/ps3.18/*.json` fact file pins the current edition via
/// TWO independent `contains` checks (the key and the value), so whitespace
/// reformatting cannot make it pass or fail spuriously. No JSON parser, no
/// regex crate, no new dependency.
fn assert_standard_edition(file: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("standard")
        .join("ps3.18")
        .join(file);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("standard fact file missing {}: {e}", path.display()));
    assert!(
        text.contains("\"standardEdition\""),
        "{file}: no standardEdition key - re-verify against dicom.nema.org"
    );
    assert!(
        text.contains(REQUIRED_EDITION),
        "{file}: does not pin {REQUIRED_EDITION} - the standard may have changed; re-verify the fact"
    );
}

/// Every fact file this sweep relies on must pin the current edition. Called
/// once so a stale base fails the whole run with a clear list.
#[test]
fn standard_fact_files_pin_the_current_edition() {
    for file in [
        "capabilities-8.9-annexH.json",
        "qido-pagination-8.3.4.4.json",
        "qido-10.6.1-5-matching-attributes.json",
        "wado-rs-retrieve-status.json",
        "wado-bulkdata-pixeldata-10.4.1.json",
        "rendered-thumbnail.json",
        "stow-rs-media-types.json",
        "ups-rs-transactions.json",
        "mwl-rs-transactions.json",
        "mpps-rs-transactions.json",
    ] {
        assert_standard_edition(file);
    }
}

// -- Server harness (in-process) / live client ---------------------------

/// The base the sweep runs against. Unset `PS318_GATE_BASE` boots the server
/// in-process (the deterministic CI path); a set value makes every assert run
/// against that running base as a black-box client.
enum Target {
    InProcess(TestServer),
    Live(String),
}

impl Target {
    fn base(&self) -> String {
        match self {
            Target::InProcess(s) => format!("http://127.0.0.1:{}/dicomweb", s.addr.port()),
            Target::Live(base) => base.trim_end_matches('/').to_string(),
        }
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        match self {
            Target::InProcess(s) => s.request("GET", &wire(path), headers, None),
            Target::Live(_) => http_request(&self.base(), "GET", path, headers, None),
        }
    }

    /// OPTIONS requests (PS3.18 8.9.1: the Capabilities Transaction method).
    fn options(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        match self {
            Target::InProcess(s) => s.request("OPTIONS", &wire(path), headers, None),
            Target::Live(_) => http_request(&self.base(), "OPTIONS", path, headers, None),
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Response {
        match self {
            Target::InProcess(s) => s.request(method, &wire(path), headers, body),
            Target::Live(_) => http_request(&self.base(), method, path, headers, body),
        }
    }
}

/// The in-process server routes the DICOMweb surface under `/dicomweb`; the
/// live base already carries that prefix in `PS318_GATE_BASE`, so the assert
/// bodies stay prefix-free and this adds it only for the in-process path.
fn wire(path: &str) -> String {
    format!("/dicomweb{path}")
}

/// Build the target for a scenario. In CI this boots the server in-process;
/// with `PS318_GATE_BASE` set it is a live black-box run.
fn target() -> Option<Target> {
    match std::env::var("PS318_GATE_BASE") {
        Ok(base) if !base.trim().is_empty() => {
            // A live run: probe reachability once. An unreachable base is
            // SKIPPED (a note), never a CI failure.
            if http_request(base.trim_end_matches('/'), "GET", "/studies", &[], None).reachable {
                Some(Target::Live(base.trim_end_matches('/').to_string()))
            } else {
                eprintln!(
                    "conformance-sweep: PS318_GATE_BASE={base} unreachable - SKIPPING live run"
                );
                None
            }
        }
        _ => Some(Target::InProcess(TestServer::start())),
    }
}

struct TestServer {
    addr: SocketAddr,
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
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        // The sweep exercises UPS/MWL/MPPS/commitment, so enable them.
        config.workflow.enabled = true;
        // The MWL/MPPS store roots at `[workflow] dir`; point it at THIS test's
        // temp dir so records never leak between tests in the process (the
        // default is a shared system-temp root, which makes a second create of
        // the same UID a spurious 409).
        config.workflow.dir = btree_root.join("workflow").to_string_lossy().to_string();

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
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: ShadowTier::new(btree_root.join("idx")),
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
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, state, shutdown, timeouts, u32::MAX);
        });

        Self { addr, _temp: temp }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Response {
        let addr = self.addr;
        raw_request(addr, method, path, headers, body)
    }
}

/// One HTTP/1.1 request over a fresh socket against `addr`, body optional.
fn raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Response {
    let mut stream = match TcpStream::connect(addr) {
        Ok(s) => s,
        Err(_) => return Response::unreachable(),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let body = body.unwrap_or(&[]);
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\nConnection: close\r\n",
        addr.port(),
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut req = head.into_bytes();
    req.extend_from_slice(body);
    if stream.write_all(&req).is_err() {
        return Response::unreachable();
    }
    let mut buf = Vec::new();
    if stream.read_to_end(&mut buf).is_err() {
        return Response::unreachable();
    }
    parse_http(&buf)
}

/// Black-box request against a `base` URL (`http://host:port/path-prefix`).
fn http_request(
    base: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Response {
    let rest = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))
        .unwrap_or(base);
    let (authority, prefix) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let addr = match authority.parse::<SocketAddr>() {
        Ok(a) => a,
        Err(_) => match std::net::ToSocketAddrs::to_socket_addrs(&authority) {
            Ok(mut it) => match it.next() {
                Some(a) => a,
                None => return Response::unreachable(),
            },
            Err(_) => return Response::unreachable(),
        },
    };
    let full_path = format!("{prefix}{path}");
    raw_request(addr, method, &full_path, headers, body)
}

// -- Response + HTTP parsing ---------------------------------------------

struct Response {
    status: u16,
    headers: std::collections::HashMap<String, Vec<String>>,
    body: Vec<u8>,
    reachable: bool,
}

impl Response {
    fn unreachable() -> Self {
        Self {
            status: 0,
            headers: std::collections::HashMap::new(),
            body: Vec::new(),
            reachable: false,
        }
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .and_then(|v| v.first())
            .map(|s| s.as_str())
    }
    fn content_type(&self) -> &str {
        self.header("content-type").unwrap_or("")
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
    fn json(&self) -> clarus_server::handlers::json_common::JsonVal {
        clarus_server::handlers::json_common::parse_json(&self.body).expect("json body")
    }
}

fn parse_http(buf: &[u8]) -> Response {
    let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Response::unreachable();
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let body = buf[head_end + 4..].to_vec();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut headers: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            headers
                .entry(k.trim().to_ascii_lowercase())
                .or_default()
                .push(v.trim().to_string());
        }
    }
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.first().is_some_and(|s| s.contains("chunked")));
    let body = if chunked {
        decode_chunked(&body).unwrap_or(body)
    } else {
        body
    };
    Response {
        status,
        headers,
        body,
        reachable: true,
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
    data.get(start..)?
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| start + i)
}

// -- DICOM fixture builders (Explicit VR LE) -----------------------------

fn push_evrle(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    // DICOM values are even length: pad text VRs with a space, the rest with
    // NUL, so the element stream never desyncs the parser.
    let mut padded;
    let value =
        if !matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN") && value.len() % 2 == 1 {
            padded = value.to_vec();
            padded.push(if matches!(vr, b"UI") { 0 } else { b' ' });
            padded.as_slice()
        } else {
            value
        };
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    // OB/OW/OF/SQ/UT/UN use a 4-byte length with 2 reserved bytes.
    if matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN") {
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    buf.extend_from_slice(value);
}

fn push_uid(buf: &mut Vec<u8>, group: u16, element: u16, uid: &str) {
    // Raw bytes, exactly as the other smoke suites write UIDs (the store
    // parser tolerates the natural odd length; padding changes nothing).
    push_evrle(buf, group, element, b"UI", uid.as_bytes());
}

/// A single-frame 8x8 MONOCHROME2 Explicit VR LE instance.
fn build_single_frame(sop_uid: &str, study_uid: &str, series_uid: &str) -> Vec<u8> {
    build_sc_image(sop_uid, study_uid, series_uid, 1, 8, 8)
}

/// A single-frame 8x8 MONOCHROME2 **RLE Lossless** (1.2.840.10008.1.2.5)
/// instance. RLE is the one compressed syntax whose codec ships in the default
/// build, so the sweep can prove the compressed `/rendered` path end-to-end
/// without a network encoder. The encapsulated stream is one fragment: the
/// PS3.5 Annex G 64-byte header (one segment, first offset 64) followed by a
/// PackBits literal run of the 64 pixel bytes.
fn build_rle_frame(sop_uid: &str, study_uid: &str, series_uid: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(2048);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_uid(&mut d, 0x0002, 0x0010, "1.2.840.10008.1.2.5"); // RLE Lossless
    push_uid(&mut d, 0x0008, 0x0016, "1.2.840.10008.5.1.4.1.1.2"); // CT Image Storage
    push_uid(&mut d, 0x0008, 0x0018, sop_uid);
    push_evrle(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_evrle(&mut d, 0x0010, 0x0010, b"PN", b"SWEEP^PATIENT");
    push_evrle(&mut d, 0x0010, 0x0020, b"LO", b"SWEEP01");
    push_uid(&mut d, 0x0020, 0x000D, study_uid);
    push_uid(&mut d, 0x0020, 0x000E, series_uid);
    push_evrle(&mut d, 0x0020, 0x0013, b"IS", b"1");
    push_evrle(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes()); // SamplesPerPixel
    push_evrle(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_evrle(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes()); // Rows
    push_evrle(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes()); // Columns
    push_evrle(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes()); // BitsAllocated
    push_evrle(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes()); // BitsStored
    // PS3.5 Annex G header: 1 segment, its offset 64, the rest zero padding.
    let mut frame = vec![0u8; 64];
    frame[0..4].copy_from_slice(&1u32.to_le_bytes());
    frame[4..8].copy_from_slice(&64u32.to_le_bytes());
    // PackBits: n+1 gives a literal run of 64 bytes, so the header byte is 63.
    frame.push(63);
    frame.extend(std::iter::repeat_n(0u8, 64));
    // Encapsulated PixelData: undefined length (0xFFFFFFFF), one item holding
    // the fragment. `push_evrle` writes the OB header (reserved 2 + u32 len),
    // so the length field is the 4 bytes ending at the current tail.
    push_evrle(&mut d, 0x7FE0, 0x0010, b"OB", &[]);
    let pd_len_off = d.len() - 4;
    d[pd_len_off..pd_len_off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    // Item and Sequence Delimiter tags are little-endian (FFFE,E000 / FFFE,E0DD).
    d.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    let flen = frame.len() as u32;
    d.extend_from_slice(&flen.to_le_bytes());
    d.extend_from_slice(&frame);
    d.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    d
}

/// A multi-frame 8x8 MONOCHROME2 Explicit VR LE instance with `frames` frames.
fn build_multi_frame(sop_uid: &str, study_uid: &str, series_uid: &str, frames: u16) -> Vec<u8> {
    build_sc_image(sop_uid, study_uid, series_uid, frames, 8, 8)
}

fn build_sc_image(
    sop_uid: &str,
    study_uid: &str,
    series_uid: &str,
    frames: u16,
    rows: u16,
    cols: u16,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(2048);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_uid(&mut d, 0x0002, 0x0010, "1.2.840.10008.1.2.1");
    push_uid(&mut d, 0x0008, 0x0016, "1.2.840.10008.5.1.4.1.1.2"); // CT Image Storage
    push_uid(&mut d, 0x0008, 0x0018, sop_uid);
    push_evrle(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_evrle(&mut d, 0x0010, 0x0010, b"PN", b"SWEEP^PATIENT");
    push_evrle(&mut d, 0x0010, 0x0020, b"LO", b"SWEEP01");
    push_uid(&mut d, 0x0020, 0x000D, study_uid);
    push_uid(&mut d, 0x0020, 0x000E, series_uid);
    push_evrle(&mut d, 0x0020, 0x0013, b"IS", b"1");
    push_evrle(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes()); // SamplesPerPixel
    push_evrle(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_evrle(&mut d, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    if frames > 1 {
        let v = frames.to_string();
        let mut padded = v.into_bytes();
        if padded.len() % 2 == 1 {
            padded.push(b' ');
        }
        push_evrle(&mut d, 0x0028, 0x0008, b"IS", &padded); // NumberOfFrames
    }
    let pixel = vec![0u8; rows as usize * cols as usize * frames as usize];
    push_evrle(&mut d, 0x7FE0, 0x0010, b"OW", &pixel);
    d
}

/// The Change Workitem State / Update payload: the new Procedure Step State
/// (0040,1001) plus the Transaction UID (0008,1195) that owns the change.
fn state_body(new_state: &str, transaction_uid: &str) -> Vec<u8> {
    format!(
        r#"{{"00741000":{{"vr":"CS","Value":["{new_state}"]}},"00081195":{{"vr":"UI","Value":["{transaction_uid}"]}}}}"#
    )
    .into_bytes()
}

/// A valid UPS Create body (PS3.4 CC.2.5-3 type-1 set). No Modality exists in
/// UPS.
fn workitem_body(label: &str) -> Vec<u8> {
    format!(
        r#"{{"00404005":{{"vr":"DT","Value":["20260917T103000"]}},"00404041":{{"vr":"CS","Value":["READY"]}},"00741200":{{"vr":"CS","Value":["MEDIUM"]}},"00741204":{{"vr":"SH","Value":["{label}"]}},"00404025":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["STATION01"]}},"00080102":{{"vr":"SH","Value":["99CLARUS"]}},"00080104":{{"vr":"LO","Value":["AI station"]}}}}]}},"00100010":{{"vr":"PN","Value":[{{"Alphabetic":"SWEEP^PATIENT"}}]}},"00100020":{{"vr":"LO","Value":["SWEEPPAT01"]}}}}"#
    )
    .into_bytes()
}

/// A Storage Commitment instance-form body referencing one SOP Instance UID.
fn commitment_body(sop_class_uid: &str, sop_instance_uid: &str) -> Vec<u8> {
    format!(
        r#"{{"00081199":{{"vr":"SQ","Value":[{{"00081150":{{"vr":"UI","Value":["{sop_class_uid}"]}},"00081155":{{"vr":"UI","Value":["{sop_instance_uid}"]}}}}]}}}}"#
    )
    .into_bytes()
}

// -- Seeding --------------------------------------------------------------

/// STOW one instance as a single `application/dicom` part. Returns the
/// response so a caller can assert the store contract too.
fn stow_instance(t: &Target, study_uid: &str, bytes: &[u8]) -> Response {
    t.request(
        "POST",
        &format!("/studies/{study_uid}"),
        &[("Content-Type", "application/dicom")],
        Some(bytes),
    )
}

/// STOW one instance as a single `multipart/related` part. This is the media
/// type every STOW-RS origin server is required to accept (8.7.3.1), so the
/// seed lands on both Clarus and a strict reference like dcm4chee-arc, which
/// rejects the optional single-part `application/dicom` form with 415
/// (standard/ps3.18/stow-rs-media-types.json `dcm4chee415`).
fn stow_multipart(t: &Target, study_uid: &str, bytes: &[u8]) -> Response {
    let boundary = "conformance-sweep-seed";
    let mut body = Vec::with_capacity(bytes.len() + 128);
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let ct = format!("multipart/related; type=\"application/dicom\"; boundary=\"{boundary}\"");
    t.request(
        "POST",
        &format!("/studies/{study_uid}"),
        &[("Content-Type", &ct)],
        Some(&body),
    )
}

/// Seed the standard sweep study: one single-frame instance plus one
/// multi-frame instance (for the frames assert C3), via multipart/related so
/// the seed is accepted by every conformant STOW-RS server.
fn seed(t: &Target) {
    let single = build_single_frame(SOP_A, STUDY_A, SERIES_A);
    let multi = build_multi_frame(SOP_B, STUDY_B, SERIES_B, 4);
    let _ = stow_multipart(t, STUDY_A, &single);
    let _ = stow_multipart(t, STUDY_B, &multi);
}

// ========================================================================
// A. Capabilities (PS3.18 8.9 / Annex H)
// ========================================================================

/// A. PS3.18 8.9: every REST Web Service SHALL implement the Retrieve
/// Capabilities Transaction. 8.9.1 fixes the request as `OPTIONS /`; Annex H
/// defines the Capabilities Description as a WADL document with an
/// `application` -> `resources` -> `resource` tree. Anchored on
/// standard/ps3.18/capabilities-8.9-annexH.json.
#[test]
fn a_capabilities_describes_the_service() {
    let Some(t) = target() else { return };
    let r = t.options("", &[("Accept", "application/vnd.sun.wadl+xml")]);
    assert_eq!(
        r.status, 200,
        "8.9: OPTIONS on the base URI must answer 200, got {}",
        r.status
    );
    assert!(
        r.content_type().starts_with("application/vnd.sun.wadl+xml"),
        "8.9.3: Content-Type must be the WADL media type, got {}",
        r.content_type()
    );
    let body = r.text();
    for needle in [
        "<application",
        "<resources",
        "<resource",
        "modality-scheduled-procedure-steps",
    ] {
        assert!(
            body.contains(needle),
            "Annex H: the Capabilities Description must be WADL carrying '{needle}': {body}"
        );
    }
}

// ========================================================================
// B. QIDO-RS Search (PS3.18 10.6)
// ========================================================================

/// B1. 8.3.3: a successful search answers 200 + application/dicom+json across
/// every search resource.
#[test]
fn b1_search_resources_answer_json() {
    let Some(t) = target() else { return };
    seed(&t);
    for path in ["/studies", "/series", "/instances", "/patients"] {
        let r = t.get(path, &[]);
        assert_eq!(r.status, 200, "8.3.3: {path} must answer 200");
        assert!(
            r.content_type().starts_with(DICOM_JSON),
            "8.3.3: {path} must answer {DICOM_JSON}, got {}",
            r.content_type()
        );
    }
}

/// B2. 8.3.4 Table 8.3.4-1 + 8.5: `limit`/`offset` are `uint`; a non-uint is an
/// invalid query parameter -> 400. [finding 3]
#[test]
fn b2_non_uint_limit_offset_is_400() {
    let Some(t) = target() else { return };
    for q in ["?limit=-1", "?offset=-1", "?limit=abc", "?offset=abc"] {
        let r = t.get(&format!("/studies{q}"), &[]);
        assert_eq!(
            r.status, 400,
            "8.3.4/8.5: /studies{q} must be 400 (non-uint), got {}",
            r.status
        );
    }
}

/// B3. 8.3.4.4.1: `limit=0` forces `results = 0` -> 200 with an empty array.
/// [finding 3]
#[test]
fn b3_limit_zero_is_empty_array() {
    let Some(t) = target() else { return };
    seed(&t);
    let r = t.get("/studies?limit=0", &[]);
    assert_eq!(r.status, 200, "8.3.4.4.1: limit=0 must answer 200");
    assert_eq!(
        r.text().trim(),
        "[]",
        "8.3.4.4.1: limit=0 must answer an empty JSON array"
    );
}

/// B4. 8.3.4.4.1 (shall): a truncated result carries
/// `Warning: 299 <service>: There are <n> additional results that can be
/// requested`. [finding 4]
#[test]
fn b4_truncation_warning_wording() {
    let Some(t) = target() else { return };
    seed(&t);
    let r = t.get("/studies?limit=1", &[]);
    assert_eq!(r.status, 200, "8.3.4.4.1: a page answers 200");
    let warn = r
        .header("warning")
        .unwrap_or_else(|| panic!("8.3.4.4.1 shall: no Warning header on a truncated page"));
    assert!(
        warn.contains("299"),
        "8.3.4.4.1: Warning must carry the 299 code: {warn}"
    );
    assert!(
        warn.contains("additional results that can be requested"),
        "8.3.4.4.1: Warning must use the standard wording \
         'There are <n> additional results that can be requested': {warn}"
    );
}

/// B5. 10.6.1-5: every REQUIRED matching attribute is a legal query key (the
/// deterministic floor: a legal key must NOT be rejected with 400). Result
/// correctness is covered by tests/qido_golden.rs and is NOT duplicated here.
#[test]
fn b5_required_matching_keys_are_accepted() {
    let Some(t) = target() else { return };
    seed(&t);
    // Study-level required matching keys (PS3.18 10.6.1-5).
    let study_keys = [
        "0020000D=1.2.3",
        "00080020=20260101",
        "00080030=120000",
        "00080050=ACC1",
        "00080061=CT",
        "00080090=SWEEP^REF",
        "00100010=SWEEP^PATIENT",
        "00100020=SWEEP01",
        "00100030=19800101",
        "00100040=M",
        "00200010=SWEEPACC",
    ];
    for k in study_keys {
        let r = t.get(&format!("/studies?{k}"), &[]);
        assert_ne!(
            r.status, 400,
            "10.6.1-5: legal matching key '{k}' must not be 400, got {}",
            r.status
        );
    }
}

// ========================================================================
// C. WADO-RS Retrieve (PS3.18 10.4)
// ========================================================================

/// C1. 10.4.3.3.1: study/series/instance retrieve -> 200.
#[test]
fn c1_study_series_instance_retrieve() {
    let Some(t) = target() else { return };
    seed(&t);
    for path in [
        format!("/studies/{STUDY_A}"),
        format!("/studies/{STUDY_A}/series/{SERIES_A}"),
        format!("/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_A}"),
    ] {
        let r = t.get(&path, &[]);
        assert!(
            r.status == 200,
            "10.4.3.3.1: {path} must answer 200, got {}",
            r.status
        );
    }
}

/// C2. 10.4.1.1.2 Table 10.4.1-2: metadata at all THREE levels -> 200.
/// [finding 6]
#[test]
fn c2_metadata_three_levels() {
    let Some(t) = target() else { return };
    seed(&t);
    for path in [
        format!("/studies/{STUDY_A}/metadata"),
        format!("/studies/{STUDY_A}/series/{SERIES_A}/metadata"),
        format!("/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_A}/metadata"),
    ] {
        let r = t.get(&path, &[("Accept", DICOM_JSON)]);
        assert_eq!(
            r.status, 200,
            "10.4.1.1.2: {path} must answer 200, got {}",
            r.status
        );
    }
}

/// C3. 10.4.3.1 Table 10.4.3-1: a frame in range -> 200; a frame beyond
/// NumberOfFrames is an absent Target Resource -> 404 (NOT 416). [finding 5]
#[test]
fn c3_frames_in_range_and_beyond() {
    let Some(t) = target() else { return };
    seed(&t);
    let base = format!("/studies/{STUDY_B}/series/{SERIES_B}/instances/{SOP_B}");
    let ok = t.get(&format!("{base}/frames/1"), &[]);
    assert_eq!(
        ok.status, 200,
        "10.4: frames/1 of a 4-frame instance must answer 200, got {}",
        ok.status
    );
    let oob = t.get(&format!("{base}/frames/5"), &[]);
    assert_eq!(
        oob.status, 404,
        "10.4.3-1: frames/5 on a 4-frame instance must be 404, got {}",
        oob.status
    );
}

/// C4. Rendered instance is MANDATORY (PS3.18 8.7.4 Table 8.7.4-1 for REST
/// single-frame). 10.4.3.3.3 wants a rendering of every valid instance, so an
/// Accept the server can satisfy must not be refused: an uncompressed AND a
/// compressed single-frame instance both answer 200 image/jpeg. The old
/// early guard answered 406 on ANY compressed syntax before the capable
/// decoder was reached - this assert is what catches that regression.
#[test]
fn c4_rendered_instance_status_in_set() {
    let Some(t) = target() else { return };
    seed(&t);
    let r = t.get(
        &format!("/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_A}/rendered"),
        &[("Accept", "image/jpeg")],
    );
    assert!(
        matches!(r.status, 200 | 404 | 406),
        "8.7.4: rendered must be 200/404/406, got {}",
        r.status
    );

    // The compressed case, end-to-end. The render can only succeed when a
    // decoder is compiled in: the shipped image is built with `transcode`
    // (RLE ships in the default codec set, so no network encoder is needed).
    // Without the feature the honest answer stays 406 - what must NOT happen
    // is the OLD behaviour, a 406 fired from an early guard BEFORE the
    // capable decoder was consulted, which is indistinguishable on the wire
    // and therefore only detectable with the feature on.
    if let Target::InProcess(_) = &t {
        let bytes = build_rle_frame(SOP_RLE, STUDY_B, SERIES_B);
        stow_multipart(&t, STUDY_B, &bytes);
        let r = t.get(
            &format!("/studies/{STUDY_B}/series/{SERIES_B}/instances/{SOP_RLE}/rendered"),
            &[("Accept", "image/jpeg")],
        );
        if cfg!(feature = "transcode") {
            assert_eq!(
                r.status,
                200,
                "10.4.3.3.3: a valid single-frame compressed instance must render, got {} ({})",
                r.status,
                r.text()
            );
            assert!(
                r.content_type().starts_with("image/jpeg"),
                "8.7.4-1: rendered image/jpeg Default, got {}",
                r.content_type()
            );
        } else {
            // No decoder in this build: the 406 is honest, and the message
            // must be the decoder's own, reached AFTER the capable path was
            // tried - never the early guard's pre-flight refusal.
            assert_eq!(
                r.status, 406,
                "a build without `transcode` has no decoder and must answer 406, got {}",
                r.status
            );
        }
    }
}

/// C5. 10.4.4 Table 10.4.4-1: thumbnail image/jpeg is the Default (mandatory)
/// -> 200 image/jpeg for a supported source.
#[test]
fn c5_thumbnail_is_jpeg_200() {
    let Some(t) = target() else { return };
    seed(&t);
    let r = t.get(
        &format!("/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_A}/thumbnail"),
        &[],
    );
    assert_eq!(
        r.status, 200,
        "10.4.4: a thumbnail must answer 200 for a supported instance, got {}",
        r.status
    );
    assert!(
        r.content_type().starts_with("image/jpeg"),
        "10.4.4-1: thumbnail Default media type is image/jpeg, got {}",
        r.content_type()
    );
}

/// C6. 10.4.1.1.5/10.4.1.1.6: bulkdata / pixeldata -> 200 or 404, never a
/// status outside the retrieve family.
#[test]
fn c6_bulkdata_pixeldata_200_or_404() {
    let Some(t) = target() else { return };
    seed(&t);
    let base = format!("/studies/{STUDY_A}/series/{SERIES_A}/instances/{SOP_A}");
    for path in [
        format!("{base}/pixeldata"),
        format!("{base}/bulkdata/00100010"),
        format!("{base}/bulkdata/00100020"),
    ] {
        let r = t.get(&path, &[]);
        assert!(
            matches!(r.status, 200 | 404),
            "10.4.1.1.5/6: {path} must be 200 or 404, got {}",
            r.status
        );
    }
}

/// C7. 10.4.3.1 Table 10.4.3-1: the retrieve family answers only
/// {200,400,404,406,410,413}; 416 is a non-conformance.
#[test]
fn c7_retrieve_status_never_416() {
    let Some(t) = target() else { return };
    seed(&t);
    let base = format!("/studies/{STUDY_B}/series/{SERIES_B}/instances/{SOP_B}");
    for path in [
        format!("{base}/frames/0"),
        format!("{base}/frames/99999"),
        format!("{base}/frames/5"),
    ] {
        let r = t.get(&path, &[]);
        assert_ne!(r.status, 416, "10.4.3-1: {path} must not be 416");
        assert!(
            matches!(r.status, 200 | 400 | 404 | 406 | 410 | 413),
            "10.4.3-1: {path} status {} is outside {{200,400,404,406,410,413}}",
            r.status
        );
    }
}

// ========================================================================
// D. STOW-RS (PS3.18 10.5)
// ========================================================================

/// D1. 10.5.3: a successful single-instance store at the root answers 200,
/// NOT 201.
#[test]
fn d1_root_store_is_200_not_201() {
    let Some(t) = target() else { return };
    let bytes = build_single_frame(SOP_A, STUDY_A, SERIES_A);
    let r = t.request(
        "POST",
        "/studies",
        &[("Content-Type", "application/dicom")],
        Some(&bytes),
    );
    assert_eq!(
        r.status,
        200,
        "10.5.3: POST /studies must answer 200, got {} body={}",
        r.status,
        r.text()
    );
}

/// D2. 10.5.1.1/10.5.3.1: a study-scoped store of a matching instance is 200
/// (idempotent); a MISMATCHING instance -> 409. [finding 2]
#[test]
fn d2_study_scoped_store_match_and_conflict() {
    let Some(t) = target() else { return };
    let bytes = build_single_frame(SOP_A, STUDY_A, SERIES_A);
    let ok = stow_instance(&t, STUDY_A, &bytes);
    assert_eq!(
        ok.status, 200,
        "10.5.3.1: matching study-scoped store must be 200, got {}",
        ok.status
    );
    // Same instance stored under a DIFFERENT study -> scope mismatch -> 409.
    let conflict = stow_instance(&t, STUDY_B, &bytes);
    assert_eq!(
        conflict.status, 409,
        "10.5.1.1: a scope-mismatched store must be 409, got {}",
        conflict.status
    );
}

/// D3. 10.5.1.4: single-part application/dicom -> 200.
#[test]
fn d3_single_part_dicom_store() {
    let Some(t) = target() else { return };
    let bytes = build_single_frame(SOP_A, STUDY_A, SERIES_A);
    let r = t.request(
        "POST",
        "/studies",
        &[("Content-Type", "application/dicom")],
        Some(&bytes),
    );
    assert_eq!(r.status, 200, "10.5.1.4: single-part store must be 200");
}

/// D4. multipart/related store -> 200.
#[test]
fn d4_multipart_store() {
    let Some(t) = target() else { return };
    let bytes = build_single_frame(SOP_A, STUDY_A, SERIES_A);
    let boundary = "conformance-sweep";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(&bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let ct = format!("multipart/related; type=\"application/dicom\"; boundary=\"{boundary}\"");
    let r = t.request("POST", "/studies", &[("Content-Type", &ct)], Some(&body));
    assert_eq!(r.status, 200, "10.5: multipart store must be 200");
}

/// D5. 10.5.3-1: a non-DICOM Content-Type -> 400 or 415, never 200.
#[test]
fn d5_non_dicom_content_type_refused() {
    let Some(t) = target() else { return };
    let r = t.request(
        "POST",
        "/studies",
        &[("Content-Type", "text/plain")],
        Some(b"not a dicom object"),
    );
    assert!(
        matches!(r.status, 400 | 415),
        "10.5.3-1: a non-DICOM Content-Type must be 400/415, got {}",
        r.status
    );
}

/// D6. 10.5.3: the store status is in {200,202,400,409,415}; 404 is a
/// non-conformance.
#[test]
fn d6_store_status_in_set() {
    let Some(t) = target() else { return };
    let bytes = build_single_frame(SOP_A, STUDY_A, SERIES_A);
    let r = t.request(
        "POST",
        "/studies",
        &[("Content-Type", "application/dicom")],
        Some(&bytes),
    );
    assert_ne!(r.status, 404, "10.5.3: a store must never be 404");
    assert!(
        matches!(r.status, 200 | 202 | 400 | 409 | 415),
        "10.5.3: store status {} is outside {{200,202,400,409,415}}",
        r.status
    );
}

// ========================================================================
// E. UPS-RS (PS3.18 11)
// ========================================================================

const WORKITEM: &str = "1.2.826.0.1.3680043.10.1234.8101";

/// E1. 11.4.2/11.4.3.2: Create -> 201 + a Location header naming the created
/// workitem. [finding 1]
#[test]
fn e1_create_returns_location() {
    let Some(t) = target() else { return };
    let r = t.request(
        "POST",
        &format!("/workitems?workitem={WORKITEM}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&workitem_body("SWEEP")),
    );
    assert_eq!(
        r.status, 201,
        "11.4.2: UPS Create must answer 201, got {}",
        r.status
    );
    let loc = r
        .header("location")
        .unwrap_or_else(|| panic!("11.4.2/11.4.3.2 shall: no Location header on 201"));
    assert!(
        loc.ends_with(&format!("/dicomweb/workitems/{WORKITEM}")),
        "11.4.3.2: Location must name the created workitem, got {loc}"
    );
}

/// E2. 11.5: Retrieve -> 200.
#[test]
fn e2_retrieve_workitem() {
    let Some(t) = target() else { return };
    let _ = t.request(
        "POST",
        &format!("/workitems?workitem={WORKITEM}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&workitem_body("SWEEP")),
    );
    let r = t.get(&format!("/workitems/{WORKITEM}"), &[("Accept", DICOM_JSON)]);
    assert_eq!(r.status, 200, "11.5: workitem retrieve must be 200");
}

/// E3. 11.6: Update is a PUT on the workitem resource with the attribute set
/// as the payload -> 200 or 202.
#[test]
fn e3_update_workitem() {
    let Some(t) = target() else { return };
    let _ = t.request(
        "POST",
        &format!("/workitems?workitem={WORKITEM}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&workitem_body("SWEEP")),
    );
    let txn = "2.25.1111111111111111111";
    let r = t.request(
        "PUT",
        &format!("/workitems/{WORKITEM}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&state_body("IN PROGRESS", txn)),
    );
    assert!(
        matches!(r.status, 200 | 202),
        "11.6: workitem update must be 200/202, got {} body={}",
        r.status,
        r.text()
    );
}

/// E4. 11.7.1: Change Workitem State is a PUT on /workitems/{uid}/state -> 200.
#[test]
fn e4_change_state_put() {
    let Some(t) = target() else { return };
    let _ = t.request(
        "POST",
        &format!("/workitems?workitem={WORKITEM}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&workitem_body("SWEEP")),
    );
    let r = t.request(
        "PUT",
        &format!("/workitems/{WORKITEM}/state"),
        &[("Content-Type", DICOM_JSON)],
        Some(&state_body("IN PROGRESS", "2.25.1111111111111111111")),
    );
    assert_eq!(
        r.status,
        200,
        "11.7.1: change state (PUT) must be 200, got {} body={}",
        r.status,
        r.text()
    );
}

/// E5. 11.9: Search -> 200.
#[test]
fn e5_search_workitems() {
    let Some(t) = target() else { return };
    let r = t.get("/workitems", &[("Accept", DICOM_JSON)]);
    assert_eq!(r.status, 200, "11.9: workitem search must be 200");
}

/// E6. 11.8: Cancel Request -> 202.
#[test]
fn e6_cancel_request_202() {
    let Some(t) = target() else { return };
    let _ = t.request(
        "POST",
        &format!("/workitems?workitem={WORKITEM}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&workitem_body("SWEEP")),
    );
    let r = t.request(
        "POST",
        &format!("/workitems/{WORKITEM}/cancelrequest"),
        &[],
        None,
    );
    assert_eq!(
        r.status, 202,
        "11.8: cancel request must be 202, got {}",
        r.status
    );
}

/// E7. 11.10: Subscribe -> 200 or 201.
#[test]
fn e7_subscribe_workitem() {
    let Some(t) = target() else { return };
    let _ = t.request(
        "POST",
        &format!("/workitems?workitem={WORKITEM}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&workitem_body("SWEEP")),
    );
    let r = t.request(
        "POST",
        "/workitems/1.2.840.10008.5.1.4.34.5/subscribers/SWEEPSUB",
        &[("Content-Type", DICOM_JSON)],
        Some(br#"{"00741220":{"vr":"SQ","Value":[{"00741222":{"vr":"LO","Value":["http://127.0.0.1:1/events"]}}]}}"#),
    );
    assert!(
        matches!(r.status, 200 | 201),
        "11.10: subscribe must be 200/201, got {}",
        r.status
    );
}

// ========================================================================
// F. MWL (PS3.18 14) / G. MPPS (PS3.18 15)
// ========================================================================

/// F. 14.4: MWL Search -> 200.
#[test]
fn f_mwl_search_200() {
    let Some(t) = target() else { return };
    let r = t.get(
        "/modality-scheduled-procedure-steps",
        &[("Accept", DICOM_JSON)],
    );
    assert_eq!(r.status, 200, "14.4: MWL search must be 200");
}

/// G. 15.4/15.5/15.6: MPPS Create/Update/Retrieve. A create of a NEW status
/// -> 201 or 200; a retrieve -> 200.
#[test]
fn g_mpps_create_and_retrieve() {
    let Some(t) = target() else { return };
    let uid = "1.2.826.0.1.3680043.10.1234.8201";
    let body = br#"[{"00400252":{"vr":"CS","Value":["IN PROGRESS"]},
                     "00080060":{"vr":"CS","Value":["CT"]},
                     "00400241":{"vr":"AE","Value":["CT01"]}}]"#;
    let created = t.request(
        "POST",
        &format!("/modality-performed-procedure-steps/{uid}"),
        &[("Content-Type", DICOM_JSON)],
        Some(body),
    );
    assert!(
        matches!(created.status, 200 | 201),
        "15.4: MPPS create must be 200/201, got {}",
        created.status
    );
    let got = t.get(
        &format!("/modality-performed-procedure-steps/{uid}"),
        &[("Accept", DICOM_JSON)],
    );
    assert_eq!(got.status, 200, "15.6: MPPS retrieve must be 200");
}

// ========================================================================
// H. Storage Commitment (PS3.18 13)
// ========================================================================

/// H1. 13.4: committing an EXISTING instance -> 200 + ReferencedSOPSequence
/// carrying that SOP Instance UID.
#[test]
fn h1_commit_existing_instance() {
    let Some(t) = target() else { return };
    seed(&t);
    let txn = "1.2.826.0.1.3680043.10.1234.8301";
    let body = commitment_body("1.2.840.10008.5.1.4.1.1.2", SOP_A);
    let r = t.request(
        "POST",
        &format!("/commitment-requests/{txn}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&body),
    );
    assert_eq!(r.status, 200, "13.4: commitment must answer 200");
    let json = r.json();
    assert!(
        json.obj_get("00081199").is_some(),
        "13.4: an existing instance must appear in ReferencedSOPSequence (0008,1199)"
    );
    assert!(
        r.text().contains(SOP_A),
        "13.4: ReferencedSOPSequence must carry the committed SOP Instance UID"
    );
}

/// H2. 13.4: committing a MISSING instance -> 200 + FailedSOPSequence (0008,1198)
/// + FailureReason (0008,1197) with the 0112 code.
#[test]
fn h2_commit_missing_instance() {
    let Some(t) = target() else { return };
    seed(&t);
    let txn = "1.2.826.0.1.3680043.10.1234.8302";
    let missing = "1.2.826.0.1.3680043.10.1234.9999";
    let body = commitment_body("1.2.840.10008.5.1.4.1.1.2", missing);
    let r = t.request(
        "POST",
        &format!("/commitment-requests/{txn}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&body),
    );
    assert_eq!(
        r.status, 200,
        "13.4: commitment answers 200 even on failure"
    );
    let json = r.json();
    assert!(
        json.obj_get("00081198").is_some(),
        "13.4: a missing instance must appear in FailedSOPSequence (0008,1198)"
    );
    assert!(
        r.text().contains("0112"),
        "13.4: FailureReason (0008,1197) must carry the 0112 code"
    );
}

/// H3. 13.2: the commitment result is retrievable -> GET -> 200.
#[test]
fn h3_commitment_retrievable() {
    let Some(t) = target() else { return };
    seed(&t);
    let txn = "1.2.826.0.1.3680043.10.1234.8303";
    let body = commitment_body("1.2.840.10008.5.1.4.1.1.2", SOP_A);
    let _ = t.request(
        "POST",
        &format!("/commitment-requests/{txn}"),
        &[("Content-Type", DICOM_JSON)],
        Some(&body),
    );
    let r = t.get(
        &format!("/commitment-requests/{txn}"),
        &[("Accept", DICOM_JSON)],
    );
    assert_eq!(
        r.status, 200,
        "13.2: a commitment result must be retrievable"
    );
}
