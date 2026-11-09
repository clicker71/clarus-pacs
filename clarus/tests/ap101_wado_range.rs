// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101 (0310 gate G-C): the frame-range serve path is bounded by a TRUSTED
//! cap, never by the untrusted `FrameRecord.compressed_len`.
//!
//! `read_cas_range` reads ONE frame's byte range for `/frames/{N}`. Its `len`
//! argument is `FrameRecord.compressed_len`, which is the encapsulated
//! `item_len` read from the attacker-supplied instance at STOW: a crafted
//! `item_len = 0xFFFFFFFF` stores a 4 GB length, and one GET would otherwise
//! request exactly that many bytes. The runtime half of the systematic rule
//! (report section 1) is this gate, which proves:
//!
//! 1. A valid chunk allocates exactly `compressed_len` bytes - and the buffer
//!    holds the file's bytes at the recorded offset.
//! 2. A `len` above `max_frame_bytes` allocates ZERO bytes: the guard fires
//!    BEFORE the `vec!`, so the 4 GB request never reaches the allocator.
//! 3. A sparse (zero-length) tile stays a legitimately empty frame.
//!
//! The byte budget is asserted with the ap101 `TestAllocator`: the whole
//! window between `reset()` and `snapshot()` must stay at or below the cap.
//! This is the "2 GiB Pi 5 == 1 TiB Xeon" property stated as an arithmetic
//! invariant - the refused read is bounded by the CONFIG, not by the machine.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_core::query::StudyScanner;
use clarus_ports::StorageTier;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use ferrite_testkit::heap::TestAllocator;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Instant;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// The frame length that a crafted `item_len = 0xFFFFFFFF` stores: ~4 GiB,
/// far above every sane cap. The pre-fix code would `vec![0u8; 4294967295]`.
const EVIL_LEN: u64 = 0xFFFF_FFFF;

/// Build a state whose CAS holds one blob of `blob.len()` bytes.
///
/// The temp dir is returned so the caller keeps it alive for the test body.
fn state_with_blob(blob: &[u8]) -> (AppState, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage_dir = temp.path().join("cas");
    std::fs::create_dir_all(&storage_dir).expect("cas dir");
    let btree_root = temp.path().join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
    std::fs::create_dir_all(&idx_dir).expect("idx dir");

    let cas = ContentAddressedStorage::new(storage_dir.clone()).expect("open cas");
    // The CAS key is the BLAKE3 of the bytes; `store_dicom` performs the same
    // hashing + two-level atomic rename `resolve_file` reads back.
    cas.store_dicom(blob, 128 * 1024 * 1024)
        .expect("store blob");

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
        shadow: ShadowTier::new(idx_dir.clone()),
        study_scanner: StudyScanner::new(manifest_dir, idx_dir),
        config,
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(4),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    };
    (state, temp)
}

/// The CAS key (BLAKE3 hex) for a stored blob, so `read_cas_range` can be
/// called with a real address.
fn cas_key(blob: &[u8]) -> String {
    clarus_adapters::hex_encode(blake3::hash(blob).as_bytes())
}

/// A valid chunk reads exactly its length and the bytes are the file's bytes
/// at the recorded offset - the table stays trustworthy.
#[test]
fn valid_chunk_allocates_exactly_compressed_len() {
    // 258 bytes: a header prefix plus the "frame" at offset 64.
    let mut blob = vec![0u8; 258];
    for (i, b) in blob.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let (state, _temp) = state_with_blob(&blob);
    let key = cas_key(&blob);
    let offset = 64u64;
    let len = 100u64;

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let out = clarus_server::dispatch_streams::read_cas_range(
        &state,
        &key,
        StorageTier::Hot,
        offset,
        len,
    )
    .expect("a valid chunk must read");
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    assert_eq!(out.len(), len as usize, "the frame buffer is frame-sized");
    assert_eq!(
        out.as_slice(),
        &blob[offset as usize..(offset + len) as usize],
        "the chunk must be the file bytes at the recorded offset"
    );
    // One allocation of exactly `len` (plus the file open/resolve, which the
    // window includes). The point of the gate is that the TOTAL stays at or
    // below the cap - never proportional to a declared 4 GB.
    let bytes = after.1.saturating_sub(before.1);
    assert!(
        bytes <= len as usize + 8192,
        "a valid {len}-byte chunk must not allocate more than the chunk plus \
         fixed overhead; got {bytes} bytes"
    );
}

