// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the readiness-regression guard costs zero allocations (A1).
//!
//! `regress_readiness_on_new_instances` runs on the re-STOW path, where every
//! durable instance of a study calls it. Its two guard branches return BEFORE
//! any allocation, which is what makes the COMMON per-re-STOW case free: a
//! workitem that is already claimed, or one that is still INCOMPLETE (the
//! ordinary mid-upload case), must not touch the allocator at all.
//!
//! The FLIP branch (the record is SCHEDULED and READY) is event-frequency, not
//! per-instance: it writes a record and is BUDGETED in the coverage matrix
//! rather than gated here.
//!
//! Window discipline (the ap101 pattern): every fixture is built OUTSIDE the
//! window; inside it only the call under test runs.

use clarus_core::ups::WorkItemStore;
use clarus_core::ups::record::WorkItemRecord;
use clarus_server::handlers::cad_workitem::regress_readiness_on_new_instances;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Window that returns the allocation count and the result.
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

/// A record with the two fields the guard reads, and nothing else that
/// matters. Built OUTSIDE every window.
fn record(workflow_state: &str, input_readiness: Option<&str>) -> WorkItemRecord {
    WorkItemRecord {
        workitem_uid: "2.25.1".to_string(),
        workflow_state: workflow_state.to_string(),
        input_readiness: input_readiness.map(str::to_string),
        ..WorkItemRecord::default()
    }
}

/// (a) A workitem that is NOT SCHEDULED returns before allocating.
///
/// A claimed workitem is left exactly as the performer left it, so the guard
/// is the first statement and nothing is read beyond the state.
#[test]
fn regress_guard_claimed_workitem_alloc_zero() {
    let store = WorkItemStore::new(std::env::temp_dir().join("ap101_readiness_a"));
    let existing = record("IN PROGRESS", Some("READY"));

    let (allocs, ()) = count_env(|| {
        regress_readiness_on_new_instances(&store, existing);
    });
    assert_eq!(
        allocs, 0,
        "the not-SCHEDULED guard must return before any allocation (A1)"
    );
}

/// (b) A SCHEDULED workitem that is NOT READY returns before allocating.
///
/// This is the ordinary mid-upload case: the workitem is already INCOMPLETE,
/// so there is nothing to revise and the guard returns. It is the case that
/// runs on every durable instance of a study being uploaded, which is why its
/// cost has to be zero.
#[test]
fn regress_guard_incomplete_workitem_alloc_zero() {
    let store = WorkItemStore::new(std::env::temp_dir().join("ap101_readiness_b"));
    let existing = record("SCHEDULED", Some("INCOMPLETE"));

    let (allocs, ()) = count_env(|| {
        regress_readiness_on_new_instances(&store, existing);
    });
    assert_eq!(
        allocs, 0,
        "the SCHEDULED-but-not-READY guard must return before any allocation (A1)"
    );
}

/// (c) A SCHEDULED workitem with no readiness set also returns before
/// allocating: "unset" is not READY, so it takes the same second guard.
#[test]
fn regress_guard_unset_readiness_alloc_zero() {
    let store = WorkItemStore::new(std::env::temp_dir().join("ap101_readiness_c"));
    let existing = record("SCHEDULED", None);

    let (allocs, ()) = count_env(|| {
        regress_readiness_on_new_instances(&store, existing);
    });
    assert_eq!(
        allocs, 0,
        "an unset readiness is not READY, so the guard returns"
    );
}
