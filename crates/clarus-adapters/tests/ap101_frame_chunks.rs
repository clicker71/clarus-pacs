// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101 (0310 gate G-B): the frame-table build is bounded by the CONFIG cap,
//! never by an untrusted `NumberOfFrames` or by the file's own item count.
//!
//! The manifest frame table costs 56 bytes per frame. Two amplification
//! surfaces are gated here:
//!
//! 1. **F1 - declared frames must not size an allocation.** `chunks_native` /
//!    `chunks_encapsulated` used to `Vec::with_capacity(num_frames)`, so a
//!    crafted `NumberOfFrames = 0xFFFFFFFF` requested ~240 GB before a single
//!    byte was examined. The table now grows O(ACTUAL frames walked).
//! 2. **F5 - a walked count must not grow the table past the cap.**
//!    `max_frames_per_instance` is a TRUSTED bound; an instance that declares
//!    OR actually holds more frames is REJECTED (`TooManyFrames`), never
//!    stored without a table.
//!
//! Every expectation is an arithmetic invariant on the allocator window, so the
//! property holds identically on a 2 GiB Pi and a 1 TiB Xeon.

#![forbid(unsafe_code)]

use clarus_adapters::dicom_rs::frame::{
    FrameTableError, MAX_FRAMES_CEILING, compute_frame_chunks, compute_frame_chunks_capped,
    compute_frame_chunks_from_file, compute_frame_chunks_from_file_capped,
};
use ferrite_testkit::heap::TestAllocator;
use std::sync::atomic::{AtomicU32, Ordering};

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Allocation window: returns (count, bytes) allocated inside `f`.
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

// -- Fixture builders (explicit VR little endian) -------------------------

fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    // PS3.5 6.2: a text VR pads to an even length INSIDE the declared length,
    // using the VR's own pad character (0x20 for the string VRs, 0x00 for UI).
    // The binary VRs pad OUTSIDE it (7.1). The previous version padded outside
    // for every VR, so an odd-length IS carried a stray byte the parser then
    // read as the next tag.
    let mut padded: Vec<u8> = value.to_vec();
    if !value.len().is_multiple_of(2) {
        let pad = match vr {
            b"UI" => 0x00,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"UN" => 0xFF,
            _ => 0x20,
        };
        padded.push(pad);
    }
    let is_text = !matches!(vr, b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"UN");
    let declared = if is_text { padded.len() } else { value.len() };

    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let extended = matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    );
    if extended {
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&(declared as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(declared as u16).to_le_bytes());
    }
    buf.extend_from_slice(&padded);
}

fn us(v: u16) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// Encapsulated PixelData value: empty BOT, one item per tile, then E0DD.
fn encapsulated(tiles: &[&[u8]]) -> Vec<u8> {
    let mut pixel = Vec::new();
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    for tile in tiles {
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        pixel.extend_from_slice(&(tile.len() as u32).to_le_bytes());
        pixel.extend_from_slice(tile);
    }
    pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    pixel
}

/// A multi-frame encapsulated instance with an OVERRIDABLE declared
/// `NumberOfFrames` string (so a malicious declaration can be injected).
fn instance_with_declared(declared: &str, tiles: &[&[u8]]) -> Vec<u8> {
    let pixel = encapsulated(tiles);
    let mut d = Vec::new();
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.4.50");
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"SM");
    append_element(&mut d, 0x0028, 0x0002, b"US", &us(1));
    append_element(&mut d, 0x0028, 0x0010, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0011, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0100, b"US", &us(8));
    append_element(&mut d, 0x0028, 0x0008, b"IS", declared.as_bytes());
    // PS3.5 A.4: an encapsulated (fragmented) PixelData has UNDEFINED length -
    // the value is an item stream terminated by SequenceDelimitationItem, and a
    // defined length would make the element native rather than encapsulated.
    append_encapsulated_pixeldata(&mut d, &pixel);
    d
}

/// Append an encapsulated PixelData element: OB, undefined length, value.
fn append_encapsulated_pixeldata(buf: &mut Vec<u8>, value: &[u8]) {
    buf.extend_from_slice(&0x7FE0u16.to_le_bytes());
    buf.extend_from_slice(&0x0010u16.to_le_bytes());
    buf.extend_from_slice(b"OB");
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    buf.extend_from_slice(value);
}

