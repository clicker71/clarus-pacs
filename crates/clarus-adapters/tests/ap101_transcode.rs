// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101 transcode gates.
//!
//! The transcode pipeline allocates BY DESIGN (decode buffers sized
//! rows*cols*spp, color conversion, output rebuild), so the ap101 0-alloc
//! discipline applies to the hot CONTROL paths, not the pixel buffers:
//!
//! 1. No-op passthrough (`source == target`): exactly 2 allocations - the
//!    TS String + the output copy. No registry/tag-scan allocations beyond
//!    that on this hot path (the PALETTE COLOR PI check is 0-alloc).
//! 2. RLE multi-frame decode (item-wrapped field shape, 01.09): a bounded
//!    allocation count - pins that PackBits decode never allocates per
//!    pixel and the stream parse stays O(frames + tags), no realloc storm.
//! 3. Palette-to-RGB transcode (01.09 owner request): a bounded allocation
//!    count - the RGB buffer is ONE exact-sized allocation per transcode;
//!    per-pixel pushes would blow the ceiling. Plus a behavior gate: a
//!    PALETTE COLOR source with source == target must NOT be a passthrough
//!    (the browser miniviewer cannot render palettes).
//!
//! Measured on Windows (01.09): passthrough = 2 events; RLE decode of the
//! 2-frame fixture = 15 events; palette transcode of the 4-pixel fixture =
//! 28 events (RLE decode + 3 LUT reads + 1 RGB buffer + EVRLE encode chain).
//! Ceilings carry small headroom for std internals across platforms (the
//! cas_rel_path gate precedent).

use clarus_adapters::codec::explicit_vr_le::ExplicitVrLeCodec;
use clarus_adapters::codec::rle::RleCodec;
use clarus_adapters::codec::{CodecRegistry, DicomCodec};
use clarus_adapters::transcode::transcode_dicom;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Run `f` inside the allocator window; return (event_count, byte_count, result).
fn count_env<F, R>(f: F) -> (usize, usize, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, bytes) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n, bytes, result)
}

/// Build a minimal RLE DICOM: preamble + meta TS=RLE + Rows/Cols/SPP/Bits/
/// NumberOfFrames(IS)/PI + PixelData (defined length).
fn build_rle_dicom(stream: &[u8], rows: u16, cols: u16, frames: u16) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");
    let ts = b"1.2.840.10008.1.2.5"; // 19 chars, odd -> physical pad byte
    out.extend_from_slice(&0x0002u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"UI");
    out.extend_from_slice(&(ts.len() as u16).to_le_bytes());
    out.extend_from_slice(ts);
    out.push(0x00);

    let us_tag = |d: &mut Vec<u8>, tag: (u16, u16), v: u16| {
        d.extend_from_slice(&tag.0.to_le_bytes());
        d.extend_from_slice(&tag.1.to_le_bytes());
        d.extend_from_slice(b"US");
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&v.to_le_bytes());
    };
    us_tag(&mut out, (0x0028, 0x0010), rows);
    us_tag(&mut out, (0x0028, 0x0011), cols);
    us_tag(&mut out, (0x0028, 0x0002), 1);
    us_tag(&mut out, (0x0028, 0x0100), 8);
    if frames > 1 {
        let s = frames.to_string();
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0008u16.to_le_bytes());
        out.extend_from_slice(b"IS");
        out.extend_from_slice(&(s.len() as u16).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
        out.push(b' ');
    }
    let pi = b"MONOCHROME2"; // 11 chars, odd -> pad byte
    out.extend_from_slice(&0x0028u16.to_le_bytes());
    out.extend_from_slice(&0x0004u16.to_le_bytes());
    out.extend_from_slice(b"CS");
    out.extend_from_slice(&(pi.len() as u16).to_le_bytes());
    out.extend_from_slice(pi);
    out.push(b' ');

    out.extend_from_slice(&0x7FE0u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"OB");
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(&(stream.len() as u32).to_le_bytes());
    out.extend_from_slice(stream);
    out
}

fn rle_item(payload: &[u8]) -> Vec<u8> {
    let mut it = Vec::new();
    it.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    it.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    it.extend_from_slice(payload);
    it
}

