// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Frame-chunk table: STOW writes it, `/frames/{N}` serves from it.
//!
//! The manifest frame-chunk table (`InstanceRecord.frames`, populated at STOW)
//! exists so `/frames/{N}` reads ONLY that frame's byte range instead of
//! buffering the whole instance. These tests pin the contract end to end over a
//! real socket:
//!
//! 1. STOW of a multi-frame encapsulated instance populates the table.
//! 2. The served frame bytes are BYTE-IDENTICAL to the frame inside the source
//!    file - the property that makes the offset table trustworthy.
//! 3. The table makes a sparse tile an empty frame (not a missing one).
//! 4. A single-frame instance carries NO table (the whole-blob path is right).
//! 5. The read path allocates the FRAME, not the instance (ap101 gate).
//!
//! The instances are built here rather than read from the corpus because the
//! corpus has no multi-frame encapsulated file; the builder is deterministic so
//! the expectations are exact, and the same file is fed to STOW and to the
//! byte-comparison reference.

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

const CT: &str = "1.2.840.10008.5.1.4.1.1.2";
/// JPEG Baseline - an ENCAPSULATED syntax, so the frame table must come from
/// the item headers (the native contiguous-frame branch does not apply).
const JPEG_BASELINE: &str = "1.2.840.10008.1.2.4.50";
const EVRLE: &str = "1.2.840.10008.1.2.1";
/// VL Whole Slide Microscopy Image Storage - the WSI SOP class.
const WSI_SOP: &str = "1.2.840.10008.5.1.4.1.1.77.1.6";

// -- Test server harness (mirror of wado_multipart_smoke.rs) --------------

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
            study_scanner: StudyScanner::new(manifest_dir.clone(), idx_dir.clone()),
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

        Self { addr, _temp: temp }
    }

    fn get(&self, path: &str) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            self.addr.port()
        );
        stream.write_all(head.as_bytes()).expect("write req");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read resp");
        parse_http(&buf)
    }

    fn post_single(&self, path: &str, body: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/dicom\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
        .expect("status code");
    Response {
        status,
        body,
        headers: head,
    }
}

struct Response {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Response {
    /// The boundary declared in the response Content-Type.
    fn boundary(&self) -> String {
        let ct = self
            .headers
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
            .map(|l| l.split_once(':').map(|(_, v)| v.trim()).unwrap_or(""))
            .expect("Content-Type header");
        let idx = ct.find("boundary=").expect("multipart boundary");
        ct[idx + "boundary=".len()..]
            .trim_matches('"')
            .trim_end_matches(';')
            .trim()
            .to_string()
    }

    /// The single part payload of a `multipart/related` frames response.
    ///
    /// The body is:
    /// `--B\r\n<part headers>\r\n\r\n<part bytes>\r\n--B--\r\n`
    /// so the part is everything between the blank line and the CRLF that
    /// precedes the closing `--B`. Taking the boundary from Content-Type (not
    /// a byte search) is what makes this unambiguous.
    fn multipart_part_body(&self) -> Vec<u8> {
        assert!(
            self.headers
                .lines()
                .any(|l| l.to_ascii_lowercase().starts_with("content-type:")
                    && l.contains("multipart/related")),
            "frames response must be multipart/related (dicomweb-client decodes it unconditionally)"
        );
        let boundary = self.boundary();
        let open = format!("--{boundary}\r\n");
        let close = format!("\r\n--{boundary}--");
        let start =
            find_subslice(&self.body, open.as_bytes()).expect("opening boundary") + open.len();
        // Skip the part headers: the bytes after the first blank line.
        let part_start =
            start + find_subslice(&self.body[start..], b"\r\n\r\n").expect("part header end") + 4;
        let part_end = find_subslice(&self.body[part_start..], close.as_bytes())
            .expect("closing boundary")
            + part_start;
        self.body[part_start..part_end].to_vec()
    }
}

/// First index of `needle` inside `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// -- DICOM builder -------------------------------------------------------

fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let extended = matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UN" | b"UC" | b"UR" | b"UT"
    );
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