/// A `len` above `max_frame_bytes` is refused BEFORE the `vec!`: zero bytes
/// allocated. This is the actual DoS fix - the 4 GB request never reaches the
/// allocator.
#[test]
fn over_cap_len_allocates_zero() {
    // A small blob is enough: the guard runs before any I/O.
    let blob = vec![0u8; 64];
    let (state, _temp) = state_with_blob(&blob);
    let key = cas_key(&blob);
    assert!(
        EVIL_LEN > state.config.max_frame_bytes,
        "the fixture must exceed the cap"
    );

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let err = clarus_server::dispatch_streams::read_cas_range(
        &state,
        &key,
        StorageTier::Hot,
        0,
        EVIL_LEN,
    )
    .expect_err("a frame length above max_frame_bytes must be refused");
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::InvalidInput,
        "the refusal is an InvalidInput, not a read failure"
    );
    let (count, bytes) = (after.0 - before.0, after.1 - before.1);
    assert_eq!(
        bytes, 0,
        "an over-cap frame length must allocate ZERO bytes (got {bytes})"
    );
    assert_eq!(
        count, 0,
        "an over-cap frame length must make ZERO allocations (got {count})"
    );
}

/// A zero-length tile is a legitimately empty frame, not a refused one: the
/// cap must not turn a sparse tile into an error.
#[test]
fn zero_len_chunk_is_an_empty_frame() {
    let blob = vec![0u8; 32];
    let (state, _temp) = state_with_blob(&blob);
    let key = cas_key(&blob);

    let out = clarus_server::dispatch_streams::read_cas_range(&state, &key, StorageTier::Hot, 0, 0)
        .expect("a sparse tile is served, not refused");
    assert!(out.is_empty(), "a zero-length chunk yields an empty buffer");
}

/// The cap value itself is allowed (inclusive bound): an exactly-cap-sized
/// frame must not be rejected.
#[test]
fn exactly_cap_len_is_accepted() {
    // The cap is 64 MiB by default; a real 64 MiB blob would be slow to
    // create, so the state's cap is lowered to the blob size and the boundary
    // is tested as the exact equality.
    let blob = vec![0xABu8; 4096];
    let (mut state, _temp) = state_with_blob(&blob);
    state.config.max_frame_bytes = blob.len() as u64;
    let key = cas_key(&blob);

    let out = clarus_server::dispatch_streams::read_cas_range(
        &state,
        &key,
        StorageTier::Hot,
        0,
        blob.len() as u64,
    )
    .expect("len == cap is inside the bound");
    assert_eq!(out.len(), blob.len());
}

// -- End to end: the fallback cannot be redirected into a whole-blob read -

