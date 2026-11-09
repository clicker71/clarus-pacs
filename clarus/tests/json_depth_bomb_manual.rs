// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
//
// Crash evidence for the JSON depth guard. NOT part of the test suite - run by
// hand (see the stage report) to record what the OLD parser did with a
// hostile body. The old parser is reproduced verbatim below, minus the guard,
// as a standalone recursive-descent parser in a thread with the SAME 512 KiB
// worker stack the connection loop uses.
//
//   cargo test -p clarus --test json_depth_bomb_manual -- --ignored --nocapture
//
// Expected on the guarded parser: the guard returns None long before the
// stack is at risk, so the run is silent. Expected on the unguarded one: a
// stack overflow abort.

#![forbid(unsafe_code)]

/// Worker stack size, mirrored from `clarus_server::serve` (WORKER_STACK).
const WORKER_STACK: usize = 512 * 1024;

/// The pre-fix parser: `parse_value` -> `parse_array` recursion with no depth
/// counter at all. Only the array branch is reproduced because that is the
/// cheapest recursion for an attacker (one byte per stack frame).
mod unguarded {
    pub fn parse_array(bytes: &[u8], pos: &mut usize) -> bool {
        if bytes.get(*pos) != Some(&b'[') {
            return false;
        }
        *pos += 1;
        // Recursion: one stack frame per '[' byte, exactly as before.
        if parse_array(bytes, pos) {
            return true;
        }
        *pos += 1;
        if bytes.get(*pos - 1) == Some(&b']') {
            return true;
        }
        false
    }
}

#[test]
#[ignore = "manual crash evidence - deliberately overflows the stack"]
fn unguarded_parser_overflows_the_worker_stack() {
    // 200k '[': well inside the 1 MiB max_json_body, and enough frames to
    // exceed 512 KiB of stack.
    let bomb = vec![b'['; 200_000];
    let handle = std::thread::Builder::new()
        .name("unguarded-parser".into())
        .stack_size(WORKER_STACK)
        .spawn(move || {
            let mut pos = 0usize;
            let ok = unguarded::parse_array(&bomb, &mut pos);
            println!("unguarded parser returned {ok} (no overflow on this host)");
        })
        .expect("spawn");
    // A stack overflow in a Rust thread aborts the PROCESS, so reaching the
    // join at all would mean the bomb did not overflow on this host.
    let joined = handle.join();
    println!("join result: {:?}", joined.is_ok());
}

#[test]
fn guarded_parser_rejects_the_same_bomb_without_recursing() {
    // The shipping parser. Same body, same stack size, no overflow: the depth
    // counter refuses at 128 levels.
    let bomb = vec![b'['; 200_000];
    let handle = std::thread::Builder::new()
        .name("guarded-parser".into())
        .stack_size(WORKER_STACK)
        .spawn(move || clarus_server::handlers::json_common::parse_json(&bomb).is_none())
        .expect("spawn");
    assert!(
        handle.join().expect("guarded parser must not overflow"),
        "the guarded parser must report the bomb as malformed"
    );
}
