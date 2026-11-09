// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Zero-heap verification for Bitap fuzzymatch.
//!
//! `clarus_core::query::bitap::bitap_match` runs PatientName fuzzymatch with a
//! Levenshtein-1 threshold. Its hotpath keeps the per-scalar mask in stack
//! arrays (`[u32; 63]` / `[u64; 63]`) and never touches the heap, so both the
//! positive (exact/1-edit) and negative (early-exit) paths must allocate
//! nothing. The only allocating branch is the >63-scalar fallback, which
//! lowercases and uses `str::contains`; the representative queries below are
//! far below that bound and take the stack path.
//!
//! The negative case matters most: the early exit (`r0==0 && r1==0`)
//! must not only return `false` fast but also spend no allocation on the way.

use clarus_core::query::bitap::bitap_match;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Run `compute` inside an inline allocation window, asserting zero allocs.
fn assert_zero_alloc(compute: impl FnOnce() -> bool) -> bool {
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let r = compute();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    assert_eq!(
        before, after,
        "guarded window must not allocate (before: {before:?}, after: {after:?})"
    );
    r
}

#[test]
fn bitap_match_never_allocates() {
    // Positive: exact cyrillic match. Stack-mask path, no allocation.
    let exact_hit = assert_zero_alloc(|| bitap_match("иванов", "иванов"));
    assert!(exact_hit, "exact match must hit");

    // Positive: exact match embedded inside a longer text (the common QIDO
    // case - a query name found within a full `Family^Given` name).
    let embedded_hit = assert_zero_alloc(|| bitap_match("сидоров", "сидоров иван петрович"));
    assert!(embedded_hit, "embedded exact match must hit");

    // Negative: a pattern whose scalar never appears anywhere in the text
    // forces the early exit `r0==0 && r1==0` path - must be fast AND free.
    let early_exit = assert_zero_alloc(|| bitap_match("сидоров", "иванов иван петрович"));
    assert!(!early_exit, "early-exit mismatch must miss");

    // Negative: 2-edit distance is beyond the threshold and must miss.
    let too_far = assert_zero_alloc(|| bitap_match("ивн", "петров"));
    assert!(!too_far, "a >1-edit divergence must miss");
}
