// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! 0410: `/rendered` rejects an over-budget instance from the HEADER, BEFORE
//! the whole-blob read.
//!
//! `h_wado_rendered` reads only a bounded header first; the 64M-px pixel
//! budget must fire there and return 413 without ever calling `read_cas_bytes`
//! (the F3 "reject before full read" rule). The same header-time decision
//! covers the compressed instance whose requested format has no decoder
//! (PNG/GIF). This lives in its OWN test binary so the process-global
//! `RENDERED_FULL_READS` counter cannot be moved by a parallel test in the
//! same binary - the assertion is on an exact delta, and a second test in
//! this binary WOULD move it.

use clarus_core::http::conn::Timeouts;
use clarus_core::shutdown::Shutdown;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const STUDY: &str = "1.2.826.0.1.3680043.10.7000.1001";
const SERIES: &str = "1.2.826.0.1.3680043.10.7000.1101";
const SOP: &str = "1.2.826.0.1.3680043.10.7000.1401";

fn push_evrle(out: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    out.extend_from_slice(&group.to_le_bytes());
    out.extend_from_slice(&element.to_le_bytes());
    out.extend_from_slice(vr);
    let extended = matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    );
    if extended {
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    out.extend_from_slice(value);
    if !value.len().is_multiple_of(2) {
        out.push(0x00);
    }
}