fn packbits_encode(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    let mut i = 0usize;
    let len = data.len();
    while i < len {
        let mut run_len = 1;
        while i + run_len < len && data[i + run_len] == data[i] && run_len < 128 {
            run_len += 1;
        }
        if run_len >= 3 {
            output.push(1u8.wrapping_sub(run_len as u8));
            output.push(data[i]);
            i += run_len;
        } else {
            let literal_start = i;
            i += 1;
            while i < len {
                let mut future_run = 1;
                while i + future_run < len && data[i + future_run] == data[i] && future_run < 3 {
                    future_run += 1;
                }
                if future_run >= 3 {
                    break;
                }
                i += 1;
                if i - literal_start >= 128 {
                    break;
                }
            }
            let literal_count = i - literal_start;
            output.push(literal_count as u8 - 1);
            output.extend_from_slice(&data[literal_start..i]);
        }
    }
    output
}

/// GATE 1: no-op passthrough (source == target) allocates exactly twice -
/// the transfer-syntax String in `extract_transfer_syntax` and the output
/// copy. No registry/tag-scan allocations beyond that on this hot path.
#[test]
fn transcode_noop_passthrough_exactly_one_alloc() {
    // Fixture OUTSIDE the window.
    let bytes = build_rle_dicom(&[0x00; 64], 1, 1, 1);
    let registry = CodecRegistry::default();

    let (n, _, out) = count_env(|| {
        transcode_dicom(
            &bytes,
            clarus_adapters::codec::ts_uid::RLE_LOSSLESS,
            &registry,
        )
        .expect("no-op transcode must succeed")
    });
    assert_eq!(
        n, 2,
        "passthrough must allocate exactly twice (TS String + output copy)"
    );
    assert_eq!(out, bytes, "passthrough must return the input bytes");
}

/// GATE 2: RLE multi-frame decode (item-wrapped field shape, 01.09) stays
/// within a bounded allocation count - no per-pixel allocations, no realloc
/// storms in the PackBits/stream-parse path.
#[test]
fn rle_multiframe_decode_alloc_budget() {
    // Fixture OUTSIDE the window: 2 frames, 4x4, item-wrapped with BOT.
    let frame0 = [
        10u8, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
    ];
    let frame1 = [200u8; 16];
    let make_blob = |pixels: &[u8]| {
        let mut header = vec![0u8; 64];
        header[0..4].copy_from_slice(&1u32.to_le_bytes());
        header[4..8].copy_from_slice(&64u32.to_le_bytes());
        let mut blob = header;
        blob.extend_from_slice(&packbits_encode(pixels));
        blob
    };
    let f0 = make_blob(&frame0);
    let f1 = make_blob(&frame1);
    let mut bot = Vec::new();
    bot.extend_from_slice(&0u32.to_le_bytes());
    bot.extend_from_slice(&(8 + f0.len() as u32).to_le_bytes());
    let mut encapsulated = rle_item(&bot);
    encapsulated.extend_from_slice(&rle_item(&f0));
    encapsulated.extend_from_slice(&rle_item(&f1));

    let bytes = {
        let b = build_rle_dicom(&[], 4, 4, 2);
        // Replace the empty defined-length PixelData with the item-wrapped
        // encapsulated form: find the PixelData tag and rebuild.
        let pix_pos = b
            .windows(4)
            .position(|w| w == [0xE0, 0x7F, 0x10, 0x00])
            .expect("PixelData tag");
        let mut d = b[..pix_pos].to_vec();
        d.extend_from_slice(&0x7FE0u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"OB");
        d.extend_from_slice(&[0x00, 0x00]);
        d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        d.extend_from_slice(&encapsulated);
        d.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        d.extend_from_slice(&0u32.to_le_bytes());
        d
    };

    let codec = RleCodec;
    let (n, _, pixels) = count_env(|| codec.decode(&bytes).expect("RLE decode must succeed"));
    // Measured: 15 events on Windows (see module doc). A per-pixel
    // allocation would blow far past this ceiling for a 16-pixel fixture.
    assert!(
        n <= 20,
        "RLE multi-frame decode must stay within 20 allocations, got {n}"
    );
    assert_eq!(
        pixels.data,
        [
            10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160, // frame 0
            200, 200, 200, 200, 200, 200, 200, 200, 200, 200, 200, 200, 200, 200, 200,
            200, // frame 1
        ],
        "decoded frames must be correct and in order"
    );
}