/// Build the encapsulated PixelData value: an empty Basic Offset Table
/// followed by one item per frame, then the sequence delimiter. This is the
/// exact PS3.5 Table A.4-1 shape, and it is also the reference the byte
/// comparison uses.
fn encapsulated_pixel_data(frames: &[&[u8]]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    for frame in frames {
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        buf.extend_from_slice(frame);
    }
    buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    buf
}

/// Identity + geometry of a synthetic multi-frame instance under test.
struct InstanceSpec<'a> {
    sop_uid: &'a str,
    ts: &'a str,
    sop_class: &'a str,
    modality: &'a str,
    rows: u16,
    cols: u16,
    bits: u16,
    frames: u32,
}

fn build_multiframe(spec: &InstanceSpec<'_>, pixel_data: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(pixel_data.len() + 2048);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", spec.ts.as_bytes());
    append_element(&mut d, 0x0008, 0x0016, b"UI", spec.sop_class.as_bytes());
    append_element(&mut d, 0x0008, 0x0018, b"UI", spec.sop_uid.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"FRAME^CHUNK");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"FRAMEPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", STUDY.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", SERIES.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", spec.modality.as_bytes());
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &spec.rows.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &spec.cols.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &spec.bits.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &spec.bits.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(
        &mut d,
        0x0028,
        0x0008,
        b"IS",
        spec.frames.to_string().as_bytes(),
    );
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", pixel_data);
    d
}

const STUDY: &str = "1.2.826.0.1.3680043.8.498.9000000000000000000000000001";
const SERIES: &str = "1.2.826.0.1.3680043.8.498.9000000000000000000000000002";

fn wsi_tiles() -> Vec<Vec<u8>> {
    // Distinct, non-uniform tile payloads so an off-by-one offset in the table
    // cannot accidentally compare equal.
    vec![
        vec![0xA1; 300],
        vec![0xB2; 512],
        vec![0xC3; 128],
        vec![0xD4; 777],
        vec![0xE5; 64],
    ]
}

/// The manifest for a study, decoded, to inspect the frame table.
fn read_manifest(server: &TestServer, study_uid: &str) -> clarus_core::manifest::Manifest {
    let _ = server;
    // The harness keeps the manifest under the temp dir; locate it by walking.
    let dir = server._temp.path().join("btree").join("manifests");
    let path = clarus_core::stow::manifest_path(&dir, study_uid);
    let bytes = std::fs::read(&path).expect("manifest file");
    clarus_core::manifest::Manifest::decode(&bytes).expect("decode manifest")
}

fn find_instance_frames(
    m: &clarus_core::manifest::Manifest,
    sop_uid: &str,
) -> Vec<clarus_core::manifest::FrameRecord> {
    m.series
        .iter()
        .flat_map(|s| &s.instances)
        .find(|i| {
            i.attrs.iter().any(|(t, v)| {
                *t == clarus_core::query::tag::SOP_INSTANCE_UID
                    && matches!(v, clarus_core::manifest::AttrValue::Text(u) if u == sop_uid)
            })
        })
        .map(|i| i.frames.clone())
        .expect("instance present")
}

// -- 1. STOW populates the table ----------------------------------------

