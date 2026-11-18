// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101 gate: the `{frames}` grammar gate is allocation-free, and the list
//! parser refuses an over-cap value BEFORE it allocates.
//!
//! D4 widened the `/frames/{n}` slot to accept `frames/1,2,3`. `/frames/{n}`
//! is one of the hot routes (WADO frame serve), so the widening must not cost
//! the single-frame request a single byte: every `GET .../frames/7` now also
//! runs the list gate, and a gate that allocated would tax every image load in
//! the viewer for a feature that load does not use.
//!
//! The second half is the bounded-resource contract: a request naming more
//! frames than an instance can hold is refused by COUNTING separators, before
//! any `Vec` exists. Without that, the cap would be enforced after the
//! allocation it exists to prevent.

use clarus_core::http::router::frame_ok;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// The `{frames}` grammar gate is allocation-free on every path it covers.
///
/// One `#[test]` owns the process-wide `TestAllocator`: three separate test
/// functions would run on parallel libtest threads, and one thread's
/// `reset()`/`set_enabled()` could land inside another thread's measured
/// window, making the zero-alloc snapshot nondeterministic. Each scenario
/// below keeps its own disable/reset/enable/snapshot window and runs
/// sequentially on the single test thread.
#[test]
fn frame_gate_paths_are_allocation_free() {
    // A single frame reference - the value every ordinary image load sends.
    {
        ALLOC.set_enabled(false);
        ALLOC.reset();
        ALLOC.set_enabled(true);
        let before = ALLOC.snapshot();
        let ok = frame_ok(b"12345");
        let after = ALLOC.snapshot();
        ALLOC.set_enabled(false);

        assert!(ok, "a plain decimal must pass the gate");
        let (count, bytes) = (after.0 - before.0, after.1 - before.1);
        assert_eq!(
            bytes, 0,
            "the single-frame gate must allocate ZERO bytes (got {bytes})"
        );
        assert_eq!(
            count, 0,
            "the single-frame gate must make ZERO allocations (got {count})"
        );
    }

    // The list form is just as cheap: the gate only walks bytes.
    {
        ALLOC.set_enabled(false);
        ALLOC.reset();
        ALLOC.set_enabled(true);
        let before = ALLOC.snapshot();
        let ok = frame_ok(b"1,2,3,4,5,6,7,8");
        let after = ALLOC.snapshot();
        ALLOC.set_enabled(false);

        assert!(ok, "a comma list must pass the gate");
        let (count, bytes) = (after.0 - before.0, after.1 - before.1);
        assert_eq!(bytes, 0, "the list gate must allocate ZERO bytes");
        assert_eq!(count, 0, "the list gate must make ZERO allocations");
    }

    // Every rejection path is allocation-free too - a refused request must not
    // be a cheap way to make the server allocate.
    for bad in [&b""[..], b",1", b"1,", b"1,,2", b"1-2", b"abc"] {
        ALLOC.set_enabled(false);
        ALLOC.reset();
        ALLOC.set_enabled(true);
        let before = ALLOC.snapshot();
        let ok = frame_ok(bad);
        let after = ALLOC.snapshot();
        ALLOC.set_enabled(false);

        assert!(!ok, "{:?} must be refused", String::from_utf8_lossy(bad));
        let (count, bytes) = (after.0 - before.0, after.1 - before.1);
        assert_eq!(
            bytes,
            0,
            "refusing {:?} must allocate ZERO bytes",
            String::from_utf8_lossy(bad)
        );
        assert_eq!(count, 0, "refusing must make ZERO allocations");
    }
}