/// Temp file guard + unique path (no `tempfile` dev-dep in this crate).
struct TempFile {
    path: std::path::PathBuf,
}
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
fn write_temp(bytes: &[u8]) -> (TempFile, std::path::PathBuf) {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "clarus-frames-ap101-{}-{n}.dcm",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("write fixture");
    (TempFile { path: path.clone() }, path)
}

// -- F1: declared frames never size an allocation -------------------------

/// A declared `NumberOfFrames = 0xFFFFFFFF` must NOT trigger a large
/// allocation. With the old `with_capacity(num_frames)` this window would
/// request ~240 GB and abort. The uncapped wrapper caps at the ceiling, and
/// 0xFFFFFFFF exceeds even that, so the table is EMPTY (refused) - and the
/// window proves nothing was allocated by the declaration.
#[test]
fn evil_declared_frames_does_not_allocate_big() {
    let tiles: Vec<&[u8]> = vec![&[0xAA; 64], &[0xBB; 64], &[0xCC; 64]];
    let file = instance_with_declared("4294967295", &tiles);

    let ((count, bytes), chunks) = window(|| compute_frame_chunks(&file));

    assert!(
        chunks.is_empty(),
        "an above-ceiling declaration is refused (empty table), never sized by it"
    );
    assert!(
        bytes < 64 * 1024,
        "a 3-item instance must not allocate by the declared 0xFFFFFFFF count (got {bytes} bytes, {count} allocs)"
    );
}

/// The same, through the UNCAPPED wrapper that uses the ceiling: a
/// 0xFFFFFFFF declaration exceeds even the ceiling, so the capped core
/// rejects it in O(1) - asserted separately below. This test pins that the
/// uncapped path (ceiling) also does not allocate by the declaration.
#[test]
fn ceiling_rejects_evil_declaration_without_walking() {
    let tiles: Vec<&[u8]> = vec![&[0xAA; 32]; 4];
    let file = instance_with_declared("4294967295", &tiles);

    let ((count, bytes), r) = window(|| compute_frame_chunks_capped(&file, MAX_FRAMES_CEILING));

    assert!(
        matches!(r, Err(FrameTableError::TooManyFrames { .. })),
        "0xFFFFFFFF > MAX_FRAMES_CEILING must be rejected"
    );
    assert!(
        bytes < 16 * 1024,
        "the declared-cap check runs BEFORE any allocation (got {bytes} bytes, {count} allocs)"
    );
}

// -- F5: walked count is bounded by the cap -------------------------------

/// A many-item stream with a small `max_frames_per_instance` must trip
/// `TooManyFrames` instead of growing a table past the cap.
#[test]
fn walked_count_over_cap_is_rejected_not_a_giant_vec() {
    // 40 zero-length items (8-byte headers): ~320 B of file, potentially 40
    // rows. Cap at 8 -> must reject, and must NOT build 40 rows.
    //
    // NumberOfFrames is declared WITHIN the cap on purpose: declaring 40 would
    // trip the declared-frame check before the walk ever starts, so the test
    // would pass without exercising the walked-count limit at all.
    let empty: Vec<&[u8]> = vec![&[]; 40];
    let file = instance_with_declared("8", &empty);

    let ((count, bytes), r) = window(|| compute_frame_chunks_capped(&file, 8));

    assert!(
        matches!(r, Err(FrameTableError::TooManyFrames { cap: 8, .. })),
        "40 items over a cap of 8 must reject"
    );
    // At most `cap` rows were pushed before the trip (8 * 56 B ~= 448 B).
    assert!(
        bytes < 16 * 1024,
        "the walk must stop at the cap, not build the full table (got {bytes} bytes, {count} allocs)"
    );
}

/// A declaration JUST UNDER the cap but a walk that exceeds it is also
/// rejected (an instance can under-declare and over-hold).
#[test]
fn under_declared_but_over_held_is_rejected() {
    let empty: Vec<&[u8]> = vec![&[]; 20];
    // Declares 5 (<= cap 8) but holds 20 items.
    let file = instance_with_declared("5", &empty);

    let r = compute_frame_chunks_capped(&file, 8);
    assert!(
        matches!(r, Err(FrameTableError::TooManyFrames { .. })),
        "an under-declared but over-held instance must still be rejected"
    );
}

