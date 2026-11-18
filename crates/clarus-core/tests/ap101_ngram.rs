// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101 gate: the n-gram torn-tail lock adds no per-call allocation.
//!
//! `ngram::append` holds a striped lock across `repair_torn_newline_tail` and
//! the O_APPEND write so two concurrent appends for the same n-gram/day cannot
//! truncate each other's row. The lock must cost nothing on the STOW path:
//! the stripe table is a preallocated `OnceLock<Vec<Arc<Mutex<()>>>>`, the
//! FNV-1a mix walks bytes, and `Arc::clone` + `Mutex::lock` are zero-alloc.
//! This binary enforces that by measuring the lock directly with `TestAllocator`.
//!
//! # Why a single test function
//!
//! `TestAllocator` counts per-thread; running the check within one function
//! confines the measurement to a single live libtest thread.

use clarus_core::idx::ngram::{self, NameKind};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

#[test]
fn the_ngram_lock_allocates_nothing() {
    let kind = NameKind::Patient;
    let gram = "d0b8d0b2d0b0";
    let day = "20240115";

    // Warm the OnceLock: the 64 stripes are allocated exactly once, outside
    // the measured window.
    ngram::ngram_lock(kind, gram, day);

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    for _ in 0..8 {
        // Look up the stripe, take the mutex, drop the guard - the exact work
        // `append` performs on every call for its lock.
        let stripe = ngram::ngram_lock(kind, gram, day);
        let _guard = stripe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    let (count, bytes) = (after.0 - before.0, after.1 - before.1);
    assert_eq!(
        bytes, 0,
        "taking the n-gram stripe lock must allocate ZERO bytes (got {bytes})"
    );
    assert_eq!(
        count, 0,
        "taking the n-gram stripe lock must make ZERO allocations (got {count})"
    );
}
