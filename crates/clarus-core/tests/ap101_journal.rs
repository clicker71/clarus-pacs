// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! zero-heap guarantee for the CBOR journal SPSC ring write.
//!
//! Pushing a 64-byte token into the ring must not allocate: the token is
//! copied into a preallocated slot. `make_token` (hotpath) must likewise not
//! allocate - it writes into the fixed 64-byte cell and copies a bounded
//! payload slice.
//!
//! # Why a dedicated binary
//!
//! `TestAllocator` is process-wide and requires an isolated single-threaded
//! binary. All checks live in one test function.

use clarus_core::journal::{EventCode, SpscRing, make_token};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

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

/// Push `n` tokens (warm-up or measured) into the ring.
fn push_many(ring: &mut SpscRing, n: usize, payload: &[u8]) {
    for _ in 0..n {
        let t = make_token(
            EventCode::STOW_ACCEPTED,
            0xDEAD_BEEF,
            &[192, 168, 1, 5],
            payload,
        );
        ring.push(t);
    }
}

#[test]
fn journal_spsc_write_never_allocates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _ = dir; // ring lives in RAM; no file needed for the write test.

    // Warm up: ring allocation happens here, outside the measured window.
    let mut ring = SpscRing::new();
    push_many(&mut ring, 8, b"1.2.840.1.2.3");

    // Measured: make_token + push must allocate nothing.
    let (zero_alloc, _) = allocate_env(|| {
        push_many(&mut ring, 4096, b"1.2.840.1.2.3");
    });
    assert!(zero_alloc, "journal SPSC write must not allocate");

    // Full-ring behavior (oldest dropped) must also allocate nothing.
    while !ring_is_full(&ring) {
        let t = make_token(EventCode::STOW_REJECTED, 1, &[0; 16], b"full");
        ring.push(t);
    }
    let (zero_alloc_full, _) = allocate_env(|| {
        let t = make_token(EventCode::STOW_REJECTED, 2, &[0; 16], b"drop");
        ring.push(t);
    });
    assert!(zero_alloc_full, "full-ring push must not allocate");
}

fn ring_is_full(ring: &SpscRing) -> bool {
    use clarus_core::journal::RING_CAPACITY;
    let len = ring.len();
    // len reaches capacity when exactly full.
    len >= RING_CAPACITY
}
