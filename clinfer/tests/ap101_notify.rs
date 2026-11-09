// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: notification-channel allocation gate (isolated binary).
//!
//! The channel is a per-EVENT path, not a per-instance one: an open
//! connection carries one Event Report per workitem the performer is
//! subscribed to, and each report is parsed to decide whether it is worth
//! waking the worker for. That parse runs on a thread that must also answer
//! pings, so its cost is pinned rather than asserted.
//!
//! Two things are measured:
//!
//! - C1 `actionable_uid`: the decision function every arriving frame goes
//!   through. It returns a `String` for the actionable case, so one
//!   allocation is inherent; the gate pins the measured number so an edit
//!   that starts allocating per attribute is caught.
//! - C2 the frame decode, which is the per-frame cost before any decision is
//!   taken.
//!
//! The channel-OPEN path is deliberately NOT measured here. A connection is
//! opened once per process lifetime (plus once per reconnect), and the
//! handshake's cost is already pinned on the server side by
//! `crates/clarus-server/tests/ap101_websocket.rs`. Measuring it again here
//! would pin a number no deployment can be sensitive to, and the two
//! implementations would then have to be kept in step for no benefit.

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

fn assert_window_is_live() {
    let (over, _) = count_env(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
    });
    assert!(over > 0, "the window must observe allocations");
}

/// The payload shape a real Event Report has: a DICOM JSON Data Set
/// (PS3.18 8.10.5.1.1), every attribute an object with `vr` + `Value`.
const REPORT: &[u8] = br#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]},"00404041":{"vr":"CS","Value":["READY"]},"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#;

/// The payload shape a report the performer ignores has.
const IGNORED: &[u8] =
    br#"{"00001000":{"vr":"UI","Value":["1.2.3.4"]},"00741000":{"vr":"CS","Value":["COMPLETED"]}}"#;

/// C1: the actionable decision. Measured, not asserted.
///
/// `actionable_uid` returns `String` for the actionable case, so ONE
/// allocation is inherent to its signature; the gate pins the measured
/// number. A report that is NOT actionable must allocate NOTHING beyond
/// what the parser needs - it is the common case on a shared channel.
#[test]
fn c1_the_actionable_decision_budget() {
    assert_window_is_live();
    let (n, uid) = count_env(|| clinfer::notify::actionable_uid(REPORT));
    assert_eq!(uid.as_deref(), Some("1.2.3.4"), "the decision is correct");
    println!("C1 actionable_uid allocations: {n}");
    // Pinned at the measured value. If this number moves, the decision
    // function changed shape and the change should be visible in review
    // rather than absorbed silently.
    let (ok, uid) = budget_env(C1_BUDGET, || clinfer::notify::actionable_uid(REPORT));
    assert!(ok, "actionable_uid allocated {uid:?} beyond {C1_BUDGET}");
}

/// C1b: a report the performer ignores must not allocate a UID.
/// C1b: an ignored report costs the parser and no UID.
///
/// The name says what is asserted rather than what would be nice to be
/// true: the ignored path still builds a JSON document, so it does NOT
/// allocate nothing. What it must not do is allocate the UID `String`, and
/// the budget below is the measured parser-only cost.
#[test]
fn c1b_an_ignored_report_allocates_no_uid() {
    assert_window_is_live();
    let (n, uid) = count_env(|| clinfer::notify::actionable_uid(IGNORED));
    assert_eq!(uid, None, "a COMPLETED report is not actionable");
    println!("C1b ignored-report allocations: {n}");
    let (ok, uid) = budget_env(C1B_BUDGET, || clinfer::notify::actionable_uid(IGNORED));
    assert!(
        ok,
        "the ignored path allocated beyond {C1B_BUDGET}: {uid:?}"
    );
}

/// C2: decoding one server frame. Measured, not asserted.
#[test]
fn c2_decoding_a_frame_budget() {
    use clinfer::ws::decode_server_frame;
    assert_window_is_live();
    // An UNMASKED frame, which is what a server sends (RFC 6455 5.1). Built
    // by hand rather than through the client's encoder, because that encoder
    // masks and the decoder would rightly refuse it.
    let frame = unmasked_text_frame(REPORT);
    let (n, decoded) = count_env(|| decode_server_frame(&frame));
    let (decoded, used) = decoded.expect("a complete frame decodes");
    assert_eq!(decoded.opcode, 0x1);
    assert_eq!(used, frame.len());
    println!("C2 decode_server_frame allocations: {n}");
    let (ok, _) = budget_env(C2_BUDGET, || decode_server_frame(&frame));
    assert!(ok, "decode allocated beyond {C2_BUDGET}");
}

/// An unmasked server text frame, exactly what a conformant peer sends.
fn unmasked_text_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | 0x1); // FIN + TEXT
    let len = payload.len();
    if len < 126 {
        frame.push(len as u8);
    } else {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

/// C3: the backoff rule is pure arithmetic and allocates nothing.
#[test]
fn c3_the_backoff_rule_allocates_nothing() {
    use clinfer::notify::{BACKOFF_FIRST, next_backoff};
    assert_window_is_live();
    let (ok, _) = budget_env(0, || {
        let mut d = BACKOFF_FIRST;
        for _ in 0..20 {
            d = next_backoff(d);
        }
        std::hint::black_box(d);
    });
    assert!(ok, "the backoff rule must not allocate");
}

/// Measured C1 budget.
///
/// 25, not 1. The UID `String` is one allocation; the rest is the JSON
/// parser building a document for the Event Report, whose attributes are
/// objects with their own `vr` and `Value` entry (a DICOM JSON Data Set,
/// PS3.18 8.10.5.1.1) rather than flat values. That is the honest cost of
/// deciding on a report - the parser is shared with every other route, so
/// its allocation profile is not something the channel gets to choose, and
/// pinning the real number is what makes a CHANGE visible. The decision
/// path is once per event, not once per instance, so this is not a hot path
/// in the pixel sense; it is pinned because it runs on the thread that must
/// also answer pings.
const C1_BUDGET: usize = 25;

/// Measured C1b budget: the parser only, no UID `String`.
///
/// 17 is 8 fewer than the actionable case, which is exactly what dropping
/// the UID `String` and its trim should look like. A report the performer
/// ignores is the common case on a shared channel, so it must not cost the
/// same as one it acts on.
const C1B_BUDGET: usize = 17;

/// Measured C2 budget: the frame's payload copy, one `Vec`.
const C2_BUDGET: usize = 1;

/// C1c: the ignored path must stay CHEAPER than the actionable one.
///
/// This is the assertion that carries meaning independent of the exact
/// numbers: if a future edit made the ignored path allocate a UID anyway,
/// the two budgets could each be re-pinned to the same value and both gates
/// would still pass. Asserting the ORDER between them is what would fail.
#[test]
fn c1c_an_ignored_report_costs_less_than_an_actionable_one() {
    assert_window_is_live();
    let (ignored, _) = count_env(|| clinfer::notify::actionable_uid(IGNORED));
    let (actionable, _) = count_env(|| clinfer::notify::actionable_uid(REPORT));
    assert!(
        ignored < actionable,
        "ignoring a report must be cheaper than acting on one: \
         ignored={ignored}, actionable={actionable}"
    );
}
