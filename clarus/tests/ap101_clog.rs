// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: a dropped log record costs zero formatting (A4).
//!
//! `emit`/`emit_tagged` must gate level/tag BEFORE `args.to_string()`, so a
//! record at a disabled level performs no allocations and produces no output.
//! (A caller that would allocate when formatted is passed as the message.)

use clarus_server::clog::{self, Level};
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

/// A4: emit at a DISABLED level (DEBUG under the default `info` ceiling)
/// performs 0 allocations and pushes nothing to the writer channel.
#[test]
fn emit_disabled_level_alloc_zero_no_output() {
    // Fixture: a real writer channel so "no output" is observable, and a
    // message whose Display would allocate if it were ever formatted.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
    clog::set_sender(tx);
    let msg = String::from("formatted only if the level gate is wrong");

    let (zero, ()) = allocate_env(|| {
        clog::emit(Level::Debug, format_args!("{msg}"));
    });
    assert!(
        zero,
        "emit at a disabled level must not format the message (A4)"
    );
    assert!(
        rx.try_recv().is_err(),
        "nothing may be pushed to the writer for a dropped record"
    );
}