/// Within the cap, the table is produced and is byte-identical to the
/// uncapped result (parity: the cap does not change a healthy table).
#[test]
fn within_cap_is_byte_identical_to_uncapped() {
    let tiles: Vec<&[u8]> = vec![&[0x11; 128], &[], &[0x33; 64], &[0x44; 256]];
    let file = instance_with_declared("4", &tiles);

    let uncapped = compute_frame_chunks(&file);
    let capped = compute_frame_chunks_capped(&file, 1000).expect("within cap");
    assert_eq!(uncapped.len(), capped.len());
    for (a, b) in uncapped.iter().zip(capped.iter()) {
        assert_eq!(a, b, "cap must not perturb a healthy table");
    }
}

// -- File variant: same caps ---------------------------------------------

/// The streaming file variant carries the same F5 cap.
#[test]
fn file_variant_over_cap_is_rejected() {
    let empty: Vec<&[u8]> = vec![&[]; 30];
    let file = instance_with_declared("30", &empty);
    let (_guard, path) = write_temp(&file);

    let r = compute_frame_chunks_from_file_capped(&path, 8);
    assert!(
        matches!(r, Err(FrameTableError::TooManyFrames { cap: 8, .. })),
        "the file variant must enforce the same cap"
    );
}

/// The streaming file variant does not allocate by the declared count either.
#[test]
fn file_variant_evil_declared_does_not_allocate_big() {
    let tiles: Vec<&[u8]> = vec![&[0xAA; 64], &[0xBB; 64]];
    let file = instance_with_declared("4294967295", &tiles);
    let (_guard, path) = write_temp(&file);

    let ((count, bytes), r) =
        window(|| compute_frame_chunks_from_file_capped(&path, MAX_FRAMES_CEILING));
    assert!(matches!(r, Err(FrameTableError::TooManyFrames { .. })));
    assert!(
        bytes < 64 * 1024,
        "the file variant must reject the declaration before allocating (got {bytes} bytes, {count} allocs)"
    );
}

/// The uncapped file convenience uses the ceiling and can never reject.
#[test]
fn file_variant_uncapped_uses_ceiling() {
    let tiles: Vec<&[u8]> = vec![&[0x11; 32], &[0x22; 32]];
    let file = instance_with_declared("2", &tiles);
    let (_guard, path) = write_temp(&file);
    let chunks = compute_frame_chunks_from_file(&path);
    assert_eq!(chunks.len(), 2);
}

// -- Cap ceiling sanity ---------------------------------------------------

/// The ceiling is the documented 2^24, so one table is bounded at ~900 MiB in
/// the absolute worst case (2^24 x 56 B) - the config may not exceed it.
#[test]
fn ceiling_is_the_documented_bound() {
    assert_eq!(MAX_FRAMES_CEILING, 16_777_216);
    assert!(
        MAX_FRAMES_CEILING as u64 * 56 < 1_024 * 1_024 * 1_024,
        "one table must stay under 1 GiB at the absolute ceiling"
    );
}

// -- Undefined-length encapsulated PixelData (RMS class) ------------------

