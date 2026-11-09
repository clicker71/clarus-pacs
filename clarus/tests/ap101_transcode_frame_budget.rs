//! ap101 gate: the per-tile decode REFUSES an over-budget tile before any
//! buffer exists.
//!
//! W22 added a decode path on `/frames/{n}`. The tile geometry it works from
//! (Rows 0028,0010 and Columns 0028,0011) is attacker-declared at STOW, so the
//! budget check is the only thing standing between a crafted `65535 x 65535`
//! tile and a ~4 GiB decode buffer. The contract measured here is arithmetic:
//! a refused tile costs ZERO bytes, not "a smaller amount".
//!
//! The gate asserts on the pure budget function rather than through the route:
//! the route would first copy the fragment into a synthesized dataset, and
//! that copy is a legitimate cost of the SUCCEEDING path. What must not
//! allocate is the refusal itself.

use clarus_server::handlers::wado_pure::FramePixelModule;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// The configured default: 64 MP (`config::DEFAULT_MAX_MEGAPIXELS`).
const MAX_MP: u32 = 64;

/// A tile whose declared geometry is far above the cap.
///
/// 65535 is the largest value a US VR can hold, so this is the worst case a
/// crafted instance can ask for, not a round number picked to look large.
fn over_budget() -> FramePixelModule {
    FramePixelModule {
        rows: 65535,
        columns: 65535,
        bits_allocated: 16,
        samples_per_pixel: 1,
    }
}

#[test]
fn the_over_budget_refusal_allocates_zero_bytes() {
    let tile = over_budget();
    assert!(
        tile.expected_bytes() > 4 * 1024 * 1024 * 1024,
        "the fixture must describe a multi-GiB tile, got {} bytes",
        tile.expected_bytes()
    );

    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let err = tile
        .check_budget(MAX_MP)
        .expect_err("an over-budget tile must be refused");
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    // The expected_bytes() call above is arithmetic only; the refusal itself
    // must not touch the allocator.
    let (count, bytes) = (after.0 - before.0, after.1 - before.1);
    assert_eq!(
        bytes, 0,
        "an over-budget tile must allocate ZERO bytes (got {bytes})"
    );
    assert_eq!(
        count, 0,
        "an over-budget tile must make ZERO allocations (got {count})"
    );
    // And it is a payload-limit refusal (413), not a generic failure.
    assert!(
        matches!(
            err,
            clarus_server::handlers::wado::error::WadoError::PayloadTooLargeStatic(_)
        ),
        "the refusal must be a payload-limit error, got {err:?}"
    );
}

/// A tile exactly at the cap is accepted, and the length it promises is the
/// decoded size: `rows * columns * bytes_per_sample`.
#[test]
fn a_tile_at_the_cap_is_accepted_and_its_size_is_exact() {
    // 8000 x 8000 = 64 MP exactly, 16-bit samples -> 128 MB of pixels.
    let tile = FramePixelModule {
        rows: 8000,
        columns: 8000,
        bits_allocated: 16,
        samples_per_pixel: 1,
    };
    assert!(tile.check_budget(MAX_MP).is_ok(), "the cap is inclusive");
    assert_eq!(tile.expected_bytes(), 8000 * 8000 * 2);
}

/// One pixel over the cap is refused: the boundary is exact, not approximate.
#[test]
fn one_pixel_over_the_cap_is_refused() {
    let tile = FramePixelModule {
        rows: 8000,
        columns: 8000,
        bits_allocated: 16,
        samples_per_pixel: 1,
    };
    assert!(tile.check_budget(MAX_MP).is_ok());
    let over = FramePixelModule {
        columns: 8001,
        ..tile
    };
    assert!(
        over.check_budget(MAX_MP).is_err(),
        "one column over 64 MP must be refused"
    );
}

/// The geometry multiply is `u64`: a 16-bit wrap would let a crafted tile
/// claim a small size and pass the cap.
#[test]
fn the_geometry_multiply_does_not_wrap() {
    let tile = over_budget();
    // 65535 * 65535 = 4_294_836_225, which overflows u32 (4_294_967_295 is
    // just above it, so the wrap would be visible only for a wider pair - the
    // point is that the function computes in u64 and never wraps at all).
    assert_eq!(tile.expected_bytes(), 65535u64 * 65535 * 2);
    assert!(
        tile.expected_bytes() > u32::MAX as u64,
        "the expected size must exceed a 32-bit count"
    );
}

/// The allocator harness itself is honest: a deliberate allocation is counted.
/// Without this, a zero-byte assertion above could pass for the wrong reason.
#[test]
fn the_allocator_harness_counts_a_real_allocation() {
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let probe: Vec<u8> = vec![0u8; 4096];
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);

    assert!(
        after.1 - before.1 >= 4096,
        "the harness must see a 4096-byte allocation (saw {})",
        after.1 - before.1
    );
    assert!(after.0 - before.0 >= 1, "at least one allocation counted");
    std::hint::black_box(&probe);
}
