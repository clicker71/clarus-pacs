// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the stack-buffer CAS path variant is zero-alloc (A8).
//!
//! `cas_rel_path_fill_buf` writes `{xx}/{yy}/{hash}.dcm` into a caller stack
//! buffer. Gate: 0 allocations for a full 64-hex key AND byte-identity with
//! the canonical `cas_rel_path` (the single geometry source).

use clarus_adapters::storage::{cas_rel_path, cas_rel_path_fill_buf};
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

/// Window asserting an allocation CEILING (for shapes that allocate by
/// design, e.g. the one PathBuf join).
fn budget_env<F, R>(ceiling: usize, f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n <= ceiling, result)
}

/// A8: fill_buf performs 0 allocations for a full 64-hex key and stays
/// byte-identical to `cas_rel_path`.
#[test]
fn fill_buf_alloc_zero_and_byte_identical() {
    let hash = "a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2";
    // Reference computed OUTSIDE the window (it allocates by design).
    let expect = cas_rel_path(hash);

    let (zero, (buf, len)) = allocate_env(|| {
        let mut buf = [0u8; 128];
        let rel = cas_rel_path_fill_buf(hash, &mut buf).expect("64-hex key fits");
        let len = rel.len();
        (buf, len)
    });
    let rel = std::str::from_utf8(&buf[..len]).expect("utf8");
    assert!(
        zero,
        "cas_rel_path_fill_buf must be zero-allocation for a 64-hex key (A8)"
    );
    assert_eq!(
        rel, expect,
        "fill_buf output must be byte-identical to cas_rel_path"
    );
    assert!(rel.ends_with(".dcm"));
    assert!(!rel.contains('\\'), "CAS notation is forward-slash");
}

/// A8 call shape used by the STOW rename path: fill_buf +
/// `root_dir().join(rel)` keeps the canonical geometry and lands on
/// byte-identically the same file path as `cas_path`. The join costs 3
/// allocation events on Windows (std PathBuf join internals: UTF-16
/// conversion Vec + Wtf8Buf realloc; measured = 3, 272 bytes). The rel
/// render itself is 0 (see the A8 test). This removes the cas_path
/// String/format chain from the STOW rename path.
#[test]
fn fill_buf_join_call_shape_alloc_budget_and_byte_identical() {
    let hash = "a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2";
    // Fixture OUTSIDE the window.
    let cas = clarus_adapters::storage::ContentAddressedStorage::new(
        std::env::temp_dir().join(format!("clarus-ap101-cas-{}", clarus_adapters::unique_id())),
    )
    .expect("open cas");
    let expect = cas.cas_path(hash);

    let (within_budget, same_path) = budget_env(3, || {
        let mut buf = [0u8; 128];
        let rel = cas_rel_path_fill_buf(hash, &mut buf).expect("64-hex key fits");
        let path = cas.root_dir().join(rel);
        path == expect // PathBuf eq allocates nothing
    });
    assert!(
        within_budget,
        "fill_buf + root join must stay within its 3-alloc Windows join budget"
    );
    assert!(
        same_path,
        "fill_buf + join must resolve to the same file path as cas_path"
    );
}
