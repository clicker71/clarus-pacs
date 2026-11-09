// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! WADO-RS never serves Implicit VR Little Endian.
//!
//! PS3.18 8.7.3: "The Implicit VR Little Endian (1.2.840.10008.1.2) ... shall
//! not be used with Web Services." The sentence is not scoped to images, and
//! Table 8.7.3-2 defaults the Text and Other resource categories to Explicit VR
//! LE as well, so a non-image instance stored as Implicit VR has to be
//! re-encoded before it is served.
//!
//! STOW is untouched: the archive keeps the bytes it was given. Only the SERVE
//! side converts, which is why these tests store an Implicit VR object and then
//! read it back.
//!
//! The fixture is a Surface Segmentation - the SOP class from the 0911 octofhir
//! finding, and the shape that made the pixel pipeline unusable: it has no Rows,
//! no Columns and no PixelData, so `transcode_dicom` cannot decode it and the
//! conversion has to work on the DATASET instead.

#![forbid(unsafe_code)]

use clarus_adapters::codec::ts_uid;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

const STUDY: &str = "1.2.826.0.1.3680043.10.1234.9301";
const SERIES: &str = "1.2.826.0.1.3680043.10.1234.9302";
const SOP: &str = "1.2.826.0.1.3680043.10.1234.9303";

// -- Fixture -------------------------------------------------------------

/// One Explicit VR LE element (the file meta group is always explicit).
fn explicit_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let extended = matches!(
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
    // Every value handed in here is already even-length, so no pad byte is
    // written: a pad that is not counted in the length is the classic way to
    // desynchronise a reader.
    assert_eq!(value.len() % 2, 0, "fixtures pass even-length values");
    if extended {
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    buf.extend_from_slice(value);
}

/// One Implicit VR LE element - tag plus a four-byte length, no VR bytes.
fn implicit_element(buf: &mut Vec<u8>, group: u16, element: u16, value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    assert_eq!(value.len() % 2, 0, "fixtures pass even-length values");
    buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    buf.extend_from_slice(value);
}

fn item(buf: &mut Vec<u8>, body: &[u8]) {
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
    buf.extend_from_slice(&0xE000u16.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

/// A Surface Segmentation stored as Implicit VR LE, with a nested sequence.
///
/// The nested item carries the mesh attributes the prompt names, so the
/// round-trip test can assert that the mesh DATA survived and not merely that
/// some bytes came back. Everything is even-length: the fixture is about the
/// encoding flip, not about padding.
fn implicit_surface_instance() -> Vec<u8> {
    // (0066,0002) Surface Sequence: one item with (0066,0015) Number of
    // Surface Points and (0066,0040) Point Coordinates Data.
    let mut surface_item = Vec::new();
    implicit_element(&mut surface_item, 0x0066, 0x0015, b"3 ");
    implicit_element(
        &mut surface_item,
        0x0066,
        0x0040,
        &[0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00],
    );
    let mut surfaces = Vec::new();
    item(&mut surfaces, &surface_item);

    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    // The File Meta Information group is Explicit VR LE whatever the dataset
    // is - PS3.5 7.1.
    explicit_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2\0");
    implicit_element(&mut d, 0x0008, 0x0016, b"1.2.840.10008.5.1.4.1.1.88.11\0");
    implicit_element(&mut d, 0x0008, 0x0018, b"1.2.826.0.1.3680043.10.1234.9303");
    implicit_element(&mut d, 0x0010, 0x0010, b"SURFACE^PATIENT ");
    implicit_element(&mut d, 0x0010, 0x0020, b"SURFACEPAT01");
    implicit_element(&mut d, 0x0020, 0x000D, b"1.2.826.0.1.3680043.10.1234.9301");
    implicit_element(&mut d, 0x0020, 0x000E, b"1.2.826.0.1.3680043.10.1234.9302");
    implicit_element(&mut d, 0x0008, 0x0060, b"SEG ");
    implicit_element(&mut d, 0x0066, 0x0001, b"1 ");
    implicit_element(&mut d, 0x0066, 0x0002, &surfaces);
    d
}

/// The same object stored as Explicit VR LE: the untouched control.
fn explicit_surface_instance() -> Vec<u8> {
    let mut surface_item = Vec::new();
    explicit_element(&mut surface_item, 0x0066, 0x0015, b"IS", b"3 ");
    explicit_element(
        &mut surface_item,
        0x0066,
        0x0040,
        b"OW",
        &[0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00],
    );
    let mut surfaces = Vec::new();
    item(&mut surfaces, &surface_item);

    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    explicit_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1\0");
    explicit_element(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.88.11\0",
    );
    explicit_element(
        &mut d,
        0x0008,
        0x0018,
        b"UI",
        b"1.2.826.0.1.3680043.10.1234.9304",
    );
    explicit_element(&mut d, 0x0010, 0x0010, b"PN", b"SURFACE^PATIENT ");
    explicit_element(&mut d, 0x0010, 0x0020, b"LO", b"SURFACEPAT02");
    explicit_element(&mut d, 0x0020, 0x000D, b"UI", STUDY.as_bytes());
    explicit_element(&mut d, 0x0020, 0x000E, b"UI", SERIES.as_bytes());
    explicit_element(&mut d, 0x0008, 0x0060, b"CS", b"SEG ");
    explicit_element(&mut d, 0x0066, 0x0001, b"IS", b"1 ");
    explicit_element(&mut d, 0x0066, 0x0002, b"SQ", &surfaces);
    d
}

// -- Server harness ------------------------------------------------------

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

        let mut config = Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;

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
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(state);
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let sd = shutdown.clone();
        let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
        std::thread::spawn(move || {
            clarus_server::serve::serve(listener, state, sd, timeouts, u32::MAX);
        });
        Self { addr, _temp: temp }
    }

    fn store(&self, instance: &[u8]) {
        let r = self.post("/dicomweb/studies", "application/dicom", instance);
        assert!(
            r.status == 200 || r.status == 202,
            "STOW of a Surface Segmentation must be accepted, got {}: {}",
            r.status,
            String::from_utf8_lossy(&r.body)
        );
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        self.request("GET", path, headers, None)
    }

    fn post(&self, path: &str, content_type: &str, body: &[u8]) -> Response {
        self.request("POST", path, &[("Content-Type", content_type)], Some(body))
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("timeout");
        let body = body.unwrap_or(&[]);
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
        stream.write_all(&req).expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read");
        Response::parse(&buf)
    }
}

struct Response {
    status: u16,
    headers: std::collections::HashMap<String, Vec<String>>,
    body: Vec<u8>,
}

impl Response {
    fn parse(buf: &[u8]) -> Self {
        let end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header terminator");
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .expect("status");
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
        let mut body = buf[end + 4..].to_vec();
        let chunked = headers
            .get("transfer-encoding")
            .is_some_and(|v| v.first().is_some_and(|s| s.contains("chunked")));
        if chunked {
            body = decode_chunked(&body).expect("chunked body");
        }
        Self {
            status,
            headers,
            body,
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .and_then(|v| v.first())
            .map(String::as_str)
    }
}

fn decode_chunked(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = data[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .map(|i| pos + i)?;
        let size =
            usize::from_str_radix(std::str::from_utf8(&data[pos..line_end]).ok()?.trim(), 16)
                .ok()?;
        let start = line_end + 2;
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(start..start + size)?);
        pos = start + size + 2;
    }
}

// -- Assertions ----------------------------------------------------------

/// The transfer syntax a DICOM body declares in its own File Meta group.
fn declared_transfer_syntax(instance: &[u8]) -> Option<String> {
    clarus_adapters::transcode::extract_transfer_syntax(instance)
}

/// The first VR byte pair found in a dataset, which tells the two encodings
/// apart without a full parse: an Implicit VR element has no VR at all.
fn dataset_bytes(instance: &[u8]) -> &[u8] {
    instance
        .windows(4)
        .position(|w| w == [0x08, 0x00, 0x16, 0x00])
        .map(|at| &instance[at..])
        .unwrap_or(&[])
}

#[test]
fn an_implicit_vr_instance_is_served_as_explicit_single_part() {
    let srv = TestServer::start();
    srv.store(&implicit_surface_instance());

    let path = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP}");
    let r = srv.get(&path, &[("Accept", "application/dicom")]);
    assert_eq!(r.status, 200, "the instance has to be retrievable");

    let content_type = r.header("content-type").expect("a media type");
    assert!(
        content_type.contains(&format!("transfer-syntax={}", ts_uid::EXPLICIT_VR_LE)),
        "PS3.18 8.7.3 forbids serving Implicit VR over Web Services, so the \
         response has to declare Explicit VR LE, got: {content_type}"
    );
    assert_eq!(
        declared_transfer_syntax(&r.body).as_deref(),
        Some(ts_uid::EXPLICIT_VR_LE),
        "the BODY has to be what the header says it is"
    );
    // The mesh data survives: the conversion rewrites headers only.
    assert!(
        r.body
            .windows(8)
            .any(|w| w == [0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00]),
        "the surface's point coordinates are carried through"
    );
    assert!(
        r.body.windows(15).any(|w| w == b"SURFACE^PATIENT"),
        "the patient name is carried through"
    );
    let dataset = dataset_bytes(&r.body);
    assert_eq!(
        &dataset[4..6],
        b"UI",
        "the first dataset element carries its VR, so the dataset is explicit"
    );
}