/// Build a single-frame RLE DICOM with PALETTE COLOR + a 4-entry LUT.
fn build_palette_rle_dicom(stream: &[u8], rows: u16, cols: u16) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");
    let ts = b"1.2.840.10008.1.2.5";
    out.extend_from_slice(&0x0002u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"UI");
    out.extend_from_slice(&(ts.len() as u16).to_le_bytes());
    out.extend_from_slice(ts);
    out.push(0x00);

    let us_tag = |d: &mut Vec<u8>, tag: (u16, u16), v: u16| {
        d.extend_from_slice(&tag.0.to_le_bytes());
        d.extend_from_slice(&tag.1.to_le_bytes());
        d.extend_from_slice(b"US");
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&v.to_le_bytes());
    };
    us_tag(&mut out, (0x0028, 0x0010), rows);
    us_tag(&mut out, (0x0028, 0x0011), cols);
    us_tag(&mut out, (0x0028, 0x0002), 1);
    us_tag(&mut out, (0x0028, 0x0100), 8);

    // PI = PALETTE COLOR (13 chars, odd -> pad)
    let pi = b"PALETTE COLOR";
    out.extend_from_slice(&0x0028u16.to_le_bytes());
    out.extend_from_slice(&0x0004u16.to_le_bytes());
    out.extend_from_slice(b"CS");
    out.extend_from_slice(&(pi.len() as u16).to_le_bytes());
    out.extend_from_slice(pi);
    out.push(b' ');

    // Descriptor (0028,1101): [4 entries, first mapped 0]
    out.extend_from_slice(&0x0028u16.to_le_bytes());
    out.extend_from_slice(&0x1101u16.to_le_bytes());
    out.extend_from_slice(b"US");
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    // LUTs (0028,1201-1203) OW, 4 entries each, 8-bit*256 values.
    let luts: [[u16; 4]; 3] = [
        [0, 85 << 8, 170 << 8, 255 << 8],
        [0, 170 << 8, 85 << 8, 255 << 8],
        [255 << 8, 128 << 8, 64 << 8, 0],
    ];
    for (elem, lut) in [
        (0x1201u16, &luts[0]),
        (0x1202, &luts[1]),
        (0x1203, &luts[2]),
    ] {
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(b"OW");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&8u32.to_le_bytes());
        for v in lut {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    // PixelData (7FE0,0010) OB defined length (odd -> physical pad byte)
    out.extend_from_slice(&0x7FE0u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"OB");
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(&(stream.len() as u32).to_le_bytes());
    out.extend_from_slice(stream);
    if !stream.len().is_multiple_of(2) {
        out.push(0x00);
    }
    out
}

/// GATE 3: palette-to-RGB transcode allocates within a bounded count - the
/// RGB output is ONE exact-sized allocation; per-pixel pushes would blow it.
#[test]
fn palette_transcode_alloc_budget() {
    // Fixture OUTSIDE the window: 2x2 palette RLE, samples [0,1,2,3].
    let mut header = vec![0u8; 64];
    header[0..4].copy_from_slice(&1u32.to_le_bytes());
    header[4..8].copy_from_slice(&64u32.to_le_bytes());
    let mut blob = header;
    blob.extend_from_slice(&packbits_encode(&[0, 1, 2, 3]));
    let bytes = build_palette_rle_dicom(&blob, 2, 2);

    let mut registry = CodecRegistry::new();
    registry.register(Box::new(RleCodec));
    registry.register(Box::new(ExplicitVrLeCodec));

    let (n, _, out) = count_env(|| {
        transcode_dicom(
            &bytes,
            clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE,
            &registry,
        )
        .expect("palette transcode must succeed")
    });
    // Measured on Windows: 28 events (see module doc). A per-pixel
    // allocation on any 2x2 image would dwarf this ceiling.
    assert!(
        n <= 45,
        "palette transcode must stay within 45 allocations, got {n}"
    );
    // Output must be 3-sample RGB: SamplesPerPixel (0028,0002) value = 3.
    let spp_pos = out
        .windows(4)
        .position(|w| w == [0x28, 0x00, 0x02, 0x00])
        .expect("SamplesPerPixel tag present in output");
    let spp = u16::from_le_bytes([out[spp_pos + 8], out[spp_pos + 9]]);
    assert_eq!(spp, 3, "palette transcode must emit 3-sample RGB");
}

