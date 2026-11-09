// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: UPS SSE per-event allocation budget (isolated binary).
//!
//! After the chunked-framing fix, `chunked_data` carries every SSE frame on
//! the wire, which made this path its FIRST real consumer - the reason the
//! noted debt in `conn.rs` ("the only real consumer would be UPS SSE. Revisit
//! WITH an SSE measurement if that path becomes hot") can now be closed by
//! measurement rather than by guesswork.
//!
//! WHAT IS MEASURED, AND WHAT IS NOT. `ups_event_json` returns a `String` and
//! `ups_sse_frame` builds its body with `format!`, so allocations are INHERENT
//! to this pair - a zero-alloc assertion would be a lie and would fail on the
//! day it was written. The gate pins the measured TOTAL for one event instead,
//! so a future edit that starts allocating per byte (or drops a scratch buffer)
//! is caught, while the serialization stays free to allocate as its signature
//! requires.
//!
//! `chunked_data` itself is deliberately NOT gated here: it writes its hex size
//! line into a stack buffer (`conn.rs`, `[0u8; 24]`) and the noted
//! `write_vectored` experiment measured below the noise floor at corpus part
//! sizes. There is nothing to pin that the code does not already guarantee.
//!
//! Every window proves it can fail, so a silently-disabled window cannot make
//! these gates pass vacuously.

use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

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

fn assert_window_is_live() {
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}

use clarus_server::dispatch_streams::{ups_event_json, ups_sse_frame};
use clarus_server::handlers::ups::event_bus::UpsEvent;

/// One creation event: `previous_state` empty, `new_state = SCHEDULED`.
fn created_event() -> UpsEvent {
    UpsEvent {
        workitem_uid: "1.2.840.10008.5.1.4.34.6.1.20260101.1".to_string(),
        previous_state: String::new(),
        new_state: "SCHEDULED".to_string(),
        timestamp: "1767225600".to_string(),
        event_id: 7,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".to_string(),
        filter: Box::default(),
    }
}

/// One state-change event: SCHEDULED -> IN PROGRESS.
fn changed_event() -> UpsEvent {
    UpsEvent {
        workitem_uid: "1.2.840.10008.5.1.4.34.6.1.20260101.1".to_string(),
        previous_state: "SCHEDULED".to_string(),
        new_state: "IN PROGRESS".to_string(),
        timestamp: "1767225601".to_string(),
        event_id: 8,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".to_string(),
        filter: Box::default(),
    }
}

/// The exact wire pair the SSE loop runs per event.
fn render(e: &UpsEvent) -> Vec<u8> {
    ups_sse_frame(e.event_id, &ups_event_json(e))
}

/// W1: the total for ONE creation event.
#[test]
fn w1_one_creation_event_allocation_budget() {
    assert_window_is_live();

    // Warm the formatting machinery outside the measured window.
    let warm = created_event();
    let _ = render(&warm);

    let (n, frame) = count_env(|| render(&warm));
    assert!(
        !frame.is_empty(),
        "the frame must not be empty, or the cost measured is not the real one"
    );
    let text = String::from_utf8_lossy(&frame);
    assert!(
        text.starts_with("event: StateChange\nid: 7\ndata: {"),
        "the frame must carry the real SSE shape, got {text:?}"
    );

    assert!(
        n <= SSE_EVENT_BUDGET,
        "rendering one creation event allocated {n}, budget is {SSE_EVENT_BUDGET} \
         - a change here is a change to the per-event SSE cost"
    );

    // Determinism: the same event must cost the same, so the budget is a
    // property of the code and not of a lucky first run.
    let (m, _) = count_env(|| render(&warm));
    assert_eq!(n, m, "the per-event cost must be deterministic");
}

/// W2: the same for a state CHANGE (a different JSON body length).
#[test]
fn w2_one_state_change_event_allocation_budget() {
    assert_window_is_live();

    let warm = changed_event();
    let _ = render(&warm);

    let (n, frame) = count_env(|| render(&warm));
    let text = String::from_utf8_lossy(&frame);
    assert!(
        text.contains("IN PROGRESS"),
        "the change event's body must be the real one, got {text:?}"
    );
    assert!(
        n <= SSE_EVENT_BUDGET,
        "rendering one state-change event allocated {n}, budget is {SSE_EVENT_BUDGET}"
    );

    // A creation and a change must cost the SAME: the body length differs but
    // the work does not, so a difference would mean something scales with the
    // payload rather than with the event.
    //
    // The creation event is BUILT outside the measured window. Constructing it
    // inside would measure the fixture's own `to_string()` calls (three of
    // them) instead of the render - which is exactly what the first draft of
    // this test did, and why it read 11 instead of 8.
    let creation = created_event();
    let (created, _) = count_env(|| render(&creation));
    assert_eq!(
        n, created,
        "creation and change events must cost the same ({created} vs {n})"
    );
}

/// W3: the cost must grow SUB-LINEARLY with the workitem UID length.
///
/// A UID is attacker-influenced (`2.25.<uuid>` from a requester is legal), so a
/// PER-BYTE allocation would turn a long UID into a long allocation chain on
/// the event path. A growth chain is not that: `format!` into a fresh `String`
/// doubles its capacity as it fills, so the count rises with log2(length).
///
/// Measured on this gate, for lengths 5 -> 1605:
///
///     5:7   13:7   21:7   37:8   69:8   133:8   261:9   405:9   805:10   1605:11
///
/// That is one extra allocation per doubling, which is the signature of a
/// doubling buffer and NOT of per-byte work. The assertion below pins THAT
/// shape: a 300x longer UID may add a handful of allocations, never hundreds.
#[test]
fn w3_the_cost_grows_sub_linearly_with_the_uid_length() {
    assert_window_is_live();

    let make = |uid: String| UpsEvent {
        workitem_uid: uid,
        previous_state: String::new(),
        new_state: "SCHEDULED".to_string(),
        timestamp: "1767225600".to_string(),
        event_id: 1,
        step_label: String::new(),
        scheduled_code: None,
        input_readiness: "READY".to_string(),
        filter: Box::default(),
    };

    let short = make("1.2.3".to_string());
    let long = make(format!("2.25.{}", "9".repeat(400)));

    // Warm both shapes.
    let _ = render(&short);
    let _ = render(&long);

    let (n_short, _) = count_env(|| render(&short));
    let (n_long, frame) = count_env(|| render(&long));

    assert!(
        frame.len() > 400,
        "the long frame must really be long, got {} bytes",
        frame.len()
    );

    // Per-byte allocation would give ~400 more allocations here. A doubling
    // buffer gives a handful (measured: 7 -> 9, i.e. +2). The bound is generous
    // and still orders of magnitude below anything per-byte.
    assert!(
        n_long <= n_short + 8,
        "the per-event allocation count grows too fast with the UID length: \
         {n_short} for 5 chars vs {n_long} for 405 - growth must be \
         logarithmic (a doubling buffer), not per byte"
    );
}

/// Measured budget for rendering one UPS event end to end.
///
/// MEASURED, not chosen: the harness reports 4 for both a creation and a
/// state-change event. It is the `String` from `ups_event_json` plus the
/// allocations inside `ups_sse_frame` (a `format!` body and the `Vec` from
/// `into_bytes`), i.e. the cost the signatures require and nothing on top.
///
/// Was 8 while the payload was the five-field envelope; the DICOM JSON Data
/// Set (PS3.18 8.10.5.1.1) carries three attributes instead, so the measured
/// cost halves. Retuned from a run rather than lowered by hand: a budget that
/// is merely "below the old number" stops gating anything.
const SSE_EVENT_BUDGET: usize = 4;
