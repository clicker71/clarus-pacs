// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101 (0310 gate G-D): the metadata / bulkdata read path is bounded by a
//! TRUSTED header cap, never by the instance size.
//!
//! Before 0310 F3, the metadata and bulkdata routes read the WHOLE instance
//! with `read_cas_bytes` (`read_to_end`). On the 13.09 GB RMS study one WADO
//! metadata GET therefore allocated 13 GB, and the same instance could be
//! fetched repeatedly for a handful of JSON fields. The fix introduces
//! `read_cas_head(max)`, whose `max` is `max_part_header` from the CONFIG, not
//! the untrusted instance size. This gate proves the runtime property:
//!
//! 1. A header read of a multi-GB-sized blob allocates at most `max` bytes -
//!    the byte window stays bounded by the trusted cap, never the file size.
//! 2. The returned buffer is a PREFIX of the file (the head, not a splice).
//! 3. A file shorter than `max` returns the whole file (the short case still
//!    works - metadata for a small instance is unchanged).
//!
//! The invariant is the "2 GiB Pi 5 == 1 TiB Xeon" property: the read is
//! bounded by the CONFIG, so the same request costs the same on both machines.

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

/// Build a state whose CAS holds one blob. The temp dir is returned so the
/// caller keeps it alive for the test body.
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
    cas.store_dicom(blob, 256 * 1024 * 1024)
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

fn cas_key(blob: &[u8]) -> String {
    clarus_adapters::hex_encode(blake3::hash(blob).as_bytes())
}

/// The header read of a large instance allocates at most `max` bytes, and the
/// window is bounded by the CONFIG cap, never by the file size.
#[test]
fn header_read_is_bounded_by_max_not_instance_size() {
    // An 8 MiB blob: far larger than the 16 KiB header cap the test uses.
    let blob = vec![0x5Au8; 8 * 1024 * 1024];
    let (state, _temp) = state_with_blob(&blob);
    let key = cas_key(&blob);
    let max = 16 * 1024u64;
    assert!(
        (blob.len() as u64) > max,
        "the fixture must exceed the header cap"
    );

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let head = clarus_server::dispatch_streams::read_cas_head(&state, &key, StorageTier::Hot, max)
        .expect("a header read of a larger instance must succeed");
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    assert_eq!(head.len(), max as usize, "the read is exactly `max` bytes");
    assert_eq!(
        head.as_slice(),
        &blob[..max as usize],
        "the header read is the file PREFIX, not a splice"
    );
    // The window includes the file open/resolve; the point is that it stays at
    // or below the header cap plus fixed overhead - NOT proportional to the
    // 8 MiB instance.
    let bytes = after.1.saturating_sub(before.1);
    assert!(
        bytes <= max as usize + 8192,
        "a {max}-byte header read must not allocate more than the cap plus \
         fixed overhead; got {bytes} bytes (instance is {} bytes)",
        blob.len()
    );
}

/// A file shorter than `max` returns the whole file: the small-instance
/// metadata path is unchanged.
#[test]
fn short_file_returns_whole_head() {
    let blob = vec![0x11u8; 2048];
    let (state, _temp) = state_with_blob(&blob);
    let key = cas_key(&blob);

    let head =
        clarus_server::dispatch_streams::read_cas_head(&state, &key, StorageTier::Hot, 1 << 20)
            .expect("a short file reads fully");
    assert_eq!(head.len(), blob.len(), "a short file returns all of it");
    assert_eq!(head.as_slice(), blob.as_slice());
}

/// The header read never grows with the instance: two instances an order of
/// magnitude apart produce the SAME byte window.
#[test]
fn header_window_is_independent_of_instance_size() {
    let small = vec![0x22u8; 512 * 1024];
    let big = vec![0x33u8; 4 * 1024 * 1024];
    let max = 4096u64;

    let (s_state, _s_temp) = state_with_blob(&small);
    let (b_state, _b_temp) = state_with_blob(&big);

    let measure = |state: &AppState, key: &str| -> usize {
        ALLOC.set_enabled(false);
        ALLOC.reset();
        ALLOC.set_enabled(true);
        let before = ALLOC.snapshot();
        let out = clarus_server::dispatch_streams::read_cas_head(state, key, StorageTier::Hot, max)
            .expect("head read");
        let after = ALLOC.snapshot();
        ALLOC.set_enabled(false);
        assert_eq!(out.len(), max as usize);
        after.1.saturating_sub(before.1)
    };

    let s_bytes = measure(&s_state, &cas_key(&small));
    let b_bytes = measure(&b_state, &cas_key(&big));
    // Both windows are the header cap plus fixed I/O overhead; the 8x larger
    // instance must not shift the window by anything proportional to it.
    let delta = b_bytes.abs_diff(s_bytes);
    assert!(
        delta <= 8192,
        "the header window must not track instance size: small={s_bytes}, \
         big={b_bytes}"
    );
}