/// 8193 x 8193 = 67,118,849 px > the 64M-px cap; tiny PixelData so the cap
/// must fire from the HEADER alone.
fn over_cap_instance() -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_evrle(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    push_evrle(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_evrle(&mut d, 0x0008, 0x0018, b"UI", SOP.as_bytes());
    push_evrle(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_evrle(&mut d, 0x0020, 0x000d, b"UI", STUDY.as_bytes());
    push_evrle(&mut d, 0x0020, 0x000e, b"UI", SERIES.as_bytes());
    push_evrle(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_evrle(&mut d, 0x0028, 0x0010, b"US", &8193u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0011, b"US", &8193u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x7FE0, 0x0010, b"OB", &[0x5a_u8; 16]);
    d
}

/// A COMPRESSED single-frame instance (JPEG Baseline meta TS, SPP=1,
/// NumberOfFrames absent => 1). Small, so the compressed-TS 406 must fire
/// from the MANIFEST transfer syntax BEFORE any whole-blob read.
fn compressed_single_frame() -> Vec<u8> {
    let mut d = Vec::with_capacity(1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_evrle(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.50");
    push_evrle(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_evrle(&mut d, 0x0008, 0x0018, b"UI", SOP3.as_bytes());
    push_evrle(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_evrle(&mut d, 0x0020, 0x000d, b"UI", STUDY.as_bytes());
    push_evrle(&mut d, 0x0020, 0x000e, b"UI", SERIES.as_bytes());
    push_evrle(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_evrle(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    // Encapsulated PixelData (undefined length + a fragment) - the shape of a
    // compressed instance; never decoded here (the 406 fires first).
    let mut pixel = Vec::new();
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    pixel.extend_from_slice(&16u32.to_le_bytes());
    pixel.extend_from_slice(&[0xAB_u8; 16]);
    pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    push_evrle_undefined(&mut d, 0x7FE0, 0x0010, b"OB", &pixel);
    d
}

fn push_evrle_undefined(out: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    out.extend_from_slice(&group.to_le_bytes());
    out.extend_from_slice(&element.to_le_bytes());
    out.extend_from_slice(vr);
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    out.extend_from_slice(value);
}

fn setup() -> AppState {
    let temp = tempfile::tempdir().expect("temp dir");
    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&idx_dir).unwrap();

    let mut config = Config::test_default();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.btree.audit_dir = btree_root.join("audit").to_string_lossy().to_string();
    config.port = 0;
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;
    let storage_dir = temp.path().join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();
    config.storage_dir = storage_dir.to_string_lossy().to_string();

    let cas = clarus_adapters::storage::ContentAddressedStorage::new(temp.path().join("cas"))
        .expect("open cas");

    // Leak the temp dir: the server thread outlives this fn and the process
    // ends when the test does; keeping it alive avoids a mid-test unlink.
    std::mem::forget(temp);

    AppState {
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
        shadow: ShadowTier::new(idx_dir),
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, btree_root.join("idx")),
        config,
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(4),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    }
}

fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

fn seed_stow(addr: SocketAddr, body: &[u8]) {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "POST /dicomweb/studies HTTP/1.1\r\nHost: x\r\nContent-Type: application/dicom\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut full = req.into_bytes();
    full.extend_from_slice(body);
    c.write_all(&full).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&out);
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    // The seed is a single instance with empty Type 2 fields, so the store
    // coerces them and answers 202 "stored with warnings" (PS3.18 10.5.2).
    // The seed still succeeded - that is what this helper is asserting.
    assert!(
        status == 200 || status == 202,
        "STOW seed must accept the instance: {head}"
    );
}

fn get_status(addr: SocketAddr, path: &str, accept: &str) -> u16 {
    get_status_body(addr, path, accept).0
}

/// Like [`get_status`] but also returns the response body (for message asserts).
fn get_status_body(addr: SocketAddr, path: &str, accept: &str) -> (u16, Vec<u8>) {
    let mut c = TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req =
        format!("GET {path} HTTP/1.1\r\nHost: x\r\nAccept: {accept}\r\nConnection: close\r\n\r\n");
    c.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head_end = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(out.len());
    let head = String::from_utf8_lossy(&out[..head_end]);
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, out[head_end..].to_vec())
}

#[test]
fn rendered_over_cap_is_413_without_full_read() {
    let state = Arc::new(setup());
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = {
        let state = Arc::clone(&state);
        let shutdown = shutdown.clone();
        std::thread::spawn(move || {
            clarus_server::serve::serve(
                listener,
                state,
                shutdown,
                Timeouts::from_secs(10, 30, 5, 60),
                64,
            );
        })
    };
    std::thread::sleep(Duration::from_millis(50));

    seed_stow(addr, &over_cap_instance());

    let path = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP}/rendered");
    let before = clarus_server::dispatch_streams::RENDERED_FULL_READS.load(Ordering::Relaxed);
    let status = get_status(addr, &path, "image/png");
    let after = clarus_server::dispatch_streams::RENDERED_FULL_READS.load(Ordering::Relaxed);

    assert_eq!(status, 413, "over-cap rendered instance must be 413");
    assert_eq!(
        after, before,
        "the 413 must fire BEFORE read_cas_bytes (counter moved {before}->{after})"
    );

    // A same-process SECOND request proves the counter still tracks a real
    // whole-blob read on an IN-cap instance (the instrument is live, not
    // simply never incremented).
    seed_stow(addr, &small_mono());
    let path2 = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP2}/rendered");
    let before2 = clarus_server::dispatch_streams::RENDERED_FULL_READS.load(Ordering::Relaxed);
    let status2 = get_status(addr, &path2, "image/jpeg");
    let after2 = clarus_server::dispatch_streams::RENDERED_FULL_READS.load(Ordering::Relaxed);
    assert_eq!(status2, 200, "in-cap mono rendered must render");
    assert_eq!(
        after2,
        before2 + 1,
        "an in-cap request must reach the whole-blob read exactly once"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

const SOP2: &str = "1.2.826.0.1.3680043.10.7000.1402";
const SOP3: &str = "1.2.826.0.1.3680043.10.7000.1403";
/// A small in-cap MONOCHROME2 instance (8x8), to prove the counter is live.
fn small_mono() -> Vec<u8> {
    let mut d = Vec::with_capacity(512);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_evrle(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    push_evrle(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    push_evrle(&mut d, 0x0008, 0x0018, b"UI", SOP2.as_bytes());
    push_evrle(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    push_evrle(&mut d, 0x0020, 0x000d, b"UI", STUDY.as_bytes());
    push_evrle(&mut d, 0x0020, 0x000e, b"UI", SERIES.as_bytes());
    push_evrle(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_evrle(&mut d, 0x0028, 0x0010, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0011, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x7FE0, 0x0010, b"OB", &[0x40_u8; 64]);
    d
}

/// A COMPRESSED single-frame instance whose requested format has no decoder
/// wired (PNG) must 406 from the MANIFEST transfer syntax, BEFORE the whole
/// blob is read. This is the falsification for the dead
/// `extract_tag_value((0002,0010))` guard: with the old code `ts` was empty,
/// the guard never fired, and the compressed bytes were rendered as raw (a 200
/// with garbage).
///
/// The JPEG Accept is not probed here: with the early guard removed it is a
/// build-dependent decision made AFTER the read (200 with a decoder, else 406),
/// so it cannot be asserted without moving the process-global counter this
/// binary exists to measure. The JPEG behaviour lives in `wado_smoke.rs`
/// (`wado_single_frame_rle_instance_rendered_200`) and the sweep's C4.
#[test]
fn rendered_compressed_png_is_406_without_full_read() {
    let state = Arc::new(setup());
    let (listener, addr) = bind();
    let shutdown = Shutdown::new();
    let handle = {
        let state = Arc::clone(&state);
        let sd = shutdown.clone();
        std::thread::spawn(move || {
            clarus_server::serve::serve(
                listener,
                state,
                sd,
                Timeouts::from_secs(10, 30, 5, 60),
                64,
            );
        })
    };
    std::thread::sleep(Duration::from_millis(50));

    seed_stow(addr, &compressed_single_frame());
    let path = format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP3}/rendered");

    let before = clarus_server::dispatch_streams::RENDERED_FULL_READS.load(Ordering::Relaxed);
    let (status, body) = get_status_body(addr, &path, "image/png");
    let after = clarus_server::dispatch_streams::RENDERED_FULL_READS.load(Ordering::Relaxed);

    assert_eq!(
        status, 406,
        "a compressed instance requested as PNG must be 406"
    );
    assert!(
        String::from_utf8_lossy(&body).contains("not wired"),
        "the 406 must say the decode-to-PNG is not wired: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        after, before,
        "the compressed+PNG 406 must fire BEFORE read_cas_bytes (counter moved {before}->{after})"
    );

    shutdown.request_stop();
    let _ = handle.join();
}