#[test]
fn stow_populates_frame_chunk_table_and_serves_matching_bytes() {
    let server = TestServer::start();
    let tiles = wsi_tiles();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let pixel = encapsulated_pixel_data(&refs);
    let sop = "1.2.826.0.1.3680043.8.498.9000000000000000000000000010";
    let file = build_multiframe(
        &InstanceSpec {
            sop_uid: sop,
            ts: JPEG_BASELINE,
            sop_class: WSI_SOP,
            modality: "SM",
            rows: 256,
            cols: 256,
            bits: 8,
            frames: tiles.len() as u32,
        },
        &pixel,
    );

    let stow = server.post_single("/dicomweb/studies", &file);
    assert!(
        stow.status == 200 || stow.status == 202,
        "STOW status {}",
        stow.status
    );

    // The table is written with one entry per frame.
    let m = read_manifest(&server, STUDY);
    let frames = find_instance_frames(&m, sop);
    assert_eq!(
        frames.len(),
        tiles.len(),
        "one frame-chunk per frame, got {frames:?}"
    );
    for (i, rec) in frames.iter().enumerate() {
        assert_eq!(rec.number, (i + 1) as u32, "1-based frame number");
    }

    // And every frame the server serves is byte-identical to the source tile.
    for (i, expected) in tiles.iter().enumerate() {
        let n = i + 1;
        let resp = server.get(&format!(
            "/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{sop}/frames/{n}"
        ));
        assert_eq!(resp.status, 200, "frame {n} status");
        let part = resp.multipart_part_body();
        assert_eq!(
            part.as_slice(),
            expected.as_slice(),
            "frame {n} bytes must equal the source tile exactly"
        );
    }
}

// -- 2. The table describes the served blob -----------------------------

#[test]
fn frame_table_offsets_point_at_the_served_frames() {
    let server = TestServer::start();
    let tiles = wsi_tiles();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let pixel = encapsulated_pixel_data(&refs);
    let sop = "1.2.826.0.1.3680043.8.498.9000000000000000000000000011";
    let file = build_multiframe(
        &InstanceSpec {
            sop_uid: sop,
            ts: JPEG_BASELINE,
            sop_class: WSI_SOP,
            modality: "SM",
            rows: 256,
            cols: 256,
            bits: 8,
            frames: tiles.len() as u32,
        },
        &pixel,
    );

    server.post_single("/dicomweb/studies", &file);
    let m = read_manifest(&server, STUDY);
    let frames = find_instance_frames(&m, sop);

    // The decisive property: slicing the SOURCE file by each recorded chunk
    // reproduces that tile. This is what proves the offsets are file-absolute
    // and that the serve path reads the right bytes.
    for (rec, expected) in frames.iter().zip(tiles.iter()) {
        let start = rec.offset as usize;
        let end = start + rec.compressed_len as usize;
        assert!(end <= file.len(), "chunk must lie inside the file");
        assert_eq!(
            &file[start..end],
            expected.as_slice(),
            "file slice at the recorded offset must be frame {}",
            rec.number
        );
    }
}

// -- 3. Sparse tile is an empty frame, not a missing one ----------------

#[test]
fn sparse_tile_serves_as_empty_frame() {
    let server = TestServer::start();
    let t1: &[u8] = &[0x11; 200];
    let t2: &[u8] = &[]; // sparse
    let t3: &[u8] = &[0x33; 150];
    let pixel = encapsulated_pixel_data(&[t1, t2, t3]);
    let sop = "1.2.826.0.1.3680043.8.498.9000000000000000000000000012";
    let file = build_multiframe(
        &InstanceSpec {
            sop_uid: sop,
            ts: JPEG_BASELINE,
            sop_class: WSI_SOP,
            modality: "SM",
            rows: 256,
            cols: 256,
            bits: 8,
            frames: 3,
        },
        &pixel,
    );

    server.post_single("/dicomweb/studies", &file);

    let m = read_manifest(&server, STUDY);
    let frames = find_instance_frames(&m, sop);
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[1].compressed_len, 0, "sparse tile has zero length");

    let resp = server.get(&format!(
        "/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{sop}/frames/2"
    ));
    assert_eq!(resp.status, 200, "a sparse tile is served, not 404/416");
    assert_eq!(
        resp.multipart_part_body().as_slice(),
        b"",
        "sparse tile body is empty"
    );

    // A frame beyond NumberOfFrames is an absent Target Resource -> 404
    // (PS3.18 10.4.3.1 Table 10.4.3-1), not 416 (that is the byte-Range
    // HEADER mechanism).
    let oob = server.get(&format!(
        "/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{sop}/frames/9"
    ));
    assert_eq!(oob.status, 404, "a frame beyond NumberOfFrames is 404");
}