#[test]
fn an_implicit_vr_instance_is_served_as_explicit_in_a_multipart_part() {
    let srv = TestServer::start();
    srv.store(&implicit_surface_instance());

    let path = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP}");
    let r = srv.get(
        &path,
        &[("Accept", "multipart/related; type=\"application/dicom\"")],
    );
    assert_eq!(r.status, 200, "the multipart retrieve succeeds");

    let body = String::from_utf8_lossy(&r.body);
    let part_header = body
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-type:"))
        .expect("a part Content-Type");
    assert!(
        part_header.contains(&format!("transfer-syntax={}", ts_uid::EXPLICIT_VR_LE)),
        "the part header has to name the syntax the part's bytes are in, got: {part_header}"
    );

    // The part body starts after the blank line that ends the part headers.
    let part_body_at = r
        .body
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 4)
        .expect("part header terminator");
    let part = &r.body[part_body_at..];
    assert_eq!(
        declared_transfer_syntax(part).as_deref(),
        Some(ts_uid::EXPLICIT_VR_LE),
        "the part's own File Meta has to declare Explicit VR LE"
    );
}

#[test]
fn an_explicit_vr_instance_is_still_served_byte_faithfully() {
    let srv = TestServer::start();
    let stored = explicit_surface_instance();
    srv.store(&stored);

    let path = format!(
        "/dicomweb/studies/{STUDY}/series/{SERIES}/instances/1.2.826.0.1.3680043.10.1234.9304"
    );
    let r = srv.get(&path, &[("Accept", "application/dicom")]);
    assert_eq!(r.status, 200);
    assert_eq!(
        r.body, stored,
        "an Explicit VR instance is streamed as-is: the transcode fires only \
         for Implicit VR"
    );
}