/// The gate above proves `read_cas_range` refuses. It does NOT prove the
/// caller stopped there: the pre-fix danger was that the refusal fell through
/// to `read_to_end` of the whole instance - the same DoS with a larger
/// multiplier. This test drives the real `/frames/{1}` route with a manifest
/// whose `compressed_len` is `0xFFFFFFFF` and whose instance is larger than
/// the cap, and asserts the answer is 416 (not 200 with a multi-GB body, not
/// a read of the blob).
///
/// The manifest is written directly (encoded CBOR) rather than produced by
/// STOW, because no honest STOW can emit a 4 GB frame length - the point is
/// exactly a manifest an attacker crafted at STOW time.
#[test]
fn over_cap_frame_serves_416_not_a_whole_blob_read() {
    // A blob bigger than the cap we set below, so the fallback guard fires.
    let blob = vec![0x5Au8; 8192];
    let (mut state, _temp) = state_with_blob(&blob);
    state.config.max_frame_bytes = 1024; // 1 KiB: far below the 8 KiB blob
    let key = cas_key(&blob);
    let key_bytes = *blake3::hash(&blob).as_bytes();

    const STUDY: &str = "1.2.826.0.1.3680043.8.498.9000000000000000000000000901";
    const SERIES: &str = "1.2.826.0.1.3680043.8.498.9000000000000000000000000902";
    const SOP: &str = "1.2.826.0.1.3680043.8.498.9000000000000000000000000903";

    let manifest = clarus_core::manifest::Manifest {
        sys: clarus_core::manifest::SysHeader {
            version: 1,
            storage_tier: clarus_core::manifest::TIER_HOT,
            total_pixel_bytes: blob.len() as u64,
            arrived_at: 0,
        },
        study: clarus_core::manifest::StudyQido {
            attrs: vec![(
                clarus_core::query::tag::STUDY_INSTANCE_UID,
                clarus_core::manifest::AttrValue::Text(STUDY.to_string()),
            )],
            series_count: 1,
            instance_count: 1,
            modalities_in_study: Some("SM".to_string()),
        },
        series: vec![clarus_core::manifest::SeriesRecord {
            attrs: vec![
                (
                    clarus_core::query::tag::SERIES_INSTANCE_UID,
                    clarus_core::manifest::AttrValue::Text(SERIES.to_string()),
                ),
                (
                    clarus_core::query::tag::MODALITY,
                    clarus_core::manifest::AttrValue::Text("SM".to_string()),
                ),
            ],
            instances: vec![clarus_core::manifest::InstanceRecord {
                blake3: key_bytes,
                attrs: vec![
                    (
                        clarus_core::query::tag::SOP_INSTANCE_UID,
                        clarus_core::manifest::AttrValue::Text(SOP.to_string()),
                    ),
                    (
                        clarus_core::query::tag::MODALITY,
                        clarus_core::manifest::AttrValue::Text("SM".to_string()),
                    ),
                ],
                transfer_syntax: "1.2.840.10008.1.2.4.50".to_string(),
                storage_tier: clarus_core::manifest::TIER_HOT,
                rejection_reason: 0,
                file_size: blob.len() as u64,
                frames: vec![clarus_core::manifest::FrameRecord {
                    number: 1,
                    hash: [0u8; 32],
                    offset: 0,
                    // The crafted length an attacker's `item_len` produces.
                    compressed_len: EVIL_LEN,
                }],
            }],
        }],
    };

    // Write it where the router looks: <manifest_dir>/<study>.manifest.
    let manifest_dir = std::path::PathBuf::from(&state.config.btree.manifest_dir);
    std::fs::create_dir_all(&manifest_dir).expect("manifest dir");
    let path = clarus_core::stow::manifest_path(&manifest_dir, STUDY);
    std::fs::write(&path, manifest.encode().expect("encode manifest")).expect("write manifest");

    // Real socket: GET the frame from the served route.
    let resp = get_frame_over_socket(&state, STUDY, SERIES, SOP);

    assert_eq!(
        resp, 416,
        "an over-cap frame on a blob above the cap must be 416, never a \
         whole-blob read (status {resp})"
    );
    let _ = key;
}

/// Minimal socket harness: bind, spawn the sync serve loop, GET `/frames/1`,
/// return the status code. Kept local so the gate stays a single binary.
fn get_frame_over_socket(state: &AppState, study: &str, series: &str, sop: &str) -> u16 {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    // `serve` takes the state by Arc; clone the pieces the test needs first.
    let shared = Arc::new(clone_state(state));
    let shutdown = clarus_core::shutdown::Shutdown::new();
    let sd = shutdown.clone();
    let timeouts = clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60);
    std::thread::spawn(move || {
        clarus_server::serve::serve(listener, shared, sd, timeouts, u32::MAX);
    });

    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .expect("timeout");
    let head = format!(
        "GET /dicomweb/studies/{study}/series/{series}/instances/{sop}/frames/1 HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    stream.write_all(head.as_bytes()).expect("write request");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read response");
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    String::from_utf8_lossy(&buf[..head_end])
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status code")
}

/// Rebuild an `AppState` from a borrowed one (the struct is not `Clone`, and
/// the serve loop needs ownership behind an `Arc`).
fn clone_state(state: &AppState) -> AppState {
    AppState {
        cas: clarus_adapters::storage::ContentAddressedStorage::new(
            state.cas.root_dir().to_path_buf(),
        )
        .expect("reopen cas"),
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
        shadow: ShadowTier::new(std::path::PathBuf::from(&state.config.btree.idx_dir)),
        study_scanner: StudyScanner::new(
            std::path::PathBuf::from(&state.config.btree.manifest_dir),
            std::path::PathBuf::from(&state.config.btree.idx_dir),
        ),
        config: state.config.clone(),
        workitems_dir: state.workitems_dir.clone(),
        commitments_dir: state.commitments_dir.clone(),
        transcode_pool: clarus_server::state::TranscodePool::new(4),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    }
}
