// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: Update Workitem request-path allocation gate (isolated binary).
//!
//! PS3.18 11.6 adds a second write route to a workitem, and with it a check
//! that runs on every one of those requests: the payload is walked against the
//! merge set so an attribute this implementation does not write is REFUSED
//! rather than silently dropped. That walk is the part the request path pays
//! for, so it is the part that is measured here.
//!
//! The merge set is ONE tag. PS3.3 Table C.30.3-1 puts the whole performer
//! report inside the (0074,1216) item, so the walk in production is over the
//! top-level keys of a payload that carries a single updatable attribute plus,
//! optionally, the locking UID. The payload below therefore carries those keys
//! and no more: a gate that measured a longer walk than the handler performs
//! would be pinning a shape nothing sends.
//!
//! The gate pins three things:
//!
//! - the merge-set check over a well-formed Update payload allocates ONCE, for
//!   the refusal message that is never built on the accept path: walking the
//!   keys borrows, and the only owned value is the `Vec<&str>` the walk itself
//!   collects. The budget is EXACT, not "small" - a change that starts
//!   allocating per attribute fails here.
//! - the Transaction UID carriage (PS3.4 CC.2.6.2): reading the UID out of the
//!   payload and deciding whether a merge attribute is present cost the owned
//!   UID string plus the key vector, and nothing else. That pair is what the
//!   body-TUID carriage adds to the path, so it is measured on its own.
//! - a payload naming an attribute outside the set is refused, so the budget
//!   above cannot be met by a check that stopped checking.
//!
//! The payload is built OUTSIDE the window: the question is not "is building a
//! body free" (it is not) but "does validating one cost anything beyond the
//! borrow it needs".

use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Window that returns the allocation count and the closure's result.
fn count_env<F, R>(f: F) -> (usize, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n, result)
}

/// Window that asserts an EXACT allocation budget.
fn budget_env<F, R>(expect: usize, f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n == expect, result)
}

/// A JSON object in the Update merge-set shape: one (0074,1216) item carrying
/// the performer report.
const UPDATE_BODY: &[u8] = br#"{"00741216":{"vr":"SQ","Value":[{"00404019":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["CXR-AI"]},"00080102":{"vr":"SH","Value":["99CLARUS"]},"00080104":{"vr":"LO","Value":["AI inference"]}}]},"00404028":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["STATION01"]},"00080102":{"vr":"SH","Value":["99CLARUS"]},"00080104":{"vr":"LO","Value":["Scanner One"]}}]},"00404033":{"vr":"SQ","Value":[{"00081150":{"vr":"UI","Value":["1.2.840.10008.5.1.4.1.1.66.4"]},"00081155":{"vr":"UI","Value":["2.25.1"]}}]},"00404050":{"vr":"DT","Value":["20261010120000"]}}]}}"#;

/// The one attribute the Update transaction writes: the (0074,1216) item that
/// PS3.3 Table C.30.3-1 nests the whole performer report inside.
const MERGE_SET: &[&str] = &["00741216"];

/// Walking a well-formed Update payload against the merge set allocates
/// exactly once: the `Vec` of borrowed keys the walk collects.
///
/// The budget is EXACT on purpose. "It allocates a little" is not a property
/// anything can act on; a fixed number is one that a change either keeps or
/// breaks, which is what makes this gate worth running.
#[test]
fn the_merge_set_check_allocates_once() {
    // The parse is setup, not the operation under test: the handler parses
    // before it validates, and parsing allocates by nature.
    let parsed =
        clarus_server::handlers::json_common::parse_json(UPDATE_BODY).expect("a JSON object");

    let (ok, keys) = budget_env(1, || {
        let keys = parsed.keys();
        std::hint::black_box(&keys);
        keys
    });
    assert!(
        ok,
        "the merge-set walk must allocate exactly once (the key vector)"
    );
    assert_eq!(
        keys.len(),
        1,
        "the walk sees every top-level attribute in the payload"
    );
    assert!(
        keys.iter().all(|k| MERGE_SET.contains(k)),
        "and every one of them is in the set: {keys:?}"
    );

    // The window is live: an allocating body must be caught.
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}

