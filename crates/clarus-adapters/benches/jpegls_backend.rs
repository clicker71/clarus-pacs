// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! JPEG-LS bench: pure_jpegls on the JPEG-LS 4.80 image.
//!
//! - decode 4.80 (776x776x16) through the DicomCodec trait. The stored
//!   4.80 stream is LegacyGo - probe: T87 decode fails, LegacyGo decode OK -
//!   so the stored bytes are a pure-only context arm.
//! - encode lossless from the decoded pixels: pure_jpegls LegacyGo (frozen
//!   production bytes) and pure_jpegls T87
//! - END-TO-END roundtrip decode+encode 4.80 through the trait
//! - context arm 4.70 -> 4.90 (JPEG-lossless SV1 -> J2K native), no
//!   JPEG-LS backend involved, measured once per build for the transcode
//!   wall number
//!
//! The arms are `#[cfg(feature = "transcode")]` (the jpegls/jpeg_lossless/
//! jpeg2000 codec modules are transcode-gated in codec/mod.rs). The DEFAULT
//! build compiles to a single no-op bench so the debug compile gate stays
//! green without the codec features; the arms run with `--features transcode`:
//!   cargo bench -p clarus-adapters --features transcode

#![forbid(unsafe_code)]

#[cfg(feature = "transcode")]
use criterion::black_box;
use criterion::{Criterion, criterion_group, criterion_main};

#[cfg(feature = "transcode")]
use clarus_adapters::codec::jpeg_lossless::JpegLosslessSv1Codec;
#[cfg(feature = "transcode")]
use clarus_adapters::codec::jpeg2000_native::Jpeg2000NativeLosslessCodec;
#[cfg(feature = "transcode")]
use clarus_adapters::codec::jpegls::JpeglsLosslessCodec;
#[cfg(feature = "transcode")]
use clarus_adapters::codec::{DecodedPixels, DicomCodec, DicomMeta};

// Fixtures live in the repo-wide test-images/ directory. Patients are
// anonymized; the compressed codestreams are untouched, so the numbers are
// comparable with earlier runs.
#[cfg(feature = "transcode")]
const F776_4_80: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-images/jpegls_4_80_776x776.dcm"
);
#[cfg(feature = "transcode")]
const BIG_A_4_70: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-images/jpeg_lossless_4_70_2586x2411.dcm"
);
#[cfg(feature = "transcode")]
const BIG_B_4_70: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-images/jpeg_lossless_4_70_2149x1635.dcm"
);
#[cfg(feature = "transcode")]
const BIG_C_4_70: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-images/jpeg_lossless_4_70_2320x1586.dcm"
);

#[cfg(feature = "transcode")]
fn read_fixture(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("fixture {path} unreadable: {e}"))
}

