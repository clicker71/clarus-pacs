// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: worklist push request-path allocation gate (isolated binary).
//!
//! The design claim is that notifying subscribers is cheap enough to call
//! straight from the workitem-creation path. This gate pins the two things that
//! claim actually rests on, and is explicit about where the cost really is:
//!
//! - W1 `push_to_subscribers` with NO subscribers: the common case on a server
//!   where nobody subscribed. It must be a cheap no-op. It takes a path through
//!   an empty registry and returns - no job, no queue, no allocation.
//! - W2 the queue hand-off itself: `SyncSender::try_send` of an owned job into
//!   a pre-allocated slot. This is the operation the request path adds, and it
//!   is allocation-free. The job is built OUTSIDE the window on purpose - the
//!   question is not "is building a job free" (it is not) but "does handing it
//!   over cost anything". It must not.
//!
//! Both gates prove they CAN fail, so a silently-disabled window cannot make
//! them pass vacuously.

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

/// The matcher input for a `push_to_subscribers` call in this gate.
///
/// Every filter key is absent, so only the fan-out cost is measured: this
/// gate is about allocations per push, not about which keys match.
fn values() -> clarus_server::push::FilterValues<'static> {
    clarus_server::push::FilterValues::default()
}

/// A gate with no subscribers must cost nothing: the registry lookup finds an
/// empty match set and returns before any job is built.
///
/// This is the case that runs on EVERY workitem creation on a server with no
/// subscriptions, so it is the one that must be free.
#[test]
fn w1_push_with_no_subscribers_does_not_allocate() {
    // Make sure the process-wide registry really is empty for this check. The
    // tests in this binary do not register anything, so this is a guard against
    // a future edit that would quietly break the measurement.
    let subs = clarus_server::push::subscriptions();
    assert!(
        subs.is_empty(),
        "this gate assumes an empty registry; {} subscriber(s) present",
        subs.len()
    );

    let (ok, _) = budget_env(0, || {
        clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.11", &values());
    });
    assert!(
        ok,
        "push_to_subscribers with no subscribers must not allocate"
    );

    // And the window is live: an allocating body must be caught.
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}

/// Handing an already-built job to a bounded queue allocates nothing.
///
/// The job is built before the window opens. Inside it, only `try_send` runs,
/// which moves the owned value into a slot the channel reserved at creation.
#[test]
fn w2_try_send_of_a_prebuilt_job_does_not_allocate() {
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(4);
    // Warm the channel: the first send on an mpsc channel can touch lazily
    // initialised internals, and that is setup, not per-push cost.
    tx.try_send("warm".to_string()).expect("warm send");
    let _ = rx.try_recv();

    let payload = "1.2.826.0.1.3680043.9.9999.12".to_string();

    let (ok, _) = budget_env(0, || {
        tx.try_send(payload).expect("send into a fresh slot");
    });
    assert!(ok, "try_send of a pre-built job must not allocate");
}

/// Determinism: the same no-subscriber call costs the same twice, so the budget
/// above is a property of the code and not of a lucky first run.
#[test]
fn push_cost_is_stable_across_calls() {
    let (first, _) = count_env(|| {
        clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.13", &values());
    });
    let (second, _) = count_env(|| {
        clarus_server::push::push_to_subscribers("1.2.826.0.1.3680043.9.9999.14", &values());
    });
    assert_eq!(
        first, second,
        "the no-subscriber cost must be deterministic"
    );
    assert_eq!(first, 0, "and it must be zero");
}

/// The matcher itself allocates nothing when it answers.
///
/// `subscription_accepts` runs on EVERY UPS event publish (the channel asks it
/// once per open requester) and on every push fan-out, so it is the one part of
/// the filter path that must be free even though the filter set is now fourteen
/// keys wide. The comparison is `Option<&str>` against `&str` throughout, and
/// the key table is a `const`, so no `String` and no `Vec` is built per event.
///
/// The registry is registered BEFORE the window opens: `subscribe` clones the
/// subscription, and that cost belongs to the control plane, not to the match.
#[test]
fn w3_the_subscription_matcher_does_not_allocate() {
    use clarus_server::push::{FilterValues, Subscription, subscriptions};

    subscriptions().clear();
    let mut sub = Subscription::unfiltered("AP101_MATCH", None);
    // A subscription carrying EVERY key, which is the widest possible match:
    // a per-key allocation would be maximally visible here.
    sub.step_label = Some("CAD".into());
    sub.code_value = Some("CXR-AI".into());
    sub.code_scheme = Some("99CLARUS".into());
    sub.study_uid = Some("1.2.3".into());
    sub.patient_id = Some("P1".into());
    sub.workflow_state = Some("SCHEDULED".into());
    sub.input_readiness = Some("READY".into());
    sub.sps_priority = Some("HIGH".into());
    sub.sps_start_datetime = Some("20260101120000".into());
    sub.patient_name = Some("DOE^JOHN".into());
    sub.patient_id_issuer = Some("ISSUER".into());
    sub.accession_number = Some("ACC-1".into());
    sub.requested_procedure_id = Some("RP-1".into());
    sub.station_aet = Some("CT1".into());
    subscriptions().subscribe(sub);

    let hit = FilterValues {
        step_label: "CAD",
        code: Some(("CXR-AI", "99CLARUS")),
        study_uid: Some("1.2.3"),
        patient_id: Some("P1"),
        workflow_state: Some("SCHEDULED"),
        input_readiness: Some("READY"),
        sps_priority: Some("HIGH"),
        sps_start_datetime: Some("20260101120000"),
        patient_name: Some("DOE^JOHN"),
        patient_id_issuer: Some("ISSUER"),
        accession_number: Some("ACC-1"),
        requested_procedure_id: Some("RP-1"),
        station_aet: Some("CT1"),
    };
    // A miss on the LAST key, so every earlier comparison really ran.
    let mut miss = hit;
    miss.station_aet = Some("CT2");

    let (ok, matched) = budget_env(0, || {
        subscriptions().accepts_for("AP101_MATCH", "1.2.3", &hit)
    });
    assert!(matched, "the fixture must actually match");
    assert!(ok, "a matching filter must allocate nothing");

    let (ok, matched) = budget_env(0, || {
        subscriptions().accepts_for("AP101_MATCH", "1.2.3", &miss)
    });
    assert!(!matched, "the fixture must actually miss");
    assert!(ok, "a non-matching filter must allocate nothing");

    subscriptions().clear();

    // The window is live: an allocating body must be caught.
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}
