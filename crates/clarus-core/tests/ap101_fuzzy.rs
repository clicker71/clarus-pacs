// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ?: zero-heap gate for the fuzzymatch K-way intersect merge.
//!
//! `intersect_study_uids` (idx/intersect.rs) intersects per-gram study-UID
//! lists into the caller's reused `cursors`/`out` buffers and emits borrowed
//! `&str` slices. A steady-state caller (buffers pre-sized) must therefore
//! allocate NOTHING while merging - the input lists and the borrowed-slice
//! views are prepared OUTSIDE the measured window; only the merge itself is
//! timed.
//!
//! Boundaries (honest): reading the `.idx` files (`ngram_day_rows`) and
//! building `name_ngrams` allocate file/string data - that is OUTSIDE the
//! merge window and not asserted here.
//!
//! TestAllocator is a process-wide `#[global_allocator]`; it works only in an
//! isolated single-threaded binary, so all cases run inside one test function
//! that libtest executes on a single thread.

use clarus_core::idx::intersect::intersect_study_uids;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Run `compute` inside an inline allocation window, asserting zero allocs.
fn assert_zero_alloc(compute: impl FnOnce()) {
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    compute();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    assert_eq!(
        before, after,
        "merge window must not allocate (before: {before:?}, after: {after:?})"
    );
}

/// Borrowed-slice view of the owned test lists (built OUTSIDE the window).
fn views_of(lists: &[Vec<String>]) -> Vec<&[String]> {
    lists.iter().map(|v| v.as_slice()).collect()
}

#[test]
fn intersect_merge_never_allocates() {
    // --- Data prepared OUTSIDE the allocation window ---
    // Single-list passthrough data: one inner list (the K=1 case).
    let single = vec![vec![
        "uid1".to_string(),
        "uid2".to_string(),
        "uid3".to_string(),
        "uid4".to_string(),
    ]];

    // K = 2 with shared + disjoint UIDs, and a duplicate inside list[0].
    let two = vec![
        vec![
            "uid1".to_string(),
            "uid2".to_string(),
            "uid2".to_string(),
            "uid3".to_string(),
            "uid4".to_string(),
        ],
        vec![
            "uid2".to_string(),
            "uid3".to_string(),
            "uid4".to_string(),
            "uid5".to_string(),
        ],
    ];

    // K = 3 with a single common UID.
    let three = vec![
        vec!["a".to_string(), "common".to_string(), "z".to_string()],
        vec!["b".to_string(), "common".to_string(), "y".to_string()],
        vec!["c".to_string(), "common".to_string(), "x".to_string()],
    ];

    let views_single = views_of(&single);
    let views_two = views_of(&two);
    let views_three = views_of(&three);

    // Sized ONCE outside the window: capacity for K up to 3 lists and up to 4
    // emitted UIDs, so the measured window never resizes (never allocates).
    let mut cursors = Vec::new();
    let mut out = Vec::new();
    cursors.reserve(8);
    out.reserve(16);

    // --- Measured window: every case must allocate nothing ---
    assert_zero_alloc(|| {
        // Empty input -> empty result, no allocation.
        intersect_study_uids(&[], None, &mut cursors, &mut out);
        assert_eq!(out.len(), 0);

        // Single-list passthrough, no limit -> all elements (borrowed).
        intersect_study_uids(&views_single, None, &mut cursors, &mut out);
        assert_eq!(out.as_slice(), &["uid1", "uid2", "uid3", "uid4"][..]);

        // Single-list passthrough, limit -> first n elements (borrowed).
        intersect_study_uids(&views_single, Some(2), &mut cursors, &mut out);
        assert_eq!(out.as_slice(), &["uid1", "uid2"][..]);

        // K = 2, common UIDs in ascending order; the in-list duplicate of
        // uid2 must not be emitted twice (borrowed, still no alloc).
        intersect_study_uids(&views_two, None, &mut cursors, &mut out);
        assert_eq!(out.as_slice(), &["uid2", "uid3", "uid4"][..]);

        // K = 2 early-stop: only the first common UID up to the limit.
        intersect_study_uids(&views_two, Some(1), &mut cursors, &mut out);
        assert_eq!(out.as_slice(), &["uid2"][..]);

        // K = 3, one shared UID.
        intersect_study_uids(&views_three, None, &mut cursors, &mut out);
        assert_eq!(out.as_slice(), &["common"][..]);
    });
}