/// GATE 4 (behavior): a PALETTE COLOR source with source == target must NOT
/// be a no-op passthrough - the pipeline falls through to palette-to-RGB so
/// the browser miniviewer gets renderable RGB bytes.
#[test]
fn palette_same_target_must_convert_not_passthrough() {
    // Explicit VR LE (EVRLE) dataset, 1 sample, PALETTE COLOR, 4 indices.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0u8; 128]);
    bytes.extend_from_slice(b"DICM");
    let ts = b"1.2.840.10008.1.2.1"; // 19 chars, odd -> pad
    bytes.extend_from_slice(&0x0002u16.to_le_bytes());
    bytes.extend_from_slice(&0x0010u16.to_le_bytes());
    bytes.extend_from_slice(b"UI");
    bytes.extend_from_slice(&(ts.len() as u16).to_le_bytes());
    bytes.extend_from_slice(ts);
    bytes.push(0x00);
    let us_tag = |d: &mut Vec<u8>, tag: (u16, u16), v: u16| {
        d.extend_from_slice(&tag.0.to_le_bytes());
        d.extend_from_slice(&tag.1.to_le_bytes());
        d.extend_from_slice(b"US");
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&v.to_le_bytes());
    };
    us_tag(&mut bytes, (0x0028, 0x0010), 2);
    us_tag(&mut bytes, (0x0028, 0x0011), 2);
    us_tag(&mut bytes, (0x0028, 0x0002), 1);
    us_tag(&mut bytes, (0x0028, 0x0100), 8);
    let pi = b"PALETTE COLOR";
    bytes.extend_from_slice(&0x0028u16.to_le_bytes());
    bytes.extend_from_slice(&0x0004u16.to_le_bytes());
    bytes.extend_from_slice(b"CS");
    bytes.extend_from_slice(&(pi.len() as u16).to_le_bytes());
    bytes.extend_from_slice(pi);
    bytes.push(b' ');
    bytes.extend_from_slice(&0x0028u16.to_le_bytes());
    bytes.extend_from_slice(&0x1101u16.to_le_bytes());
    bytes.extend_from_slice(b"US");
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    for elem in [0x1201u16, 0x1202, 0x1203] {
        bytes.extend_from_slice(&0x0028u16.to_le_bytes());
        bytes.extend_from_slice(&elem.to_le_bytes());
        bytes.extend_from_slice(b"OW");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&(255u16 << 8).to_le_bytes());
        bytes.extend_from_slice(&(128u16 << 8).to_le_bytes());
        bytes.extend_from_slice(&(64u16 << 8).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    bytes.extend_from_slice(&0x7FE0u16.to_le_bytes());
    bytes.extend_from_slice(&0x0010u16.to_le_bytes());
    bytes.extend_from_slice(b"OW");
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&[0, 1, 2, 3]);

    let mut registry = CodecRegistry::new();
    registry.register(Box::new(RleCodec));
    registry.register(Box::new(ExplicitVrLeCodec));

    let out = transcode_dicom(
        &bytes,
        clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE,
        &registry,
    )
    .expect("palette fall-through must succeed");
    assert_ne!(out, bytes, "palette source must NOT pass through unchanged");
    let spp_pos = out
        .windows(4)
        .position(|w| w == [0x28, 0x00, 0x02, 0x00])
        .expect("SamplesPerPixel tag present in output");
    let spp = u16::from_le_bytes([out[spp_pos + 8], out[spp_pos + 9]]);
    assert_eq!(spp, 3, "palette fall-through must emit 3-sample RGB");
}
