// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: `/rendered` RGB-to-PNG allocation gate (isolated binary).
//!
//! The RGB preview path (`render_rgb_png`) must not allocate per PIXEL. Its
//! shape: the deinterleave allocates THREE source-sized planes (r, g, b), the
//! per-plane area-average allocates ONE dst-sized buffer per plane, and the
//! re-interleave allocates ONE interleaved RGB buffer; the PNG encoder produces
//! ONE output buffer. Every allocation is proportional to the INPUT or OUTPUT
//! size - never to the pixel-iteration count, and nothing inside the per-pixel
//! loops allocates.
//!
//! The SAME instance is rendered at two output sizes 256x apart, so the only
//! variable is the output pixel count. A regression that introduced a
//! per-output-pixel allocation anywhere along the path would raise the count
//! with the output size, which the ratio assertion catches. Fixture build and
//! warm-up happen OUTSIDE the measured window.

use clarus_server::handlers::wado_pure::{RgbGeometry, render_rgb_png};
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

/// Append an Explicit VR LE element (PS3.5 7.1).
fn push_evrle(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let long = matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN");
    if long {
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    buf.extend_from_slice(value);
    if value.len() % 2 == 1 {
        buf.push(0);
    }
}

/// A bare interleaved RGB8 PixelData value (even-row-padded) for a WxH image.
fn rgb_pixels(rows: u32, cols: u32) -> Vec<u8> {
    let row = (cols as usize) * 3;
    let stride = row + (row & 1);
    let mut v = vec![0u8; stride * rows as usize];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    v
}

/// A minimal Explicit VR LE DICOM with an interleaved RGB8 PixelData.
fn build_rgb(rows: u16, cols: u16) -> Vec<u8> {
    let mut d = Vec::with_capacity(4096 + (rows as usize * cols as usize * 3));
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_evrle(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1\0");
    push_evrle(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.77.1.6\0",
    );
    push_evrle(&mut d, 0x0008, 0x0018, b"UI", b"1.2.3.4.5.6.7.8.9\0");
    push_evrle(&mut d, 0x0028, 0x0002, b"US", &3u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0004, b"CS", b"RGB");
    push_evrle(&mut d, 0x0028, 0x0006, b"US", &0u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    push_evrle(
        &mut d,
        0x7FE0,
        0x0010,
        b"OB",
        &rgb_pixels(rows as u32, cols as u32),
    );
    d
}

#[test]
fn rendered_rgb_png_does_not_allocate_per_output_pixel() {
    // One 256x256 RGB source rendered at two output sizes whose pixel counts
    // differ by 256x (16x16 vs 256x256). Everything except the output size is
    // identical.
    let dcm = build_rgb(256, 256);
    let pixels = rgb_pixels(256, 256);
    let geom = RgbGeometry {
        rows: 256,
        cols: 256,
        bits_allocated: 8,
    };
    let small = (16u32, 16u32);
    let large = (256u32, 256u32);

    // Warm the shared paths (PNG encoder deflate tables, first-touch statics).
    render_rgb_png(&pixels, RgbGeometry { ..geom }, small).expect("warmup small");
    render_rgb_png(&pixels, RgbGeometry { ..geom }, large).expect("warmup large");
    let _ = &dcm;

    let ((small_count, _small_bytes), small_resp) =
        measure(|| render_rgb_png(&pixels, RgbGeometry { ..geom }, small).expect("small"));
    let ((large_count, _large_bytes), large_resp) =
        measure(|| render_rgb_png(&pixels, RgbGeometry { ..geom }, large).expect("large"));

    assert_eq!(small_resp.content_type, "image/png", "small must be PNG");
    assert_eq!(large_resp.content_type, "image/png", "large must be PNG");
    assert_eq!(
        &small_resp.body[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
    );

    // Count shape: the real path costs a small CONSTANT number of allocations
    // regardless of output size. An implementation that allocated per output
    // pixel would put the large count at 65536+ against the small count's 256.
    // The bound is generous headroom and exists solely to catch that explosion.
    let ratio = large_count as f64 / small_count.max(1) as f64;
    assert!(
        ratio < 4.0,
        "allocation COUNT grew {ratio:.1}x when the output grew 256x \
         ({small_count} -> {large_count}); a per-output-pixel alloc would explode here",
    );
}
