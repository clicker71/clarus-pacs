// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A-009: zero-heap guarantee for the CBOR codec.
//!
//! The codec must never allocate: the encoder writes into a fixed caller
//! buffer and the parser borrows token-by-token from the input slice. This
//! test file enforces that by routing every allocation through
//! [`TestAllocator`] (from `ferrite-testkit`, see ap101) and asserting the
//! guarded windows allocate nothing.
//!
//! # Why the global allocator lives here
//!
//! `TestAllocator` is installed as this test binary's `#[global_allocator]`.
//! The library crate declares none, so no conflict arises (the ap101 reference
//! examples do the same in their own test binaries). See ap101
//! DEVELOPER_GUIDE Section 9 (Common Pitfalls).
//!
//! # Why a single test function
//!
//! `TestAllocator` counts process-wide, so a concurrent sibling test thread
//! could attribute its own allocation to a measured window. Running every
//! A-009 check within one test confines the measurement to a single live
//! libtest thread, which is the deterministic configuration the global
//! allocator supports (multithreaded processes need an isolated process).

use clarus_core::{CborReader, CborToken, CborWriter};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Enable counting, run `f`, disable counting, report whether `f` allocated.
///
/// Counting is switched off everywhere except inside the guarded closure, so
/// unrelated runtime allocations are never attributed to the code under test.
/// The closure must allocate nothing for the before/after snapshots to compare
/// equal.
fn allocate_env<F, R>(f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let result = f();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (before == after, result)
}

/// 32-byte BLAKE3 digest used as a frame hash in the payload.
const FRAME_HASH: [u8; 32] = {
    let mut h = [0u8; 32];
    h[0] = 0xAB;
    h[31] = 0xEF;
    h
};

/// Encode a full 4-record-type manifest payload into `buf`, returning bytes.
///
/// Mirrors the `clarus_core::manifest` shape (system header, study QIDO keys,
/// series, instance, frame). Written with `CborWriter` into a fixed stack
/// buffer - the codec path under test allocates nothing.
fn encode_manifest_payload(buf: &mut [u8]) -> usize {
    let mut w = CborWriter::new(buf);

    w.write_map_header(3).expect("top map");
    // sys
    w.write_u64(1).expect("sys key");
    w.write_map_header(3).expect("sys map");
    w.write_u64(1).expect("ver key");
    w.write_u64(1).expect("ver val");
    w.write_u64(2).expect("tier key");
    w.write_u64(1).expect("tier val");
    w.write_u64(3).expect("bytes key");
    w.write_u64(230_686_720).expect("bytes val");
    // qido study
    w.write_u64(2).expect("study key");
    w.write_map_header(2).expect("study map");
    w.write_u64(0x0008_0020).expect("StudyDate key");
    w.write_u64(20_260_802).expect("StudyDate val");
    w.write_u64(0x0010_0010).expect("PatientName key");
    w.write_text("ivanov").expect("PatientName val");
    // series
    w.write_u64(3).expect("series key");
    w.write_array_header(1).expect("series arr");
    w.write_map_header(2).expect("series map");
    w.write_u64(0x0020_000e).expect("SeriesUID key");
    w.write_text("1.2.840.1.2").expect("SeriesUID val");
    w.write_u64(4).expect("instances key");
    w.write_array_header(1).expect("instances arr");
    // instance: 1 attr (SOPUID) + 5 WADO system fields.
    w.write_map_header(6).expect("instance map");
    w.write_u64(0x0008_0018).expect("SOPUID key");
    w.write_text("1.2.840.1.3").expect("SOPUID val");
    w.write_u64(2).expect("tier key");
    w.write_u64(1).expect("tier val"); // hot
    w.write_u64(5).expect("ts key");
    w.write_text("1.2.840.10008.1.2.1").expect("ts val");
    w.write_u64(7).expect("blake3 key");
    w.write_bytes(&FRAME_HASH).expect("blake3 val");
    w.write_u64(9).expect("file_size key");
    w.write_u64(2_000_000).expect("file_size val");
    w.write_u64(6).expect("frames key");
    w.write_array_header(1).expect("frames arr");
    // frame
    w.write_map_header(4).expect("frame map");
    w.write_u64(1).expect("frame no key");
    w.write_u64(1).expect("frame no val");
    w.write_u64(2).expect("hash key");
    w.write_bytes(&FRAME_HASH).expect("hash val");
    w.write_u64(3).expect("offset key");
    w.write_u64(0).expect("offset val");
    w.write_u64(4).expect("len key");
    w.write_u64(1_059_968).expect("len val");

    w.written()
}

