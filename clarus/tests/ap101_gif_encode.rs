// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: allocation gate for the hand-rolled GIF encoder.
//!
//! `/rendered` is on the WADO-RS hot path, so the encoder must satisfy the
//! ap101 discipline. The GIF encoder differs from the PNG one in one respect:
//! LZW output size is DATA-DEPENDENT (it compresses), so `len == capacity` is
//! not achievable - the exact thing the gate can pin is the CAPACITY, which is
//! the fixed `worst_case_len` value, plus the allocation COUNT, which must not
//! grow with the pixel count.
//!
//! So the gate asserts:
//!   - allocation count == 2 at both sizes (the presized output buffer and the
//!     flat LZW dictionary - no third allocation anywhere in the loop);
//!   - `body.capacity() == worst_case_len(pixels)` (sized exactly, never grown);
//!   - `body.len() <= worst_case_len(pixels)` (the bound holds and compresses);
//!   - the container markers are correct.
//!
//! NOTE on the byte counter: the encoder's dictionary is freed at the end of
//! `encode`, and `TestAllocator::dealloc` subtracts bytes on the same thread,
//! so the dictionary's 2 MiB allocation-and-free nets to zero inside the
//! measured window. The COUNT is the invariant that cannot cancel, which is
//! why the count assertion (not a byte total) is the ap101 gate here.
//!
//! The pixel-correctness proof is the ESCALATION ROUND-TRIP below, which
//! decodes with Pillow - an implementation independent of this encoder.

use clarus_adapters::gif::{ColorType, encode, worst_case_len};
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

/// Deterministic Gray8 ramp.
fn ramp(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// Incompressible Gray8 bytes (xorshift): the dictionary fills, the code width
/// widens 9 -> 10 -> 11 -> 12, and a Clear reset fires - the paths a
/// ramp-compressible image never reaches.
fn incompressible(n: usize) -> Vec<u8> {
    let mut s: u32 = 0x1234_5678;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 24) as u8
        })
        .collect()
}

/// Dictionary bytes: `1 << 20` `u16` slots = 2 MiB. Kept for the doc record;
/// the byte counter nets it to zero because `encode` frees it before returning.
const DICT_BYTES: usize = (1 << 20) * 2;

#[test]
fn gif_encode_is_two_allocations_with_exact_capacity() {
    // Warm first-touch statics so the measured window is the encoder alone.
    let _ = encode(&[0u8; 16 * 16], 16, 16, ColorType::Gray).expect("warmup");
    let _ = DICT_BYTES;

    for (w, h) in [(16u32, 16u32), (256u32, 256u32)] {
        let src = ramp((w * h) as usize);
        let expect = worst_case_len((w * h) as usize);

        let ((count, _bytes), body) =
            measure(|| encode(&src, w, h, ColorType::Gray).expect("encode"));

        assert_eq!(
            count, 2,
            "encode({w}x{h}) must allocate exactly twice (output + dictionary), got {count}"
        );
        assert_eq!(
            body.capacity(),
            expect,
            "encode({w}x{h}) capacity must be the exact worst-case bound"
        );
        assert!(
            body.len() <= expect,
            "encode({w}x{h}) produced {} bytes, over the {expect} bound",
            body.len()
        );
        assert_eq!(&body[..6], b"GIF89a", "header");
        assert_eq!(body[body.len() - 1], 0x3B, "trailer");
    }
}

#[test]
fn gif_encode_count_is_constant_on_incompressible_input() {
    // Same gate on the escalation path: an image that widens the code width and
    // resets the dictionary must still cost exactly two allocations.
    let (w, h) = (256u32, 256u32);
    let src = incompressible((w * h) as usize);
    let _ = encode(&incompressible(256), 16, 16, ColorType::Gray).expect("warmup");

    let expect = worst_case_len((w * h) as usize);
    let ((count, _bytes), body) = measure(|| encode(&src, w, h, ColorType::Gray).expect("encode"));
    assert_eq!(
        count, 2,
        "incompressible encode must allocate exactly twice"
    );
    assert_eq!(body.capacity(), expect);
    assert!(body.len() <= expect);
}

/// ESCALATION ROUND-TRIP - the Task 1 acceptance test. Writes the encoded
/// stream to disk and decodes it with Pillow (`Image.open().convert("L")`),
/// which is an INDEPENDENT LZW implementation. Pixel identity proves the
/// encoder's width-escalation and Clear-reset timing are correct, not merely
/// self-consistent.
///
/// If Pillow is unavailable the test FAILS rather than skipping: an unverified
/// compressor on the hot path is not acceptable, and a silent skip would hide
/// exactly the defect this test exists to catch.
#[test]
fn gif_escalation_round_trips_through_pillow() {
    let dir = std::env::temp_dir().join("clarus_gif_roundtrip");
    std::fs::create_dir_all(&dir).expect("temp dir");

    let cases: [(&str, u32, u32, Vec<u8>); 4] = [
        ("ramp16", 16, 16, ramp(256)),
        ("ramp256", 256, 256, ramp(65536)),
        ("rand256", 256, 256, incompressible(65536)),
        ("ramp37x19", 37, 19, ramp(703)),
    ];

    let mut script = String::new();
    for (name, w, h, src) in &cases {
        let body = encode(src, *w, *h, ColorType::Gray).expect("encode");
        std::fs::write(dir.join(format!("{name}.gif")), &body).unwrap();
        std::fs::write(dir.join(format!("{name}.raw")), src).unwrap();
        script.push_str(&format!(
            "im = Image.open(r'{d}/{n}.gif').convert('L')\n\
             exp = open(r'{d}/{n}.raw','rb').read()\n\
             got = im.tobytes()\n\
             assert got == exp, '{n}: pixel mismatch ({n}={w}x{h})'\n",
            d = dir.display(),
            n = name
        ));
    }
    let script = format!("import sys\nfrom PIL import Image\n{script}print('ROUNDTRIP_OK')\n");
    let script_path = dir.join("check.py");
    std::fs::write(&script_path, &script).unwrap();

    let out = std::process::Command::new("python")
        .arg(&script_path)
        .output()
        .expect("python not found: the GIF escalation round-trip requires Python with Pillow");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "Pillow round-trip failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("ROUNDTRIP_OK"),
        "Pillow round-trip did not confirm.\nstdout: {stdout}\nstderr: {stderr}"
    );
}
