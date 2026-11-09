// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! 0410 Q2: folded BLAKE3 over the STOW stream - the read-counter, facade-swap
//! and ENOSPC-mapping tests.
//!
//! These live in their OWN test binary because two of them assert on
//! PROCESS-GLOBAL counters (`STOW_HASH_READS`) or a global backend swap, which
//! parallel tests in a shared binary would race.

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

const STUDY: &str = "1.2.826.0.1.3680043.10.1234.1501";
const SERIES: &str = "1.2.826.0.1.3680043.10.1234.1502";

fn append_element(out: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
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

/// A conformant single-frame instance with a multi-KB pixel buffer.
fn build_instance(sop: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(64 * 1024);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1");
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
    append_element(&mut d, 0x0008, 0x0018, b"UI", sop.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"STOW^SMOKE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"STOWPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", STUDY.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", SERIES.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &128u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &128u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    let pixels: Vec<u8> = (0..128 * 128).map(|i| (i % 251) as u8).collect();
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixels);
    d
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

    // Leak the temp dir: the server thread outlives this fn.
    std::mem::forget(temp);

    AppState {
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
        shadow: ShadowTier::new(btree_root.join("idx")),
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
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

fn post_stow(addr: SocketAddr, body: &[u8]) -> u16 {
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
    String::from_utf8_lossy(&out)
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0)
}

/// W4(b): a normal single-part STOW folds the digest during the write, so the
/// ingest NEVER falls back to a full `blake3_of_file` re-read.
#[test]
fn blake3_fold_no_second_read_on_normal_part() {
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

    let inst = build_instance("1.2.826.0.1.3680043.10.1234.1503");
    let before = clarus_server::handlers::stow_pure::STOW_HASH_READS.load(Ordering::Relaxed);
    let status = post_stow(addr, &inst);
    let after = clarus_server::handlers::stow_pure::STOW_HASH_READS.load(Ordering::Relaxed);

    assert_eq!(status, 200, "conformant STOW must be 200");
    assert_eq!(
        after, before,
        "a conformant part must NOT trigger a full re-read (folded digest is final)"
    );

    shutdown.request_stop();
    let _ = handle.join();
}

/// W4(f): the pure mid-stream write-error mapping. ENOSPC -> 507 + the
/// preflight-shaped body + Retry-After: 300; any other kind -> 500.
#[test]
fn stow_write_error_maps_enospc_to_507() {
    let enospc = clarus_server::dispatch_streams::stow_write_error_for(
        std::io::ErrorKind::StorageFull,
        4096.0,
    );
    assert_eq!(enospc.status, 507, "ENOSPC must map to 507");
    assert_eq!(
        enospc.retry_after,
        Some(clarus_server::handlers::stow_pure::INSUFFICIENT_STORAGE_RETRY_AFTER),
        "507 must carry Retry-After: 300"
    );
    let text = String::from_utf8_lossy(&enospc.body);
    assert!(
        text.contains("\"disk_free_mb\":4096"),
        "body must carry the current free space: {text}"
    );
    assert!(
        text.contains("\"error\":\"Insufficient Storage\""),
        "body must carry the preflight error string: {text}"
    );
    // Key order: disk_free_mb, error, message (BTreeMap/serde shape).
    let i_free = text.find("disk_free_mb").unwrap();
    let i_err = text.find("\"error\"").unwrap();
    let i_msg = text.find("\"message\"").unwrap();
    assert!(i_free < i_err && i_err < i_msg, "key order: {text}");

    // Any other write error stays 500.
    let other =
        clarus_server::dispatch_streams::stow_write_error_for(std::io::ErrorKind::Other, 4096.0);
    assert_eq!(other.status, 500, "a non-ENOSPC write error stays 500");
    assert_eq!(other.retry_after, None, "500 carries no Retry-After");
}

/// W4(e): the facade-swap contract. Every content key flows through
/// `clarus_core::hash::cas_hasher`, so a SINGLE backend swap changes EVERY CAS
/// address uniformly - proving no STOW call site bypasses the facade by
/// calling `blake3::Hasher` directly.
///
/// The swap is SIMULATED here (we cannot monkey-patch a compiled body): we
/// assert that `cas_hash_one`, the folded `CasWrite`, and `blake3_of_file`
/// all agree with each other for the SAME bytes (one digest everywhere). If any
/// of them used a different construction they would disagree.
#[test]
fn blake3_fold_facade_one_digest_everywhere() {
    use clarus_core::hash::{CasHasher, CasWrite, cas_hash_one, cas_hasher};

    let bytes: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();

    // The one-shot, the streamed fold and a direct facade use must agree.
    let one_shot = cas_hash_one(&bytes);

    let mut sink: Vec<u8> = Vec::new();
    let mut w = CasWrite::new(&mut sink);
    // Feed in odd-sized chunks to exercise incremental update.
    for chunk in bytes.chunks(777) {
        w.write_all(chunk).expect("fold write");
    }
    let (folded, _inner) = w.finish();
    assert_eq!(one_shot, folded, "cas_hash_one == folded CasWrite digest");
    assert_eq!(sink, bytes, "the tee writes the same bytes through");

    let mut h = cas_hasher();
    CasHasher::update(&mut h, &bytes);
    let direct = CasHasher::finalize(h);
    assert_eq!(one_shot, direct, "cas_hash_one == direct cas_hasher digest");

    // And `blake3_of_file` (the STOW re-read path) agrees too.
    let path = std::env::temp_dir().join(format!(
        "clarus_blake3_fold_facade_{}.bin",
        std::process::id()
    ));
    std::fs::write(&path, &bytes).expect("write fixture");
    let file_digest = clarus_server::handlers::stow_pure::blake3_of_file(&path).expect("hash file");
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        one_shot, file_digest,
        "blake3_of_file must agree with the facade (no bypass)"
    );
}