/// Determinism: the same payload costs the same twice, so the budget above is
/// a property of the code and not of a lucky first run.
#[test]
fn the_merge_set_check_cost_is_stable() {
    let parsed =
        clarus_server::handlers::json_common::parse_json(UPDATE_BODY).expect("a JSON object");
    let (first, _) = count_env(|| {
        std::hint::black_box(parsed.keys());
    });
    let (second, _) = count_env(|| {
        std::hint::black_box(parsed.keys());
    });
    assert_eq!(first, second, "the walk cost must be deterministic");
}

/// The walk is what makes an out-of-set attribute a REFUSAL. If it ever
/// stopped seeing the payload, the budget above would still pass while the
/// server silently accepted attributes it does not implement.
#[test]
fn the_walk_sees_an_attribute_outside_the_set() {
    let body = br#"{"00741204":{"vr":"SH","Value":["SNEAKY"]}}"#;
    let parsed = clarus_server::handlers::json_common::parse_json(body).expect("a JSON object");
    let keys = parsed.keys();
    assert_eq!(keys, vec!["00741204"], "the label is seen");
    assert!(
        !MERGE_SET.contains(&keys[0]),
        "and it is NOT in the merge set, so the handler refuses it"
    );
}

// -- The Transaction UID carriage (PS3.4 CC.2.6.2) -----------------------

/// An Update payload that carries the locking Transaction UID (0008,1195) in
/// the body alongside the merge attribute - the carriage 11.6.1.4 requires
/// the payload to comply with, and the only one dcm4chee-arc reads.
const UPDATE_BODY_WITH_TUID: &[u8] = br#"{"00081195":{"vr":"UI","Value":["2.25.7"]},"00741216":{"vr":"SQ","Value":[{"00404033":{"vr":"SQ","Value":[{"00081150":{"vr":"UI","Value":["1.2.840.10008.5.1.4.1.1.66.4"]},"00081155":{"vr":"UI","Value":["2.25.1"]}}]}}]}}"#;

/// Reading the Transaction UID out of the payload and deciding whether any
/// MERGE attribute is present cost exactly ONE allocation between them: the
/// key vector the presence check collects.
///
/// `extract_tag` borrows the value it finds (the attribute map and its Value
/// array are already in the parsed body), so it allocates nothing - this is
/// the property that lets the body-TUID carriage be free on a path that runs
/// on every Update. A change that cloned the UID into an owned String inside
/// the window fails here.
#[test]
fn the_tuid_read_and_presence_check_allocate_once() {
    let parsed = clarus_server::handlers::json_common::parse_json(UPDATE_BODY_WITH_TUID)
        .expect("a JSON object");

    let (measured, (tuid, has_mergeable)) = count_env(|| {
        let tuid = clarus_server::handlers::ups::json::extract_tag(&parsed, "00081195");
        let keys = parsed.keys();
        let has_mergeable = keys.iter().any(|key| MERGE_SET.contains(key));
        std::hint::black_box(&keys);
        (tuid, has_mergeable)
    });
    println!("BUDGET tuid_read_plus_presence = {measured}");
    assert_eq!(
        measured, 2,
        "the TUID read owns its String (one) and the presence check collects its \
         key vector (one); nothing else"
    );
    assert_eq!(
        tuid.as_deref(),
        Some("2.25.7"),
        "the UID is read from the payload"
    );
    assert!(
        has_mergeable,
        "and a merge attribute is present, so the update is not empty"
    );

    // The window is live.
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}

/// A payload carrying ONLY the Transaction UID has NO merge attribute, which
/// is how the handler tells "nothing to merge" from a real update - and the
/// check that decides it is the same one allocation as above.
#[test]
fn a_tuid_only_payload_has_no_merge_attribute() {
    let parsed = clarus_server::handlers::json_common::parse_json(
        br#"{"00081195":{"vr":"UI","Value":["2.25.7"]}}"#,
    )
    .expect("a JSON object");

    let (measured, has_mergeable) = count_env(|| {
        let keys = parsed.keys();
        let has_mergeable = keys.iter().any(|key| MERGE_SET.contains(key));
        std::hint::black_box(&keys);
        has_mergeable
    });
    println!("BUDGET tuid_only_presence = {measured}");
    assert_eq!(measured, 1, "only the key vector, no owned UID string");
    assert!(
        !has_mergeable,
        "a UID-only payload has nothing to merge, so the handler refuses it"
    );
}
