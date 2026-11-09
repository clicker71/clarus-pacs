// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: router matching (all methods + 404 + BadUid) is zero-alloc.
//!
//! The router is method-first, manual segment matching over borrowed slices;
//! it must never allocate. TestAllocator guards the window.

use clarus_core::http::parse::Method;
use clarus_core::http::router::{RouterOutcome, match_route};
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

#[test]
fn router_all_methods_404_and_baduid_alloc_zero() {
    let (zero_alloc, ()) = allocate_env(|| {
        // Match across all four methods (route ids are the wire contract).
        assert!(matches!(
            match_route(Method::Get, b"/health"),
            RouterOutcome::Match(_)
        ));
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/studies"),
            RouterOutcome::Match(_)
        ));
        // HEAD goes through the GET table (the curl -I easter-egg path).
        assert!(matches!(
            match_route(Method::Head, b"/dicomweb/studies"),
            RouterOutcome::Match(_)
        ));
        assert!(matches!(
            match_route(Method::Post, b"/dicomweb/studies"),
            RouterOutcome::Match(_)
        ));
        assert!(matches!(
            match_route(Method::Put, b"/dicomweb/workitems/1.2.3.4.5.200"),
            RouterOutcome::Match(_)
        ));
        assert!(matches!(
            match_route(Method::Delete, b"/dicomweb/workitems/1.2.3.4.5.200"),
            RouterOutcome::Match(_)
        ));

        // Unknown path -> NotFound (no allocation).
        assert!(matches!(
            match_route(Method::Get, b"/no/such/route"),
            RouterOutcome::NotFound
        ));

        // Unknown method -> MethodNotAllowed (no allocation).
        assert!(matches!(
            match_route(Method::Unknown, b"/health"),
            RouterOutcome::MethodNotAllowed { .. }
        ));

        // BadUID data (non-[0-9.]) -> BadUid (no allocation).
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/studies/BAD/series"),
            RouterOutcome::BadUid
        ));

        // The digits requirement added for traversal probes is also pure
        // comparison over the borrowed segment: `..` must be rejected here
        // without touching the allocator. This is the hot path for every
        // request, so it is guarded, not assumed.
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/studies/../series"),
            RouterOutcome::BadUid
        ));
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/studies/./series"),
            RouterOutcome::BadUid
        ));
        // A legitimate UID that merely contains dots still matches.
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb/studies/1.2.840.1/series"),
            RouterOutcome::Match(_)
        ));
    });

    assert!(zero_alloc, "router matching must be zero-allocation");
}

/// The QA service root must match the same table as `/dicomweb`, be flagged,
/// and stay zero-allocation: it sits on every QA request, so a `Vec` rewrite
/// of the path would both allocate and dangle the borrowed UIDs.
#[test]
fn qa_root_matches_flagged_and_alloc_zero() {
    let (zero_alloc, ()) = allocate_env(|| {
        // Global resources are reachable under the QA root.
        let out = match_route(Method::Get, b"/dicomweb_qa/studies");
        assert!(matches!(out, RouterOutcome::Match(m) if m.qa && m.id == "qido_studies"));

        // Study/series-scoped resources keep their UIDs, borrowed from the
        // rewritten segment array rather than from a new buffer.
        let out = match_route(Method::Get, b"/dicomweb_qa/studies/1.2.840.1/series");
        match out {
            RouterOutcome::Match(m) => {
                assert!(m.qa);
                assert_eq!(m.id, "qido_series");
                assert_eq!(m.n_uids, 1);
            }
            other => panic!("expected a match, got {other:?}"),
        }

        // The same path on the ordinary root must NOT be flagged.
        let out = match_route(Method::Get, b"/dicomweb/studies");
        assert!(matches!(out, RouterOutcome::Match(m) if !m.qa));

        // A prefix that merely starts with the same bytes is not the QA root.
        assert!(matches!(
            match_route(Method::Get, b"/dicomweb_qax/studies"),
            RouterOutcome::NotFound
        ));

        // Read-only: a store, update or delete on the QA root is unroutable,
        // so no write path can be reached through the expose grant.
        assert!(matches!(
            match_route(Method::Post, b"/dicomweb_qa/studies"),
            RouterOutcome::NotFound
        ));
        assert!(matches!(
            match_route(Method::Put, b"/dicomweb_qa/workitems/1.2.3.4.5.200"),
            RouterOutcome::NotFound
        ));
        assert!(matches!(
            match_route(Method::Delete, b"/dicomweb_qa/workitems/1.2.3.4.5.200"),
            RouterOutcome::NotFound
        ));
    });

    assert!(
        zero_alloc,
        "QA root matching must be zero-allocation (borrowed segment swap)"
    );
}