/// Minimal explicit-VR LE dataset carrying a JPEG-LS codestream (same shape
/// as the wrapper in codec/jpegls.rs tests; the bench cannot call it - it is
/// test-scoped).
#[cfg(feature = "transcode")]
fn build_minimal_dicom_wrapper(pixel_data: &[u8], w: u16, h: u16, bits: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");
    // (0002,0010) TransferSyntaxUID = JPEG-LS Lossless
    let ts = b"1.2.840.10008.1.2.4.80";
    out.extend_from_slice(&0x0002u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"UI");
    out.extend_from_slice(&(ts.len() as u16).to_le_bytes());
    out.extend_from_slice(ts);
    for (tag, val) in [
        ((0x0028u16, 0x0010u16), w),     // Rows
        ((0x0028, 0x0011), h),           // Columns
        ((0x0028, 0x0002), 1u16),        // SamplesPerPixel
        ((0x0028, 0x0100), bits as u16), // BitsAllocated
    ] {
        out.extend_from_slice(&tag.0.to_le_bytes());
        out.extend_from_slice(&tag.1.to_le_bytes());
        out.extend_from_slice(b"US");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }
    // (0028,0004) PhotometricInterpretation = MONOCHROME2
    let photo = b"MONOCHROME2";
    out.extend_from_slice(&0x0028u16.to_le_bytes());
    out.extend_from_slice(&0x0004u16.to_le_bytes());
    out.extend_from_slice(b"CS");
    out.extend_from_slice(&(photo.len() as u16).to_le_bytes());
    out.extend_from_slice(photo);
    // PS3.5 6.2: an odd-length value is padded to even, and the pad byte is
    // NOT part of the value length. MONOCHROME2 is 11 bytes, so without the
    // pad every element after it is read one byte early and the bench measures
    // a decoder fed a malformed dataset.
    if !photo.len().is_multiple_of(2) {
        out.push(0x20); // space, the CS pad character
    }
    // (7FE0,0010) PixelData, OB, undefined length + BOT + one fragment + delim
    out.extend_from_slice(&0x7FE0u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"OB");
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
    out.extend_from_slice(&(pixel_data.len() as u32).to_le_bytes());
    out.extend_from_slice(pixel_data);
    out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// Decode F776 once; expose planar u8 + u16 views + minimal meta, and build
/// a T.87 re-encoded fixture (the stored 4.80 stream is LegacyGo - verified
/// with a probe: T87 decode fails, LegacyGo decode OK).
#[cfg(feature = "transcode")]
fn setup_f776() -> (Vec<u8>, Vec<u8>, DecodedPixels, Vec<u16>, DicomMeta) {
    let bytes = read_fixture(F776_4_80);
    let pixels = JpeglsLosslessCodec
        .decode(&bytes)
        .expect("setup: F776 4.80 decode failed");
    pixels.validate().expect("setup: decoded pixels invalid");
    let u16_pixels: Vec<u16> = pixels
        .data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let meta = DicomMeta {
        rows: pixels.rows,
        columns: pixels.columns,
        samples_per_pixel: pixels.samples_per_pixel,
        bits_allocated: pixels.bits_allocated,
        bits_stored: pixels.bits_allocated,
        photometric_interpretation: pixels.photometric_interpretation.clone(),
        planar_configuration: None,
        pixel_representation: 0,
        frames: 1,
    };
    // T.87 re-encode of the same pixels -> wrapped dataset for the fair
    // pure arms (both decode the same T.87 codestream).
    let mut out = Vec::new();
    let mut opts = jpegls::EncodeOptions::default();
    opts.near = 0;
    opts.profile = jpegls::Profile::T87;
    opts.precision = None;
    jpegls::encode_with_options(
        &u16_pixels,
        u32::from(pixels.columns),
        u32::from(pixels.rows),
        &opts,
        &mut out,
    )
    .expect("setup: T87 re-encode failed");
    let t87_bytes =
        build_minimal_dicom_wrapper(&out, pixels.rows, pixels.columns, pixels.bits_allocated);
    (bytes, t87_bytes, pixels, u16_pixels, meta)
}

#[cfg(feature = "transcode")]
fn decode_4_80(c: &mut Criterion) {
    let (corpus_bytes, t87_bytes, ..) = setup_f776();
    let mut group = c.benchmark_group("decode_jpegls_4_80_f776");

    // Context: the stored 4.80 stream is LegacyGo (T87 decode fails on it,
    // verified by probe) - pure_jpegls decodes it via the LegacyGo fallback.
    group.bench_function("pure_jpegls_corpus_legacygo", |b| {
        b.iter(|| {
            let p = JpeglsLosslessCodec
                .decode(black_box(&corpus_bytes))
                .expect("decode failed");
            black_box(p.data.len())
        })
    });

    // The A/B pair: both decode the SAME T.87 codestream.
    group.bench_function("pure_jpegls_t87", |b| {
        b.iter(|| {
            let p = JpeglsLosslessCodec
                .decode(black_box(&t87_bytes))
                .expect("decode failed");
            black_box(p.data.len())
        })
    });

    group.finish();
}

#[cfg(feature = "transcode")]
fn encode_4_80(c: &mut Criterion) {
    let (_, _, _pixels, u16_pixels, _meta) = setup_f776();
    let (w, h) = (u32::from(_pixels.columns), u32::from(_pixels.rows));
    let mut group = c.benchmark_group("encode_jpegls_4_80_from_decoded");

    // Production bytes: frozen LegacyGo profile (codec/jpegls.rs default).
    group.bench_function("pure_jpegls_legacygo", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            let mut opts = jpegls::EncodeOptions::default();
            opts.near = 0;
            opts.profile = jpegls::Profile::LegacyGo;
            opts.precision = None;
            jpegls::encode_with_options(black_box(&u16_pixels), w, h, &opts, &mut out)
                .expect("LegacyGo encode failed");
            black_box(out.len())
        })
    });

    group.bench_function("pure_jpegls_t87", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            let mut opts = jpegls::EncodeOptions::default();
            opts.near = 0;
            opts.profile = jpegls::Profile::T87;
            opts.precision = None;
            jpegls::encode_with_options(black_box(&u16_pixels), w, h, &opts, &mut out)
                .expect("T87 encode failed");
            black_box(out.len())
        })
    });

    group.finish();
}

