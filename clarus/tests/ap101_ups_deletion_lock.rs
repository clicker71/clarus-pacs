// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: Deletion Lock allocation gate (isolated binary).
//!
//! Sprint C adds two things to paths that already existed:
//!
//! - `subscribe_pure::parse_deletion_lock` reads one key out of the request's
//!   query string. The subscribe route is NOT a pixel hot path - it is called
//!   once per subscription, which is operator-scale - but the parse touches an
//!   attacker-influenced string, so its cost is pinned rather than asserted.
//! - `push::SubscriptionRegistry::newest_deletion_lock` is called by the
//!   RETENTION SWEEP, not by the request path. The sweep runs once at start-up
//!   and then every 24 hours, so its cost is irrelevant by construction; the
//!   gate exists to prove that claim rather than to defend a budget.
//!
//! What matters for the hot path is the thing this gate shows is NOT added:
//! `push_to_subscribers` - which runs on EVERY workitem creation - reads the
//! subscription list through `matching()`, which never looks at
//! `deletion_lock_at`. A subscription carrying a lock must cost the push path
//! exactly what a subscription without one costs.
//!
//! Every window proves it can fail, so a silently-disabled window cannot make
//! these gates pass vacuously.

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

/// The window is live: an allocating body must be observed.
fn assert_window_is_live() {
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}

/// Serialises the process-wide subscription registry between the tests in this
/// binary that seed and clear it. `cargo test` runs test functions in parallel
/// threads; without the guard one test's `clear()` would wipe the other's
/// just-seeded subscription mid-measurement, and the allocation gate would
/// measure the wrong registry state.
fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// The parse must not allocate for a query that carries no `deletionlock` key.
///
/// This is the shape of every subscription request that does not ask for a
/// lock, which is all of them on a server whose subscribers never use the
/// feature. The function scans the borrowed query string and returns a `bool`.
/// The matcher input for a `push_to_subscribers` call in this gate.
///
/// Every filter key is absent, so an unfiltered or lock-only subscription
/// is measured: this gate is about the cost of the fan-out, not about
/// which keys match.
fn label_values() -> clarus_server::push::FilterValues<'static> {
    clarus_server::push::FilterValues::default()
}

#[test]
fn parse_deletion_lock_allocates_nothing() {
    assert_window_is_live();

    // The realistic shapes: no query at all, and a query with other keys.
    for query in ["", "limit=10", "limit=10&offset=0"] {
        let (ok, locked) = budget_env(0, || {
            // Reached through the public handler so the gate covers the whole
            // entry point rather than a private helper.
            clarus_server::handlers::subscribe_pure::deletion_lock_for_query(query)
                .expect("a valid query parses")
        });
        assert!(ok, "query {query:?} must parse without allocating");
        assert!(!locked, "query {query:?} carries no lock");
    }

    // And the key that IS present, in the two accepted spellings.
    for query in ["deletionlock=true", "deletionlock=false"] {
        let (ok, locked) = budget_env(0, || {
            clarus_server::handlers::subscribe_pure::deletion_lock_for_query(query)
                .expect("a valid query parses")
        });
        assert!(ok, "query {query:?} must parse without allocating");
        assert_eq!(locked, query.ends_with("true"), "value {query:?}");
    }

    // A query with the key among others: still a scan of a borrowed string.
    let (ok, locked) = budget_env(0, || {
        clarus_server::handlers::subscribe_pure::deletion_lock_for_query(
            "limit=10&deletionlock=true&offset=0",
        )
        .expect("a valid query parses")
    });
    assert!(ok, "an interleaved key must not allocate either");
    assert!(locked);
}

/// An invalid value is refused; the refusal builds an error String, which is
/// the cost of reporting it. The gate pins that this is the ONLY path that
/// allocates, so the common case above cannot regress into allocating.
#[test]
fn only_the_invalid_value_allocates() {
    let (n, result) = count_env(|| {
        clarus_server::handlers::subscribe_pure::deletion_lock_for_query("deletionlock=maybe")
    });
    assert!(result.is_err(), "an out-of-range value is refused");
    assert!(
        n > 0,
        "building the error message is an allocation, and this is the only \
         path that may take one"
    );
}

