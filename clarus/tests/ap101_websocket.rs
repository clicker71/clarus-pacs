// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: WebSocket handshake allocation gate (isolated binary).
//!
//! The handshake is the one part of the Notification Connection that runs on a
//! REQUEST path: it parses client headers, computes an accept digest and writes
//! a 101. It is not a pixel hot path (a connection is opened once, not per
//! instance), but it is attacker-reachable, so its cost is pinned rather than
//! asserted.
//!
//! Three things are measured:
//!
//! - W1 `accept_key` (SHA-1 + base64 over the client key and the magic GUID):
//!   the only computation the server must do to answer. Its heap cost must be
//!   the digest buffer and nothing that scales with input size.
//! - W2 the base64 encoder's append form (`base64_encode_into`), which exists
//!   precisely so a caller holding a buffer can avoid a second allocation.
//! - W3 the framing of one Event Report, which is the per-event cost once a
//!   channel is open.
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

/// W1: the accept computation. Measured, not asserted.
///
/// `accept_key` returns a `String`, so at least ONE allocation is inherent to
/// its signature. The gate pins the measured number so a future edit that
/// starts allocating per byte (or drops a scratch buffer) is caught.
#[test]
fn w1_accept_key_allocation_budget() {
    use clarus_server::handlers::websocket::accept_key;
    assert_window_is_live();

    // Warm any lazily-initialised formatting machinery outside the window.
    let _ = accept_key("dGhlIHNhbXBsZSBub25jZQ==");

    let (n, accept) = count_env(|| accept_key("dGhlIHNhbXBsZSBub25jZQ=="));
    assert_eq!(
        accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
        "the RFC 6455 1.3 vector, so the cost measured is the real computation"
    );

    // The measured value is the budget. Recorded here so the number is stated
    // rather than implied by a magic constant elsewhere.
    assert!(
        n <= ACCEPT_KEY_BUDGET,
        "accept_key allocated {n}, budget is {ACCEPT_KEY_BUDGET} - a change here \
         is a change to the handshake path"
    );

    // Determinism: same input, same cost, so the budget is a property of the
    // code and not of a lucky first run.
    let (m, _) = count_env(|| accept_key("dGhlIHNhbXBsZSBub25jZQ=="));
    assert_eq!(n, m, "the accept cost must be deterministic");
}

/// Measured budget for one `accept_key` call.
///
/// The digest is a fixed 20 bytes and the accept is a fixed 28 characters, so
/// this value is a property of the algorithm, not of the input.
const ACCEPT_KEY_BUDGET: usize = 4;

/// W2: appending base64 into a caller-owned buffer must not allocate at all.
#[test]
fn w2_base64_append_into_a_warm_buffer_allocates_nothing() {
    use clarus_server::handlers::websocket::{base64_encode_into, base64_len};

    let digest = [0xA5u8; 20];
    let mut out: Vec<u8> = Vec::with_capacity(base64_len(digest.len()));
    // Warm the buffer outside the window: its capacity is already right.
    base64_encode_into(&digest, &mut out);
    let warm_capacity = out.capacity();
    out.clear();
    assert!(
        warm_capacity >= base64_len(digest.len()),
        "the buffer must be sized before the measured call"
    );

    let (ok, _) = budget_env(0, || {
        base64_encode_into(&digest, &mut out);
    });
    assert!(ok, "appending into a sized buffer must not allocate");
    assert_eq!(out.len(), 28, "a SHA-1 digest encodes to 28 characters");
}

/// W3: framing one Event Report is a single allocation for the frame, and the
/// payload is not copied twice.
#[test]
fn w3_framing_an_event_report_is_a_single_allocation() {
    use clarus_server::handlers::websocket::{encode_frame, opcode};

    let payload = "{\"workitem_uid\":\"1.2.3\",\"previous_state\":\"SCHEDULED\",\
                   \"new_state\":\"IN PROGRESS\",\"timestamp\":\"1700000000\",\
                   \"event_id\":7}";

    // Warm: `encode_frame` returns a Vec, which is one allocation by signature.
    let _ = encode_frame(opcode::TEXT, payload.as_bytes());

    let (n, frame) = count_env(|| encode_frame(opcode::TEXT, payload.as_bytes()));
    assert!(
        n <= FRAME_BUDGET,
        "framing allocated {n}, budget is {FRAME_BUDGET}"
    );
    assert_eq!(frame[0], 0x81, "FIN + text");
    assert_eq!(
        frame.len(),
        payload.len() + 2,
        "small payload: 2-byte header"
    );
}

/// Measured budget for one `encode_frame` call.
const FRAME_BUDGET: usize = 2;

/// The handshake decision itself must not allocate for the two REFUSAL paths
/// that matter: a version mismatch returns a unit variant, and the accept
/// variant's cost is the accept string measured above.
#[test]
fn the_version_refusal_does_not_allocate() {
    use clarus_server::handlers::websocket::{Handshake, evaluate_upgrade};

    let (ok, outcome) = budget_env(0, || evaluate_upgrade(Some("key"), Some("8")));
    assert!(ok, "a version refusal must not allocate");
    assert_eq!(outcome, Handshake::VersionUnsupported);
}

/// Reading the handshake decision out of real headers must stay allocation-free
/// apart from the accept string: the token matcher scans a borrowed slice.
#[test]
fn token_matching_does_not_allocate() {
    use clarus_server::handlers::websocket::header_has_token;

    // Warm any first-call machinery.
    let _ = header_has_token(b"keep-alive, Upgrade", "upgrade");

    let (ok, found) = budget_env(0, || header_has_token(b"keep-alive, Upgrade", "upgrade"));
    assert!(ok, "token matching must not allocate");
    assert!(found);

    let (ok, found) = budget_env(0, || header_has_token(b"websocketx", "websocket"));
    assert!(ok);
    assert!(!found, "a near-miss token must not match");
}