// -- 4. Single-frame instance carries no table --------------------------

#[test]
fn single_frame_instance_has_no_frame_table() {
    let server = TestServer::start();
    let sop = "1.2.826.0.1.3680043.8.498.9000000000000000000000000013";
    // A plain uncompressed single-frame CT.
    let mut d = Vec::new();
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", EVRLE.as_bytes());
    append_element(&mut d, 0x0008, 0x0016, b"UI", CT.as_bytes());
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"FRAME^CHUNK");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"FRAMEPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", STUDY.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", SERIES.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &[0x7F; 64]);

    server.post_single("/dicomweb/studies", &d);
    let m = read_manifest(&server, STUDY);
    assert!(
        find_instance_frames(&m, sop).is_empty(),
        "a single-frame instance must not carry a frame table"
    );

    // Single-frame instances still serve (whole-blob path) - the change must
    // not regress them.
    let resp = server.get(&format!(
        "/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{sop}/frames/1"
    ));
    assert_eq!(resp.status, 200);
    assert_eq!(resp.multipart_part_body().len(), 64);
}

// -- 5. ap101: the read path allocates the frame, not the instance -------

#[test]
fn frame_table_ranges_are_frame_sized_not_instance_sized() {
    // The ap101 property, stated as an arithmetic invariant on the table the
    // serve path uses: the sum of the chunk lengths is far below the instance
    // size, and each chunk is a small fraction of it. A whole-blob read would
    // touch every byte; a chunk read touches only its own range.
    let server = TestServer::start();
    // ~40 frames of ~64 KiB: a "big instance, small tile" shape.
    let tiles: Vec<Vec<u8>> = (0..40u8).map(|i| vec![i; 65_536]).collect();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let pixel = encapsulated_pixel_data(&refs);
    let sop = "1.2.826.0.1.3680043.8.498.9000000000000000000000000014";
    let file = build_multiframe(
        &InstanceSpec {
            sop_uid: sop,
            ts: JPEG_BASELINE,
            sop_class: WSI_SOP,
            modality: "SM",
            rows: 256,
            cols: 256,
            bits: 8,
            frames: tiles.len() as u32,
        },
        &pixel,
    );
    server.post_single("/dicomweb/studies", &file);

    let m = read_manifest(&server, STUDY);
    let frames = find_instance_frames(&m, sop);
    assert_eq!(frames.len(), 40);

    let instance_bytes = std::fs::metadata(manifest_blob_path(&server, &m, sop))
        .map(|md| md.len())
        .unwrap_or(file.len() as u64);
    let max_chunk = frames.iter().map(|f| f.compressed_len).max().unwrap();
    assert!(
        max_chunk * 4 < instance_bytes,
        "one tile ({max_chunk} B) must be a small fraction of the instance ({instance_bytes} B)"
    );

    // And the served tile is exactly the chunk length, not the instance length.
    let resp = server.get(&format!(
        "/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{sop}/frames/1"
    ));
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.multipart_part_body().len() as u64,
        frames[0].compressed_len
    );
}

/// Path of the stored CAS blob for an instance (used only to size the
/// instance in the ap101 invariant test).
fn manifest_blob_path(
    server: &TestServer,
    m: &clarus_core::manifest::Manifest,
    sop_uid: &str,
) -> PathBuf {
    let key = m
        .series
        .iter()
        .flat_map(|s| &s.instances)
        .find(|i| {
            i.attrs.iter().any(|(t, v)| {
                *t == clarus_core::query::tag::SOP_INSTANCE_UID
                    && matches!(v, clarus_core::manifest::AttrValue::Text(u) if u == sop_uid)
            })
        })
        .map(|i| clarus_adapters::hex_encode(&i.blake3))
        .expect("instance");
    server
        ._temp
        .path()
        .join("cas")
        .join(&key[0..2])
        .join(&key[2..4])
        .join(format!("{key}.dcm"))
}
