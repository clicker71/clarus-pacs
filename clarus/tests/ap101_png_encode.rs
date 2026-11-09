// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: allocation gate for the hand-rolled PNG encoder.
//!
//! `encode` is on the `/rendered` hot path, so it must satisfy the ap101
//! discipline exactly: ONE allocation whose size is computed up front, no
//! reallocation, and no slack. The three assertions below are independent
//! falsification targets - a per-row `to_vec()` breaks the count assertion, a
//! wrong sizing formula breaks the byte-total assertion, and a `Vec::new()`
//! plus `push` loop breaks the capacity assertion.
//!
//! The two sizes differ by 256x in pixel count. The allocation COUNT must be
//! identical (1) at both, which is the only way to prove the cost is not
//! proportional to the pixel-iteration count.

use clarus_adapters::png::{BitDepth, ColorType, encode, encoded_len};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Allocation count + byte total charged to the closure, measured with tracking
/// enabled only for the closure itself.
fn measure<F, R>(f: F) -> ((usize, usize), R)
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
    ((after.0 - before.0, after.1 - before.1), result)
}

/// Deterministic RGB8 ramp, so the byte totals are reproducible.
fn rgb_pixels(px: usize) -> Vec<u8> {
    (0..px * 3).map(|i| (i % 251) as u8).collect()
}

/// The exact filtered-stream length for a WxH image with `ch` channels:
/// one filter-tag byte per scanline plus the tight rows (PNG spec 9.2).
fn filtered_len(w: u32, h: u32, ch: usize) -> usize {
    (h as usize) * ((w as usize) * ch + 1)
}

#[test]
fn png_encode_is_one_exact_capacity_allocation() {
    // Warm first-touch statics (the CRC table is const-folded into `.rodata`,
    // but keep the window clean of anything the runtime may lazily touch).
    let _ = encode(&[0u8; 16 * 16 * 3], 16, 16, ColorType::Rgb, BitDepth::Eight).expect("warmup");

    for (w, h) in [(16u32, 16u32), (256u32, 256u32)] {
        let src = rgb_pixels((w * h) as usize);
        let filtered = filtered_len(w, h, 3);
        let expect = encoded_len(filtered);

        let ((count, bytes), body) =
            measure(|| encode(&src, w, h, ColorType::Rgb, BitDepth::Eight).expect("encode"));

        // ap101: exactly one allocation, and its size is the predicted total.
        assert_eq!(
            count, 1,
            "encode({w}x{h}) must allocate exactly once, got {count}"
        );
        assert_eq!(
            bytes, expect,
            "encode({w}x{h}) allocated {bytes} bytes, predicted {expect}"
        );

        // The wire size must equal the sizing formula exactly.
        assert_eq!(
            body.len(),
            expect,
            "encode({w}x{h}) produced {} bytes, formula says {expect}",
            body.len()
        );
        // ...and the value equals `63 + 5*blocks + filtered`.
        assert_eq!(
            body.len(),
            63 + 5 * filtered.div_ceil(65535) + filtered,
            "encode({w}x{h}) wire size does not match the stored-block formula"
        );

        // No slack: the single allocation was sized exactly, never grown.
        assert_eq!(
            body.len(),
            body.capacity(),
            "encode({w}x{h}) left {} bytes of slack (realloc or over-reserve)",
            body.capacity() - body.len()
        );
    }
}

#[test]
fn png_encode_gray_is_also_single_allocation() {
    // The Gray8 arm is the other color type the route emits; it must match the
    // same discipline (the /rendered handler picks it for MONO instances).
    let (w, h) = (64u32, 64u32);
    let src: Vec<u8> = (0..(w * h) as usize).map(|i| (i % 251) as u8).collect();
    let _ = encode(&[0u8; 8], 8, 1, ColorType::Grayscale, BitDepth::Eight).expect("warmup");

    let filtered = filtered_len(w, h, 1);
    let expect = encoded_len(filtered);
    let ((count, bytes), body) =
        measure(|| encode(&src, w, h, ColorType::Grayscale, BitDepth::Eight).expect("encode"));

    assert_eq!(count, 1, "gray encode must allocate exactly once");
    assert_eq!(bytes, expect, "gray encode byte total");
    assert_eq!(body.len(), expect);
    assert_eq!(body.len(), body.capacity(), "no slack");
}