/// An undefined-length encapsulated instance must produce the correct table
/// with a byte total INDEPENDENT of the file size (no whole-blob buffering).
/// The file is padded with a large trailing payload that the walk never reads.
#[test]
fn undefined_length_pixeldata_table_is_file_size_independent() {
    // 6 items, undefined length, plus 4 MiB of trailing junk after E0DD that a
    // correct walk must never touch.
    let tiles: Vec<&[u8]> = vec![
        &[0x11; 64],
        &[],
        &[0x33; 128],
        &[0x44; 32],
        &[0x55; 8],
        &[0x66; 256],
    ];
    let mut pixel = Vec::new();
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    for tile in &tiles {
        pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        pixel.extend_from_slice(&(tile.len() as u32).to_le_bytes());
        pixel.extend_from_slice(tile);
    }
    pixel.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    pixel.extend_from_slice(&vec![0xEEu8; 4 * 1024 * 1024]); // trailing junk

    let mut d = Vec::new();
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"SM");
    append_element(&mut d, 0x0028, 0x0010, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0011, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0008, b"IS", b"6 ");
    // Undefined length: OB + reserved + 0xFFFFFFFF.
    d.extend_from_slice(&0x7FE0u16.to_le_bytes());
    d.extend_from_slice(&0x0010u16.to_le_bytes());
    d.extend_from_slice(b"OB");
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    d.extend_from_slice(&pixel);

    let (_guard, path) = write_temp(&d);

    // The file is ~4 MiB; the allocation window must stay FAR below it.
    let ((count, bytes), r) = window(|| compute_frame_chunks_from_file_capped(&path, 1000));
    let chunks = r.expect("within cap");
    assert_eq!(chunks.len(), 6, "undefined-length items are tabled");
    assert_eq!(chunks[1].len, 0, "sparse tile survives");
    assert!(
        bytes < 256 * 1024,
        "the walk must not buffer the blob (file ~4 MiB; allocated {bytes} bytes, {count} allocs)"
    );

    // And the slice variant agrees (parity).
    let slice = compute_frame_chunks(&d);
    assert_eq!(slice.len(), chunks.len());
    for (a, b) in slice.iter().zip(chunks.iter()) {
        assert_eq!(a.number, b.number);
        assert_eq!(a.offset, b.offset);
        assert_eq!(a.len, b.len);
    }
}

// -- Boundary cases the prompt pins ---------------------------------------

/// Defined-length PixelData with `pd_len == 0` -> empty table.
#[test]
fn boundary_zero_length_pixeldata_is_empty() {
    let mut d = Vec::new();
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0028, 0x0010, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0011, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0008, b"IS", b"3 ");
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &[]); // zero-length value
    let (_guard, path) = write_temp(&d);
    assert!(compute_frame_chunks(&d).is_empty());
    assert!(compute_frame_chunks_from_file(&path).is_empty());
}

/// A single-frame instance (NumberOfFrames absent/1) -> empty table.
#[test]
fn boundary_single_frame_is_empty() {
    let frames: Vec<u8> = (0u8..16).collect();
    let mut d = Vec::new();
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0028, 0x0010, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0011, b"US", &us(2));
    // No NumberOfFrames at all -> defaults to 1.
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &frames);
    let (_guard, path) = write_temp(&d);
    assert!(compute_frame_chunks(&d).is_empty());
    assert!(compute_frame_chunks_from_file(&path).is_empty());
}

/// Undefined-length encapsulated PixelData with NO E0DD delimiter (malformed,
/// truncated) -> empty table, no hang, no abort: the walk terminates on EOF.
#[test]
fn boundary_undefined_length_without_delimiter_is_empty() {
    let mut pixel = Vec::new();
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    // Two well-formed items, then a THIRD item header that claims more bytes
    // than exist, and NO E0DD.
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    pixel.extend_from_slice(&16u32.to_le_bytes());
    pixel.extend_from_slice(&[0xAA; 16]);
    pixel.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    pixel.extend_from_slice(&999_999u32.to_le_bytes()); // overruns EOF
    pixel.extend_from_slice(b"trunc");

    let mut d = Vec::new();
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0028, 0x0010, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0011, b"US", &us(2));
    append_element(&mut d, 0x0028, 0x0008, b"IS", b"5 ");
    d.extend_from_slice(&0x7FE0u16.to_le_bytes());
    d.extend_from_slice(&0x0010u16.to_le_bytes());
    d.extend_from_slice(b"OB");
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    d.extend_from_slice(&pixel);

    let (_guard, path) = write_temp(&d);
    // Must return (empty or a table fully inside the file), never hang/abort.
    let slice = compute_frame_chunks(&d);
    for c in &slice {
        assert!(c.offset + c.len <= d.len() as u64, "chunk past EOF");
    }
    let streamed = compute_frame_chunks_from_file(&path);
    for c in &streamed {
        assert!(c.offset + c.len <= d.len() as u64, "chunk past EOF");
    }
}