/// Build a Part-10 DICOM file with a value-length element placed AFTER a large
/// filler, so it lands past any head-sized prefix. The filler is a private
/// element with an even value length (no pad needed).
fn dcm_with_late_value(late: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 128];
    buf.extend_from_slice(b"DICM");
    // Meta group element (0002,0010) UI - lets the walk skip group 0002.
    let uid = b"1.2.840.10008.1.2";
    buf.extend_from_slice(&0x0002u16.to_le_bytes());
    buf.extend_from_slice(&0x0010u16.to_le_bytes());
    buf.extend_from_slice(b"UI");
    buf.extend_from_slice(&(uid.len() as u16).to_le_bytes());
    buf.extend_from_slice(uid);
    if !uid.len().is_multiple_of(2) {
        buf.push(0x00); // PS3.5 7.1 value pad
    }
    // A large private OB filler (extended VR, 4-byte length).
    let filler = vec![0x00u8; 128 * 1024];
    buf.extend_from_slice(&0x0009u16.to_le_bytes());
    buf.extend_from_slice(&0x1001u16.to_le_bytes());
    buf.extend_from_slice(b"OB");
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&(filler.len() as u32).to_le_bytes());
    buf.extend_from_slice(&filler);
    // The late LUT (0028,1101) OB.
    buf.extend_from_slice(&0x0028u16.to_le_bytes());
    buf.extend_from_slice(&0x1101u16.to_le_bytes());
    buf.extend_from_slice(b"OB");
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&(late.len() as u32).to_le_bytes());
    buf.extend_from_slice(late);
    buf
}

/// A bulkdata request for a tag PAST the head on a large instance performs a
/// BOUNDED read: the allocation tracks the tag value, not the instance size.
#[test]
fn bulkdata_past_head_reads_only_the_tag_value() {
    // ~1 MiB instance with the LUT late (past a 16 KiB head).
    let lut = vec![0x7Cu8; 4096];
    let blob = dcm_with_late_value(&lut);
    let (state, _temp) = state_with_blob(&blob);
    let key = cas_key(&blob);
    let path = state.cas.cas_path(&key);
    assert!(
        (blob.len() as usize) > 16 * 1024,
        "the fixture must exceed the head"
    );

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let got = clarus_adapters::dicom_rs::frame::find_tag_value_in_file(
        &path,
        (0x0028, 0x1101),
        state.config.max_frame_bytes as usize,
    )
    .expect("the late tag is found by the seek-based read");
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    assert_eq!(got, lut, "the seek-based read serves exactly the tag value");
    let bytes = after.1.saturating_sub(before.1);
    assert!(
        bytes <= lut.len() + 8192,
        "a bulkdata read must allocate about the tag value ({}) plus fixed \
         overhead, not the {} byte instance; got {bytes}",
        lut.len(),
        blob.len()
    );
}

/// A huge DECLARED value length on a bulkdata tag allocates ZERO bytes and
/// returns empty (the cap fires before the `vec!`).
#[test]
fn bulkdata_over_cap_declared_length_allocates_zero() {
    // A hand-built file whose target tag declares a 1 GiB value but has no
    // such bytes: the guard must refuse before any allocation.
    let mut buf = vec![0u8; 128];
    buf.extend_from_slice(b"DICM");
    buf.extend_from_slice(&0x0028u16.to_le_bytes());
    buf.extend_from_slice(&0x1101u16.to_le_bytes());
    buf.extend_from_slice(b"OB");
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&0x4000_0000u32.to_le_bytes()); // 1 GiB declared
    // (No value bytes follow - the read must never be attempted.)

    let (state, _temp) = state_with_blob(&buf);
    let key = cas_key(&buf);
    let path = state.cas.cas_path(&key);

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let got = clarus_adapters::dicom_rs::frame::find_tag_value_in_file(
        &path,
        (0x0028, 0x1101),
        state.config.max_frame_bytes as usize,
    );
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    assert!(got.is_none(), "an over-cap declared length must be refused");
    // The cap fires BEFORE the value buffer is sized: the only allocation in
    // the window is the fixed file-open overhead, never anything proportional
    // to the declared 1 GiB. (Neutering the cap allocates the full length.)
    let bytes = after.1 - before.1;
    assert!(
        bytes <= 8192,
        "an over-cap length must not size a value buffer; got {bytes} bytes \
         (the declared length is 1 GiB)"
    );
}
