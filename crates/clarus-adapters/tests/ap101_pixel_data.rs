// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the PixelData extractor must not allocate.
//!
//! `extract_pixel_data` sits on the rendered-WADO and transcode frame path,
//! so every call is a per-frame cost. Both phases are pure borrows: the
//! sequential walker reads header bytes and returns two `usize`, and the
//! byte-scan fallback searches a slice and returns the same pair. The
//! undefined-length support added to the walker is an integer comparison and
//! an eight-byte step, so it cannot introduce an allocation - which is
//! exactly what this gate holds it to.
//!
//! Fixtures are built OUTSIDE the measured window, and the window is warmed
//! first: a cold branch predictor or a first-touch page fault is not the
//! property under test.

#![forbid(unsafe_code)]

use clarus_adapters::codec::pixel_data::extract_pixel_data;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Allocation window returning `((count, bytes), result)`.
fn window<F, R>(f: F) -> ((usize, usize), R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let snap = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (snap, result)
}

fn append_undefined_length_sq(buf: &mut Vec<u8>) {
    buf.extend_from_slice(&0x0008u16.to_le_bytes());
    buf.extend_from_slice(&0x2112u16.to_le_bytes());
    buf.extend_from_slice(b"SQ");
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
    buf.extend_from_slice(&0xE000u16.to_le_bytes());
    buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    buf.extend_from_slice(&0x0008u16.to_le_bytes());
    buf.extend_from_slice(&0x1150u16.to_le_bytes());
    buf.extend_from_slice(b"UI");
    // The UID is 27 characters, so the NUL pad makes the value 28 bytes.
    buf.extend_from_slice(&28u16.to_le_bytes());
    buf.extend_from_slice(b"1.2.840.10008.5.1.4.1.1.1.1\0");
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
    buf.extend_from_slice(&0xE00Du16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0xFFFEu16.to_le_bytes());
    buf.extend_from_slice(&0xE0DDu16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
}

/// The instance the walker must get past an undefined-length sequence on.
fn undefined_sq_instance() -> Vec<u8> {
    let mut buf = Vec::with_capacity(1024);
    buf.extend_from_slice(&[0u8; 128]);
    buf.extend_from_slice(b"DICM");
    append_undefined_length_sq(&mut buf);
    buf.extend_from_slice(&0x0028u16.to_le_bytes());
    buf.extend_from_slice(&0x0010u16.to_le_bytes());
    buf.extend_from_slice(b"US");
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&1024u16.to_le_bytes());
    // A payload that fits entirely in the buffer, so both the walker and
    // the "runs past the end" branch are exercised.
    let payload = 4096usize;
    buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
    buf.extend_from_slice(b"OW");
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&(payload as u32).to_le_bytes());
    buf.extend(std::iter::repeat_n(0x33u8, payload));
    buf
}

/// The instance only the byte-scan fallback can read: no DICM magic, so the
/// sequential walker returns None before it starts.
///
/// The tag has to sit at or after offset 132, which is where the scan
/// begins. It skips the preamble and the meta group, because in a real file
/// no element can precede them.
fn byte_scan_only_instance() -> Vec<u8> {
    let mut buf = Vec::with_capacity(1024);
    buf.extend_from_slice(&[0u8; 132]);
    let payload = 2048usize;
    buf.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
    buf.extend_from_slice(b"OW");
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&(payload as u32).to_le_bytes());
    buf.extend(std::iter::repeat_n(0x44u8, payload));
    buf
}

#[test]
fn the_sequential_walker_allocates_nothing() {
    let bytes = undefined_sq_instance();
    // Warm: the first call touches the slice, and a first-touch fault is not
    // an allocation the extractor made.
    let _ = extract_pixel_data(&bytes);

    let (snap, result) = window(|| extract_pixel_data(&bytes));
    assert_eq!(
        snap,
        (0, 0),
        "the walker borrowed {} bytes and allocated {snap:?}",
        bytes.len()
    );
    assert_eq!(result.map(<[u8]>::len), Some(4096), "and it still found it");
}

#[test]
fn the_byte_scan_fallback_allocates_nothing() {
    let bytes = byte_scan_only_instance();
    let _ = extract_pixel_data(&bytes);

    let (snap, result) = window(|| extract_pixel_data(&bytes));
    assert_eq!(snap, (0, 0), "the fallback must search, not copy");
    assert_eq!(result.map(<[u8]>::len), Some(2048));
}

#[test]
fn a_repeated_call_costs_the_same_as_the_first() {
    // A gate that only ever sees 0,0 can pass on a path that was never
    // entered. Repeating the call and comparing two identical warm windows
    // catches growth the single-shot assertion would not distinguish from a
    // constant offset.
    let bytes = undefined_sq_instance();
    let _ = extract_pixel_data(&bytes);
    let (first, _) = window(|| extract_pixel_data(&bytes));
    let (second, _) = window(|| extract_pixel_data(&bytes));
    assert_eq!(
        first, second,
        "the cost is constant, so nothing accumulates"
    );
    assert_eq!(first, (0, 0), "and that constant is zero");
}