/// Walk `bytes` token-by-token asserting the exact 4-record-type structure.
///
/// Pure assertions - never allocates - so it can run inside a guarded closure.
fn assert_manifest_structure(bytes: &[u8]) {
    let mut r = CborReader::new(bytes);

    macro_rules! next_is {
        ($pat:pat) => {
            assert!(
                matches!(r.next().expect("token"), $pat),
                "unexpected token at {}",
                r.position()
            )
        };
    }

    next_is!(CborToken::MapStart(3));
    // sys
    next_is!(CborToken::UInt(1));
    next_is!(CborToken::MapStart(3));
    next_is!(CborToken::UInt(1));
    next_is!(CborToken::UInt(1));
    next_is!(CborToken::UInt(2));
    next_is!(CborToken::UInt(1));
    next_is!(CborToken::UInt(3));
    next_is!(CborToken::UInt(230_686_720));
    // qido study
    next_is!(CborToken::UInt(2));
    next_is!(CborToken::MapStart(2));
    next_is!(CborToken::UInt(0x0008_0020));
    next_is!(CborToken::UInt(20_260_802));
    next_is!(CborToken::UInt(0x0010_0010));
    next_is!(CborToken::Text("ivanov"));
    // series
    next_is!(CborToken::UInt(3));
    next_is!(CborToken::ArrayStart(1));
    next_is!(CborToken::MapStart(2));
    next_is!(CborToken::UInt(0x0020_000e));
    next_is!(CborToken::Text("1.2.840.1.2"));
    next_is!(CborToken::UInt(4));
    next_is!(CborToken::ArrayStart(1));
    // instance: 1 attr (SOPUID) + 5 WADO system fields.
    next_is!(CborToken::MapStart(6));
    next_is!(CborToken::UInt(0x0008_0018));
    next_is!(CborToken::Text("1.2.840.1.3"));
    next_is!(CborToken::UInt(2));
    next_is!(CborToken::UInt(1)); // tier = hot
    next_is!(CborToken::UInt(5));
    next_is!(CborToken::Text("1.2.840.10008.1.2.1"));
    next_is!(CborToken::UInt(7));
    match r.next().expect("instance blake3") {
        CborToken::Bytes(b) => assert_eq!(b, &FRAME_HASH, "instance blake3 mismatch"),
        other => panic!("expected instance blake3 bytes, got {other:?}"),
    }
    next_is!(CborToken::UInt(9));
    next_is!(CborToken::UInt(2_000_000));
    next_is!(CborToken::UInt(6));
    next_is!(CborToken::ArrayStart(1));
    // frame
    next_is!(CborToken::MapStart(4));
    next_is!(CborToken::UInt(1));
    next_is!(CborToken::UInt(1));
    next_is!(CborToken::UInt(2));
    match r.next().expect("frame hash") {
        CborToken::Bytes(b) => assert_eq!(b, &FRAME_HASH, "frame hash mismatch"),
        other => panic!("expected frame hash bytes, got {other:?}"),
    }
    next_is!(CborToken::UInt(3));
    next_is!(CborToken::UInt(0));
    next_is!(CborToken::UInt(4));
    next_is!(CborToken::UInt(1_059_968));

    assert!(r.is_empty(), "trailing bytes at {}", r.position());
}

#[test]
fn cbor_codec_never_allocates() {
    // `TestAllocator` counts allocations process-wide, so libtest must not run
    // a second test thread during a window. Cargo runs this test file's tests
    // on separate threads; serializing them still leaves the harness thread's
    // bookkeeping free to allocate mid-window. Running every A-009 check in a
    // single test confines the measurement to one live test thread, which is
    // the deterministic configuration the global allocator supports (see ap101
    // DEVELOPER_GUIDE Section 9: multithreaded code needs an isolated process).
    let mut buf = [0u8; 4096]; // fixed stack buffer -- no Vec

    // A-009-1: encoder allocates nothing.
    let _ = encode_manifest_payload(&mut buf);
    let (zero_alloc, written) = allocate_env(|| encode_manifest_payload(&mut buf));
    assert!(zero_alloc, "CBOR encoder must not allocate");
    assert_manifest_structure(&buf[..written]);

    // A-009-2: streaming parser allocates nothing.
    let (zero_alloc, ()) = allocate_env(|| assert_manifest_structure(&buf[..written]));
    assert!(zero_alloc, "CBOR parser must not allocate");

    // A-009-3: encode -> decode round-trips with no allocation.
    let (zero_alloc, written2) = allocate_env(|| {
        let w = encode_manifest_payload(&mut buf);
        // Decode is the inverse of encode: yielding the exact original
        // structure proves the byte stream round-trips without losing or
        // adding any item.
        assert_manifest_structure(&buf[..w]);
        w
    });
    assert!(zero_alloc, "encode+decode must not allocate");
    assert_eq!(written2, written, "roundtrip must reproduce the same bytes");
    assert!(written > 0, "payload must not be empty");

    // CborReader::skip_value() - the shallow-read skip.
    // One pass over the top-level map recursively skips sys, study, series,
    // and every instance/frame record - the same deep skip `decode_study_only`
    // relies on to avoid parsing hundreds of series/instance records. The skip
    // borrows from the input slice and recurses through the reader; it owns no
    // heap memory, so a guarded window must not allocate.
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let mut r = CborReader::new(&buf[..written]);
    let skipped: bool = r.skip_value().is_ok();
    let consumed: bool = r.is_empty();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    assert_eq!(
        before, after,
        "skip_value must not allocate (before: {before:?}, after: {after:?})"
    );
    assert!(
        skipped,
        "skip_value must consume the whole manifest payload"
    );
    assert!(
        consumed,
        "skipping the single top-level map must exhaust the payload at {}",
        r.position()
    );
}