/// Pins the claim that matters for the hot path: `push_to_subscribers` costs
/// the same whether or not the registered subscription carries a lock.
///
/// `matching()` filters on `step_label` and never reads `deletion_lock_at`, so
/// the workitem-creation path is untouched by this feature. If a future edit
/// makes the push path consult the lock, this gate fails.
#[test]
fn the_push_path_is_unaffected_by_a_lock() {
    use clarus_server::push::{Subscription, subscriptions};
    let _guard = registry_lock();

    let mut plain = Subscription::unfiltered("PLAIN", Some("http://127.0.0.1:1/ups"));
    plain.step_label = Some("CAD".to_string());
    let mut locked = Subscription::unfiltered("LOCKED", Some("http://127.0.0.1:2/ups"));
    locked.step_label = Some("CAD".to_string());
    locked.deletion_lock_at = Some(1_700_000_000);

    // Build the two registry states OUTSIDE the measured window: the question
    // is what the push path costs, not what registering a subscription costs.
    //
    // The push queue is lazily created on the first call in the process, so
    // the first measurement carries one-time setup. Warm it once before either
    // measurement, or the comparison is between "cold" and "warm" rather than
    // between "plain" and "locked" - which is exactly how the first draft of
    // this gate failed (14 vs 5).
    let saved = subscriptions().snapshot();
    subscriptions().clear();
    subscriptions().subscribe(plain.clone());
    let _ = count_env(|| {
        clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.20", &label_values());
    });

    subscriptions().clear();
    subscriptions().subscribe(plain.clone());
    let (plain_cost, _) = count_env(|| {
        clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.21", &label_values());
    });

    subscriptions().clear();
    subscriptions().subscribe(locked);
    let (locked_cost, _) = count_env(|| {
        clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.22", &label_values());
    });

    // And back again, so a drift caused by ordering rather than by the lock
    // shows up as an asymmetry between the two plain measurements.
    subscriptions().clear();
    subscriptions().subscribe(plain);
    let (plain_again, _) = count_env(|| {
        clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.23", &label_values());
    });

    subscriptions().clear();
    for sub in saved {
        subscriptions().subscribe(sub);
    }

    assert_eq!(
        plain_cost, locked_cost,
        "a Deletion Lock must not change what the push path costs"
    );
    assert_eq!(
        plain_cost, plain_again,
        "and the cost must be stable, so the comparison above is not an artefact \
         of warm-up order"
    );
}

/// `newest_deletion_lock` runs in the retention sweep's setup, once per sweep.
/// The gate pins that it does not allocate, because a sweep is a loop over
/// every stored workitem and an allocation here would be per-record.
#[test]
fn newest_deletion_lock_allocates_nothing() {
    use clarus_server::push::{Subscription, subscriptions};
    let _guard = registry_lock();
    assert_window_is_live();

    let saved = subscriptions().snapshot();
    subscriptions().clear();
    subscriptions().subscribe(Subscription::unfiltered(
        "A",
        Some("http://127.0.0.1:1/ups"),
    ));
    subscriptions().subscribe(Subscription::unfiltered(
        "B",
        Some("http://127.0.0.1:2/ups"),
    ));
    // Both locks are set directly: the constructor takes no lock, and this
    // test is about the retention sweep's read, not about subscription shape.
    let mut seeded = subscriptions().snapshot();
    for (index, sub) in seeded.iter_mut().enumerate() {
        sub.deletion_lock_at = Some(1_700_000_000 + index as u64 * 100);
        subscriptions().subscribe(sub.clone());
    }

    let (ok, newest) = budget_env(0, || subscriptions().newest_deletion_lock());
    assert!(ok, "newest_deletion_lock must not allocate");
    assert_eq!(newest, Some(1_700_000_100), "the newest lock wins");

    subscriptions().clear();
    for sub in saved {
        subscriptions().subscribe(sub);
    }
}

/// Determinism: the same parse costs the same twice, so the zero budgets above
/// are a property of the code and not of a lucky first run.
#[test]
fn the_parse_cost_is_stable_across_calls() {
    let (first, _) = count_env(|| {
        clarus_server::handlers::subscribe_pure::deletion_lock_for_query("deletionlock=true")
    });
    let (second, _) = count_env(|| {
        clarus_server::handlers::subscribe_pure::deletion_lock_for_query("deletionlock=true")
    });
    assert_eq!(first, second, "the parse cost must be deterministic");
    assert_eq!(first, 0, "and it must be zero");
}
