// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: access journal render gate (isolated binary, inline window +
//! set_enabled(false); the fixture event + warm buffer are built OUTSIDE
//! the window).
//!
//! - render_event into a warm 256-byte Vec = 0 alloc (all render helpers
//!   are stack + Vec pushes; the minimal line fits 256 so no realloc).
//! - The rendered line is byte-identical to the design section 2 shape
//!   (field order pinned by the journal goldens).

use clarus_server::access_log::{AccessEvent, render_event};
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

/// Minimal access event (loopback WADO read), all owned strings pre-built.
fn minimal_event() -> AccessEvent {
    AccessEvent {
        event: "access",
        ts: "2026-08-28T00:00:00Z".to_string(),
        user: "(loopback)".to_string(),
        src_ip: "127.0.0.1".to_string(),
        via: "loopback",
        action: "R",
        atna: "110103",
        status: 200,
        study: String::new(),
        series: String::new(),
        sop: String::new(),
        patient: String::new(),
        obj_type: "13",
        q: String::new(),
        count: 1,
        changed_attrs: 0,
        bytes_replaced: 0,
        reason: "",
    }
}

#[test]
fn render_event_warm_vec_zero_alloc_and_byte_exact() {
    let ev = minimal_event(); // fixture OUTSIDE the window
    // Warm 256-byte buffer, pre-warmed by one render outside the window.
    let mut out = Vec::with_capacity(256);
    render_event(&mut out, &ev);
    out.clear();

    let (zero, ()) = allocate_env(|| {
        render_event(&mut out, &ev);
        // NOTE: emit's newline push stays OUTSIDE the gate window.
        // measured the minimal line at exactly 256 bytes, so `emit` with a
        // 256-byte buffer reallocated on EVERY event (line + newline = 257).
        // emit now pre-allocates 512; the gate here pins render_event itself
        // (0 alloc, no realloc: the line is exactly capacity).
    });
    assert!(
        zero,
        "render_event into a warm 256-byte Vec must be zero-allocation"
    );
    assert_eq!(
        out.capacity(),
        256,
        "the minimal access line must fit the 256-byte buffer (no realloc)"
    );
    out.push(b'\n'); // outside the window; grows to 512 (the emit finding)
    assert_eq!(
        out.len(),
        257,
        "minimal log line is exactly 256 bytes plus the newline"
    );

    // Byte-exact shape (design section 2 field order, W2 goldens).
    let expect = concat!(
        "{\"event\":\"access\",\"ts\":\"2026-08-28T00:00:00Z\",",
        "\"user\":\"(loopback)\",\"role\":\"\",\"src\":\"clarus-server\",",
        "\"src_ip\":\"127.0.0.1\",\"via\":\"loopback\",\"action\":\"R\",",
        "\"atna\":\"110103\",\"status\":200,\"study\":\"\",\"series\":\"\",",
        "\"sop\":\"\",\"patient\":\"\",\"obj_type\":\"13\",\"q\":\"\",",
        "\"count\":1}\n"
    );
    assert_eq!(
        std::str::from_utf8(&out).expect("journal lines are utf8"),
        expect,
        "rendered line must be byte-identical to the design section 2 shape"
    );
}

/// The PHI masking runs on the request path, once per journal line, so its
/// steady-state cost is a gate: a query with no escape must not allocate
/// beyond the output itself.
///
/// The gate is falsifiable by construction: pointing `mask_query_phi` back at
/// an unconditional percent-decode allocates an intermediate `String` per key
/// and the assertion below fails.
#[test]
fn mask_query_phi_allocates_only_the_output() {
    // A realistic QIDO query: one identifier that stays, one name that masks,
    // both in the plain spelling a stock client sends.
    let qs = "PatientID=P1&PatientName=Ivanov";

    // Warm the global scratch/allocator bookkeeping outside the window, then
    // measure. The output String is the only expected allocation.
    let warm = clarus_server::access_log::mask_query_phi(qs);
    assert_eq!(warm, "PatientID=P1&PatientName=***");

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let out = clarus_server::access_log::mask_query_phi(qs);
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    // One allocation: the returned String (its capacity is reserved once).
    assert_eq!(
        after.0 - before.0,
        1,
        "a plain query must allocate exactly the output string"
    );
    assert_eq!(out, "PatientID=P1&PatientName=***");
}