#[cfg(feature = "transcode")]
fn roundtrip_4_80(c: &mut Criterion) {
    // Decode source is the T.87 fixture for BOTH arms (the stored bytes
    // are LegacyGo).
    let (_, t87_bytes, _, _, meta) = setup_f776();
    let mut group = c.benchmark_group("e2e_roundtrip_jpegls_4_80_f776");

    group.bench_function("pure_jpegls", |b| {
        b.iter(|| {
            let dec = JpeglsLosslessCodec
                .decode(black_box(&t87_bytes))
                .expect("decode failed");
            let enc = JpeglsLosslessCodec
                .encode(black_box(&dec), black_box(&meta))
                .expect("encode failed");
            black_box(enc.len())
        })
    });

    group.finish();
}

/// Context arm: the transcode wall 4.70 -> 4.90. No JPEG-LS backend
/// involved (SV1 decode + J2K encode); both builds run identical code, so
/// this is a report number, not an A/B arm.
#[cfg(feature = "transcode")]
fn context_4_70_to_4_90(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_transcode_4_70_to_4_90");
    for (name, path) in [
        ("big_a_2586x2411", BIG_A_4_70),
        ("big_b_2149x1635", BIG_B_4_70),
        ("big_c_2320x1586", BIG_C_4_70),
    ] {
        let bytes = read_fixture(path);
        group.bench_function(name, |b| {
            b.iter(|| {
                let dec = JpegLosslessSv1Codec
                    .decode(black_box(&bytes))
                    .expect("SV1 decode failed");
                let meta = DicomMeta {
                    rows: dec.rows,
                    columns: dec.columns,
                    samples_per_pixel: dec.samples_per_pixel,
                    bits_allocated: dec.bits_allocated,
                    bits_stored: dec.bits_allocated,
                    photometric_interpretation: dec.photometric_interpretation.clone(),
                    planar_configuration: None,
                    pixel_representation: 0,
                    frames: 1,
                };
                let enc = Jpeg2000NativeLosslessCodec
                    .encode(black_box(&dec), black_box(&meta))
                    .expect("J2K encode failed");
                black_box(enc.len())
            })
        });
    }
    group.finish();
}

/// No-op arm for the default (no transcode) build: the codec modules are
/// feature-gated, so without `transcode` there is nothing to measure. Keeps
/// `cargo check --benches` green in the default build.
#[cfg(not(feature = "transcode"))]
fn empty_bench(_c: &mut Criterion) {}

#[cfg(feature = "transcode")]
criterion_group!(
    jpegls_backend,
    decode_4_80,
    encode_4_80,
    roundtrip_4_80,
    context_4_70_to_4_90
);
#[cfg(not(feature = "transcode"))]
criterion_group!(jpegls_backend, empty_bench);
criterion_main!(jpegls_backend);
