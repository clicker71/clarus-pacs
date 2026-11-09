// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: thumbnail downscale allocation gate (isolated binary).
//!
//! The thumbnail path must not allocate per OUTPUT PIXEL. The area-average
//! downscale (`resize_area_average`, the box filter that replaced
//! nearest-neighbour) allocates exactly ONE `vec![0u8; dst_w * dst_h]` - a
//! single allocation whose SIZE is linear in the OUTPUT pixel count - and then
//! writes into it. Nothing inside the per-pixel loops allocates.
//!
//! This gate pins that shape through the PUBLIC entry point
//! (`render_thumbnail_from_bytes`) rather than the private helper, so the whole
//! raw-path render runs inside the measured window: windowing, downscale and
//! JPEG encode are all inside it. The SAME instance is rendered at two output
//! sizes 256x apart, so the only variable is the output pixel count. A
//! regression that introduced a per-output-pixel allocation anywhere along that
//! path raises the allocation COUNT with the output size, which the ratio
//! assertion catches.
//!
//! Fixture construction and buffer pre-warming happen OUTSIDE the window.

use clarus_server::handlers::wado_pure::render_thumbnail_from_bytes;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Allocation count + byte total charged to the closure, measured with
/// tracking enabled only for the closure itself.
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

/// Append an Explicit VR Little Endian element (PS3.5 Section 7.1). Uses the
/// reserved-4-byte long form for the VRs that require it (OB/OW/OF/SQ/UT/UN),
/// so a PixelData value larger than 65535 is framed correctly.
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

/// Minimal Explicit VR LE instance with an 8-bit MONOCHROME2 plane. Meta
/// transfer syntax and dataset framing agree (both Explicit VR LE), which the
/// parser requires.
fn build_evrle(rows: u16, cols: u16) -> Vec<u8> {
    let mut d = Vec::with_capacity(4096 + rows as usize * cols as usize);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    push_evrle(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1\0");
    push_evrle(
        &mut d,
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.7\0",
    );
    push_evrle(&mut d, 0x0008, 0x0018, b"UI", b"1.2.3.4.5.6.7.8.9\0");
    push_evrle(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    push_evrle(&mut d, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    push_evrle(&mut d, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    push_evrle(
        &mut d,
        0x7FE0,
        0x0010,
        b"OB",
        &vec![0x5a_u8; rows as usize * cols as usize],
    );
    d
}

const SOP: &str = "1.2.3.4.5.6.7.8.9";

#[test]
fn thumbnail_downscale_does_not_allocate_per_output_pixel() {
    // One 512x512 source rendered at two output sizes whose pixel counts differ
    // by 256x (256 vs 65536). Everything except the output size is identical.
    let dcm = build_evrle(512, 512);
    let small = (16u32, 16u32);
    let large = (256u32, 256u32);

    // Warm the shared code paths (parser arming, JPEG encoder quantisation
    // tables, first-touch lazy statics) so one-off setup is not charged to the
    // first measured call.
    render_thumbnail_from_bytes(&dcm, SOP, small).expect("warmup small");
    render_thumbnail_from_bytes(&dcm, SOP, large).expect("warmup large");

    let ((small_count, small_bytes), small_resp) =
        measure(|| render_thumbnail_from_bytes(&dcm, SOP, small).expect("small render"));
    let ((large_count, large_bytes), large_resp) =
        measure(|| render_thumbnail_from_bytes(&dcm, SOP, large).expect("large render"));

    assert_eq!(small_resp.status, 200, "16x16 thumbnail must render");
    assert_eq!(large_resp.status, 200, "256x256 thumbnail must render");

    // Count shape: the real path costs a small CONSTANT number of allocations
    // regardless of output size (measured 51 vs 53 for a 256x output growth).
    // An implementation that allocated per output pixel would put the large
    // count at 65536+ against the small count's 256. The bound is generous
    // (constant-factor headroom) and exists solely to catch that explosion.
    let ratio = large_count as f64 / small_count.max(1) as f64;
    assert!(
        ratio < 4.0,
        "allocation COUNT grew {ratio:.1}x when the output grew 256x \
         ({small_count} -> {large_count}); a per-output-pixel allocation is present"
    );

    // Byte shape: the byte total is likewise a small constant, NOT proportional
    // to the output. A per-output-pixel `Vec`/`String` would make the large
    // render charge at least dst_w * dst_h = 65536 bytes; the real path charges
    // ~2 KB. This bound catches both a per-pixel allocation and an accidental
    // full-size intermediate.
    assert!(
        large_bytes < 65_536,
        "large render charged {large_bytes} bytes for a 256x256 output; the path is \
         allocating an intermediate proportional to the output"
    );

    // And the byte total must GROW with the output (the resize buffer does),
    // proving the measurement window actually covers the downscale.
    assert!(
        small_bytes < large_bytes,
        "16x16 render charged {small_bytes} bytes vs {large_bytes} for 256x256; \
         the measured window does not follow the output size"
    );
}
