// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Transcoding roundtrip integration tests (P27-003).
//!
//! Verifies decode -> encode -> decode again for each supported
//! transfer-syntax pair. The roundtrip proves pixel-level fidelity:
//! lossless codecs must be bit-exact; lossy codecs must stay within
//! a small tolerance.
//!
//! # Test matrix
//!
//! | #   | Source              | Target              | Type     |
//! |:---:|:--------------------|:--------------------|:---------|
//! | Explicit VR LE      | Explicit VR BE      | lossless |
//! | Explicit VR LE      | RLE Lossless        | lossless |
//! | Explicit VR LE      | JPEG Baseline       | lossy    |
//! | RLE Lossless        | Explicit VR LE      | lossless |
//! | Explicit VR LE      | JPEG2000 Lossless   | lossless*|
//! | JPEG Baseline       | Explicit VR LE      | lossy    |
//!
//! # Endianness / compressed roundtrip tests
//!
//! | #   | Source              | Target              | Type     |
//! |:---:|:--------------------|:--------------------|:---------|
//! | Explicit VR BE      | Explicit VR LE      | lossless |
//! | Explicit VR BE      | RLE Lossless        | lossless |
//! | RLE Lossless        | Explicit VR BE      | lossless |
//! | RLE Lossless        | JPEG Baseline       | lossy    |
//! | JPEG Baseline       | RLE Lossless        | lossy    |
//!
//! *Feature-gated behind `--features transcode`.

use clarus_adapters::codec::CodecRegistry;
use clarus_adapters::codec::ts_uid;
use clarus_adapters::transcode::transcode_dicom;
// Only the transcode-gated tests use `extract_metadata` (the DicomParser trait
// method), so gate the import to the same feature: present under `--all`
// (feature unification) and absent under default-feature clippy.
#[cfg(feature = "transcode")]
use clarus_ports::dicom::DicomParser;

// -- Helpers --------------------------------------------------------------

/// Build a minimal Explicit VR LE DICOM with uncompressed monochrome
/// pixel data.
///
/// Generates a *non-random* deterministic ramp pattern so roundtrip
/// comparisons are reproducible.
fn build_explicit_vr_le_mono(rows: u16, cols: u16, bits: u8) -> Vec<u8> {
    assert!(bits == 8 || bits == 16, "bits must be 8 or 16");
    assert!(rows > 0 && cols > 0, "rows and cols must be > 0");

    let pixel_count = rows as usize * cols as usize;
    let bytes_per_sample = (bits as usize) >> 3;

    // Deterministic ramp: 0, 1, 2, ... modulo 256 (or 65536)
    let raw_pixels: Vec<u8> = if bits == 8 {
        (0..pixel_count).map(|i| (i & 0xFF) as u8).collect()
    } else {
        (0..pixel_count)
            .flat_map(|i| {
                let v = (i & 0xFFFF) as u16;
                v.to_le_bytes()
            })
            .collect()
    };
    assert_eq!(raw_pixels.len(), pixel_count * bytes_per_sample);

    build_dicom_explicit_vr_le(rows, cols, 1, bits, "MONOCHROME2", &raw_pixels, 0)
}

/// Build a minimal Explicit VR **Big Endian** DICOM with uncompressed
/// monochrome pixel data (same deterministic ramp pattern).
fn build_explicit_vr_be_mono(rows: u16, cols: u16, bits: u8) -> Vec<u8> {
    assert!(bits == 8 || bits == 16, "bits must be 8 or 16");
    assert!(rows > 0 && cols > 0, "rows and cols must be > 0");

    let pixel_count = rows as usize * cols as usize;

    // Deterministic ramp - same values as LE, different byte order
    let raw_pixels: Vec<u8> = if bits == 8 {
        (0..pixel_count).map(|i| (i & 0xFF) as u8).collect()
    } else {
        (0..pixel_count)
            .flat_map(|i| {
                let v = (i & 0xFFFF) as u16;
                v.to_be_bytes()
            })
            .collect()
    };

    let mut d = Vec::with_capacity(512 + raw_pixels.len());

    // Preamble (128 zero bytes) + DICM magic
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");

    // -- Meta-header (group 0x0002) - ALWAYS Explicit VR LE -------
    append_element_le(
        &mut d,
        0x0002,
        0x0010,
        b"UI",
        ts_uid::EXPLICIT_VR_BE.as_bytes(),
    );

    // -- Dataset (Explicit VR BE) ---------------------------------
    append_element_be(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.1");
    append_element_be(
        &mut d,
        0x0008,
        0x0018,
        b"UI",
        b"1.2.840.113619.2.55.3.p28.rtt",
    );
    append_element_be(&mut d, 0x0010, 0x0010, b"PN", b"Test^Patient");
    append_element_be(&mut d, 0x0010, 0x0020, b"LO", b"PAT-001");
    append_element_be(
        &mut d,
        0x0020,
        0x000D,
        b"UI",
        b"1.2.840.113619.2.55.3.p28.study",
    );
    append_element_be(
        &mut d,
        0x0020,
        0x000E,
        b"UI",
        b"1.2.840.113619.2.55.3.p28.series",
    );
    append_element_be(&mut d, 0x0008, 0x0060, b"CS", b"OT");
    append_element_be(&mut d, 0x0028, 0x0002, b"US", &(1u16).to_be_bytes());
    append_element_be(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element_be(&mut d, 0x0028, 0x0010, b"US", &rows.to_be_bytes());
    append_element_be(&mut d, 0x0028, 0x0011, b"US", &cols.to_be_bytes());
    append_element_be(&mut d, 0x0028, 0x0100, b"US", &(bits as u16).to_be_bytes());
    append_element_be(&mut d, 0x0028, 0x0101, b"US", &(bits as u16).to_be_bytes());
    append_element_be(
        &mut d,
        0x0028,
        0x0102,
        b"US",
        &((bits.saturating_sub(1)) as u16).to_be_bytes(),
    );
    append_element_be(&mut d, 0x0028, 0x0106, b"US", &0u16.to_be_bytes());

    // (7FE0,0010) OB - PixelData (always OB for Explicit VR, even for OW-like content)
    append_element_be(&mut d, 0x7FE0, 0x0010, b"OB", &raw_pixels);

    d
}

/// Low-level DICOM builder - Explicit VR LE, no compression.
///
/// Values are passed as-is; `append_element` handles DICOM odd-length
/// padding correctly (pad byte NOT included in length, per PS3.5 Section 7.1).
fn build_dicom_explicit_vr_le(
    rows: u16,
    cols: u16,
    samples_per_pixel: u8,
    bits_allocated: u8,
    photometric: &str,
    pixel_data: &[u8],
    pixel_representation: u16,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(512 + pixel_data.len());

    // Preamble (128 zero bytes) + DICM magic
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");

    // -- Meta-header (group 0x0002) --------------------------------
    append_element(
        &mut d,
        0x0002,
        0x0010,
        b"UI",
        ts_uid::EXPLICIT_VR_LE.as_bytes(),
    );

    // -- Dataset (Explicit VR LE) ----------------------------------
    append_element(&mut d, 0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.1");
    append_element(
        &mut d,
        0x0008,
        0x0018,
        b"UI",
        b"1.2.840.113619.2.55.3.p27.rtt",
    );
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"Test^Patient");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"PAT-001");
    append_element(
        &mut d,
        0x0020,
        0x000D,
        b"UI",
        b"1.2.840.113619.2.55.3.p27.study",
    );
    append_element(
        &mut d,
        0x0020,
        0x000E,
        b"UI",
        b"1.2.840.113619.2.55.3.p27.series",
    );
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"OT");
    append_element(
        &mut d,
        0x0028,
        0x0002,
        b"US",
        &(samples_per_pixel as u16).to_le_bytes(),
    );
    append_element(&mut d, 0x0028, 0x0004, b"CS", photometric.as_bytes());
    append_element(&mut d, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
    append_element(
        &mut d,
        0x0028,
        0x0100,
        b"US",
        &(bits_allocated as u16).to_le_bytes(),
    );
    append_element(
        &mut d,
        0x0028,
        0x0101,
        b"US",
        &(bits_allocated as u16).to_le_bytes(),
    );
    append_element(
        &mut d,
        0x0028,
        0x0102,
        b"US",
        &((bits_allocated.saturating_sub(1)) as u16).to_le_bytes(),
    );
    append_element(&mut d, 0x0028, 0x0106, b"US", &0u16.to_le_bytes());
    append_element(
        &mut d,
        0x0028,
        0x0103,
        b"US",
        &pixel_representation.to_le_bytes(),
    );

    // (7FE0,0010) OW/OB - PixelData
    let vr = if bits_allocated <= 8 { b"OB" } else { b"OW" };
    append_element(&mut d, 0x7FE0, 0x0010, vr, pixel_data);

    d
}

/// Append an Explicit VR DICOM element.
///
/// Format: tag(4) + VR(2) + [reserved(2) + len(4) for extended VR,
/// or len(2) for short VR] + value(N) + [pad(1) if N is odd].
///
/// DICOM PS3.5 Section 7.1: Value Length is the number of bytes of the Value Field.
/// The pad byte (0x00 for UI/binary, 0x20 for strings) is OUTSIDE the length.
fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();

    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);

    let is_extended = matches!(
        vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    );

    // DICOM PS3.5 Section 6.2: UI values with odd length SHALL include the
    // NULL pad character (00H) in the Value Length. All other VRs
    // pad OUTSIDE the length per Section 7.1.
    if vr == b"UI" && !len.is_multiple_of(2) {
        // UI: pad INSIDE length
        let padded_len = len + 1;
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(padded_len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(padded_len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        buf.push(0x00); // NULL pad
    } else {
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(len as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !len.is_multiple_of(2) {
            let pad = if vr == b"CS"
                || vr == b"LO"
                || vr == b"SH"
                || vr == b"PN"
                || vr == b"AE"
                || vr == b"AS"
                || vr == b"DA"
                || vr == b"DS"
                || vr == b"DT"
                || vr == b"IS"
                || vr == b"LT"
                || vr == b"ST"
                || vr == b"TM"
                || vr == b"UC"
                || vr == b"UR"
                || vr == b"UT"
            {
                0x20
            } else {
                0x00
            };
            buf.push(pad);
        }
    }
}

/// Append an Explicit VR **Little Endian** DICOM element (alias).
fn append_element_le(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    append_element(buf, group, element, vr, value)
}

/// Append an Explicit VR **Big Endian** DICOM element.
///
/// Tags are always LE per DICOM; VR is ASCII; lengths and multi-byte
/// values are BE.
fn append_element_be(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();

    // Explicit VR Big Endian reverses the whole element, tag included - the
    // reference implementation (pydicom) writes group and element big endian
    // here. Writing them little endian produced a file no BE reader agrees is
    // BE, so this fixture was testing the parser against a non-conformant
    // input rather than against the syntax it names.
    buf.extend_from_slice(&group.to_be_bytes());
    buf.extend_from_slice(&element.to_be_bytes());
    buf.extend_from_slice(vr);

    let is_extended = matches!(
        vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    );

    if vr == b"UI" && !len.is_multiple_of(2) {
        // UI: pad INSIDE length (PS3.5 Section 6.2)
        let padded_len = len + 1;
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(padded_len as u32).to_be_bytes());
        } else {
            buf.extend_from_slice(&(padded_len as u16).to_be_bytes());
        }
        buf.extend_from_slice(value);
        buf.push(0x00);
    } else {
        if is_extended {
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(len as u32).to_be_bytes());
        } else {
            buf.extend_from_slice(&(len as u16).to_be_bytes());
        }
        buf.extend_from_slice(value);
        if !len.is_multiple_of(2) {
            let pad = if vr == b"CS"
                || vr == b"LO"
                || vr == b"SH"
                || vr == b"PN"
                || vr == b"AE"
                || vr == b"AS"
                || vr == b"DA"
                || vr == b"DS"
                || vr == b"DT"
                || vr == b"IS"
                || vr == b"LT"
                || vr == b"ST"
                || vr == b"TM"
                || vr == b"UC"
                || vr == b"UR"
                || vr == b"UT"
            {
                0x20
            } else {
                0x00
            };
            buf.push(pad);
        }
    }
}

/// Compare two pixel buffers for equality.
///
/// For **lossless** codecs the pixels must match exactly.
/// For **lossy** codecs (e.g. JPEG Baseline) we allow a per-pixel
/// difference up to `max_diff` (default 3 for 8-bit, 5 for 16-bit).
fn assert_pixels_equal(expected: &[u8], actual: &[u8], tolerance: u8, label: &str) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "{label}: pixel buffer size mismatch"
    );

    let mut max_err = 0u8;
    let mut err_count = 0usize;

    for (i, (&e, &a)) in expected.iter().zip(actual.iter()).enumerate() {
        let diff = e.abs_diff(a);
        if diff > tolerance {
            if err_count < 5 {
                eprintln!("{label}: pixel[{i}] expected {e}, got {a} (diff={diff})");
            }
            err_count += 1;
        }
        if diff > max_err {
            max_err = diff;
        }
    }

    if err_count > 0 {
        panic!(
            "{label}: {err_count}/{total} pixels exceed tolerance {tolerance} (max diff {max_err})",
            total = expected.len(),
        );
    }
}

/// Run a full roundtrip: source TS -> target TS -> source TS, then
/// decode final result and compare against original decoded pixels.
fn roundtrip_test(
    build_dicom: fn() -> Vec<u8>,
    source_ts: &str,
    target_ts: &str,
    test_label: &str,
    tolerance: u8,
) {
    let registry = CodecRegistry::default();

    // 1. Build source DICOM
    let original = build_dicom();

    // 2. Decode original to get reference pixels
    let source_codec = registry
        .get(source_ts)
        .unwrap_or_else(|| panic!("{test_label}: no codec for source TS {source_ts}"));
    let ref_pixels = source_codec
        .decode(&original)
        .unwrap_or_else(|e| panic!("{test_label}: decode original failed: {e}"));
    ref_pixels
        .validate()
        .unwrap_or_else(|e| panic!("{test_label}: ref_pixels invalid: {e}"));

    // 3. Transcode A -> B
    let transcoded = transcode_dicom(&original, target_ts, &registry)
        .unwrap_or_else(|e| panic!("{test_label}: transcode {source_ts}→{target_ts} failed: {e}"));
    assert!(
        transcoded.len() > 200,
        "{test_label}: transcoded output too small ({} bytes)",
        transcoded.len()
    );

    // 4. Transcode B -> A (roundtrip back)
    let roundtrip = transcode_dicom(&transcoded, source_ts, &registry)
        .unwrap_or_else(|e| panic!("{test_label}: roundtrip {target_ts}→{source_ts} failed: {e}"));

    // 5. Decode roundtripped back to pixels
    let rt_pixels = source_codec
        .decode(&roundtrip)
        .unwrap_or_else(|e| panic!("{test_label}: decode roundtrip failed: {e}"));
    rt_pixels
        .validate()
        .unwrap_or_else(|e| panic!("{test_label}: rt_pixels invalid: {e}"));

    // 6. Compare
    assert_pixels_equal(
        &ref_pixels.data,
        &rt_pixels.data,
        tolerance,
        &format!("{test_label} roundtrip {source_ts}⇄{target_ts}"),
    );

    eprintln!(
        "[OK] {test_label}: {source_ts} ⇄ {target_ts} — {} px, {} bytes → {} bytes → {} bytes",
        ref_pixels.pixel_count(),
        original.len(),
        transcoded.len(),
        roundtrip.len(),
    );
}

// ===========================================================================
// Explicit VR LE -> Explicit VR BE (lossless)
// ===========================================================================
#[test]
fn roundtrip_le_to_be_lossless() {
    roundtrip_test(
        || build_explicit_vr_le_mono(64, 64, 8),
        ts_uid::EXPLICIT_VR_LE,
        ts_uid::EXPLICIT_VR_BE,
        "LE to BE",
        0,
    );
}

// ===========================================================================
// Explicit VR LE -> RLE Lossless (lossless)
// ===========================================================================
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_le_to_rle_lossless() {
    roundtrip_test(
        || build_explicit_vr_le_mono(128, 128, 8),
        ts_uid::EXPLICIT_VR_LE,
        ts_uid::RLE_LOSSLESS,
        "LE to RLE",
        0,
    );
}

// ===========================================================================
// Explicit VR LE -> JPEG Baseline (lossy)
// ===========================================================================
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_le_to_jpeg_baseline_lossy() {
    // JPEG Baseline is inherently lossy - allow small per-pixel diff
    roundtrip_test(
        || build_explicit_vr_le_mono(64, 64, 8),
        ts_uid::EXPLICIT_VR_LE,
        ts_uid::JPEG_BASELINE,
        "LE to JPEG",
        3,
    );
}

// ===========================================================================
// RLE Lossless -> Explicit VR LE (lossless)
// ===========================================================================
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_rle_to_le_lossless() {
    let registry = CodecRegistry::default();

    // Build source as Explicit VR LE, then pre-transcode to RLE
    let original = build_explicit_vr_le_mono(32, 32, 8);
    let rle_source = transcode_dicom(&original, ts_uid::RLE_LOSSLESS, &registry)
        .expect("RLE roundtrip: pre-transcode to RLE failed");

    // Decode RLE to get reference pixels
    let rle_codec = registry
        .get(ts_uid::RLE_LOSSLESS)
        .expect("RLE roundtrip: RLE codec missing");
    let ref_pixels = rle_codec
        .decode(&rle_source)
        .expect("RLE roundtrip: decode RLE failed");
    ref_pixels
        .validate()
        .expect("RLE roundtrip: ref_pixels invalid");

    // Roundtrip: RLE -> Explicit VR LE -> RLE
    let to_le = transcode_dicom(&rle_source, ts_uid::EXPLICIT_VR_LE, &registry)
        .expect("RLE roundtrip: RLE→LE failed");
    let back_to_rle = transcode_dicom(&to_le, ts_uid::RLE_LOSSLESS, &registry)
        .expect("RLE roundtrip: LE→RLE failed");

    let rt_pixels = rle_codec
        .decode(&back_to_rle)
        .expect("RLE roundtrip: decode roundtrip failed");
    rt_pixels
        .validate()
        .expect("RLE roundtrip: rt_pixels invalid");

    assert_pixels_equal(&ref_pixels.data, &rt_pixels.data, 0, "RLE roundtrip");

    eprintln!(
        "[OK] RLE Lossless ⇄ Explicit VR LE — {} px, {} bytes → {} bytes → {} bytes",
        ref_pixels.pixel_count(),
        rle_source.len(),
        to_le.len(),
        back_to_rle.len(),
    );
}

// ===========================================================================
// JPEG Baseline -> Explicit VR LE (lossy decode)
// ===========================================================================
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_jpeg_baseline_to_le_lossy() {
    let registry = CodecRegistry::default();

    // Build source as Explicit VR LE, pre-transcode to JPEG Baseline
    let original = build_explicit_vr_le_mono(64, 64, 8);
    let jpeg_source = transcode_dicom(&original, ts_uid::JPEG_BASELINE, &registry)
        .expect("JPEG baseline roundtrip: pre-transcode to JPEG failed");

    // Decode JPEG to get reference pixels
    let jpeg_codec = registry
        .get(ts_uid::JPEG_BASELINE)
        .expect("JPEG baseline roundtrip: JPEG codec missing");
    let ref_pixels = jpeg_codec
        .decode(&jpeg_source)
        .expect("JPEG baseline roundtrip: decode JPEG failed");
    ref_pixels
        .validate()
        .expect("JPEG baseline roundtrip: ref_pixels invalid");

    // Roundtrip: JPEG Baseline -> Explicit VR LE -> JPEG Baseline
    let to_le = transcode_dicom(&jpeg_source, ts_uid::EXPLICIT_VR_LE, &registry)
        .expect("JPEG baseline roundtrip: JPEG→LE failed");
    let back_to_jpeg = transcode_dicom(&to_le, ts_uid::JPEG_BASELINE, &registry)
        .expect("JPEG baseline roundtrip: LE→JPEG failed");

    let rt_pixels = jpeg_codec
        .decode(&back_to_jpeg)
        .expect("JPEG baseline roundtrip: decode roundtrip failed");
    rt_pixels
        .validate()
        .expect("JPEG baseline roundtrip: rt_pixels invalid");

    assert_pixels_equal(
        &ref_pixels.data,
        &rt_pixels.data,
        3,
        "JPEG baseline roundtrip",
    );

    eprintln!(
        "[OK] JPEG Baseline ⇄ Explicit VR LE — {} px, {} bytes → {} bytes → {} bytes",
        ref_pixels.pixel_count(),
        jpeg_source.len(),
        to_le.len(),
        back_to_jpeg.len(),
    );
}

// ===========================================================================
// Explicit VR LE -> JPEG2000 Lossless (feature-gated)
// ===========================================================================
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_le_to_jpeg2000_lossless() {
    roundtrip_test(
        || build_explicit_vr_le_mono(64, 64, 8),
        ts_uid::EXPLICIT_VR_LE,
        ts_uid::JPEG2000_LOSSLESS,
        "RLE to LE",
        0,
    );
}

// ===========================================================================
// T-5b: Explicit VR LE (PR=1 signed 16-bit) -> JPEG2000 Lossless
// ===========================================================================

/// Build a minimal Explicit VR LE DICOM with signed (PR=1) 16-bit
/// monochrome pixel data: two's complement values including negatives.
#[cfg(feature = "transcode")]
fn build_explicit_vr_le_mono16_signed(rows: u16, cols: u16) -> Vec<u8> {
    let pixel_count = rows as usize * cols as usize;
    // Deterministic signed ramp starting at -1000 (0xFC18), +7 per pixel,
    // wrapping the full 16-bit range.
    let raw_pixels: Vec<u8> = (0..pixel_count)
        .flat_map(|i| {
            let v = (0xFC18u32 + (i as u32 * 7)) & 0xFFFF;
            (v as u16).to_le_bytes()
        })
        .collect();
    assert_eq!(raw_pixels.len(), pixel_count * 2);
    build_dicom_explicit_vr_le(rows, cols, 1, 16, "MONOCHROME2", &raw_pixels, 1)
}

#[cfg(feature = "transcode")]
#[test]
fn roundtrip_le_to_jpeg2000_lossless_signed_16bit() {
    // Regression guard for the PR-aware signed flag: the integration
    // suite missed this bug class because the JPEG2000 case is 8-bit and
    // unsigned.
    // A PR=1 dataset must transcode to JPEG2000 and back byte-exactly
    // (signed values must not be shifted).
    roundtrip_test(
        || build_explicit_vr_le_mono16_signed(4, 4),
        ts_uid::EXPLICIT_VR_LE,
        ts_uid::JPEG2000_LOSSLESS,
        "T-5b",
        0,
    );
}

// ===========================================================================
// Endianness / compressed roundtrip tests
// ===========================================================================

/// BE -> LE -> BE - frame data must be byte-identical.
#[test]
fn roundtrip_be_to_le_to_be_byte_identical() {
    let registry = CodecRegistry::default();

    // Build source as Explicit VR BE
    let be_original = build_explicit_vr_be_mono(64, 64, 8);

    // Decode BE to get reference pixels
    let be_codec = registry
        .get(ts_uid::EXPLICIT_VR_BE)
        .expect("BE roundtrip: BE codec missing");
    let ref_pixels = be_codec
        .decode(&be_original)
        .expect("BE roundtrip: decode BE failed");
    ref_pixels
        .validate()
        .expect("BE roundtrip: ref_pixels invalid");

    // Roundtrip: BE -> LE -> BE
    let to_le = transcode_dicom(&be_original, ts_uid::EXPLICIT_VR_LE, &registry)
        .expect("BE roundtrip: BE→LE failed");
    let back_to_be = transcode_dicom(&to_le, ts_uid::EXPLICIT_VR_BE, &registry)
        .expect("BE roundtrip: LE→BE failed");

    // Decode roundtripped back to pixels
    let rt_pixels = be_codec
        .decode(&back_to_be)
        .expect("BE roundtrip: decode roundtrip failed");
    rt_pixels
        .validate()
        .expect("BE roundtrip: rt_pixels invalid");

    assert_pixels_equal(&ref_pixels.data, &rt_pixels.data, 0, "BE roundtrip");

    eprintln!(
        "[OK] BE ⇄ LE ⇄ BE — {} px, {} bytes → {} bytes → {} bytes",
        ref_pixels.pixel_count(),
        be_original.len(),
        to_le.len(),
        back_to_be.len(),
    );
}

/// BE -> RLE - must produce valid RLE-compressed DICOM.
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_be_to_rle() {
    let registry = CodecRegistry::default();

    let be_original = build_explicit_vr_be_mono(128, 128, 8);

    // Decode BE to get reference pixels
    let be_codec = registry
        .get(ts_uid::EXPLICIT_VR_BE)
        .expect("BE to RLE: BE codec missing");
    let ref_pixels = be_codec
        .decode(&be_original)
        .expect("BE to RLE: decode BE failed");
    ref_pixels
        .validate()
        .expect("BE to RLE: ref_pixels invalid");

    // BE -> RLE
    let rle_out = transcode_dicom(&be_original, ts_uid::RLE_LOSSLESS, &registry)
        .expect("BE to RLE: BE→RLE failed");

    // Verify output is valid RLE DICOM
    let rle_codec = registry
        .get(ts_uid::RLE_LOSSLESS)
        .expect("BE to RLE: RLE codec missing");
    let decoded = rle_codec
        .decode(&rle_out)
        .expect("BE to RLE: decode RLE failed");
    decoded.validate().expect("BE to RLE: RLE output invalid");

    assert_pixels_equal(&ref_pixels.data, &decoded.data, 0, "BE to RLE");

    // Roundtrip: RLE -> LE -> RLE (extra resilience check)
    let to_le = transcode_dicom(&rle_out, ts_uid::EXPLICIT_VR_LE, &registry)
        .expect("BE to RLE: RLE→LE failed");
    let back_to_rle =
        transcode_dicom(&to_le, ts_uid::RLE_LOSSLESS, &registry).expect("BE to RLE: LE→RLE failed");
    let rt_pixels = rle_codec
        .decode(&back_to_rle)
        .expect("BE to RLE: roundtrip decode failed");
    assert_pixels_equal(&ref_pixels.data, &rt_pixels.data, 0, "BE to RLE roundtrip");

    eprintln!(
        "[OK] BE → RLE — {} px, {} bytes → {} bytes",
        ref_pixels.pixel_count(),
        be_original.len(),
        rle_out.len(),
    );
}

/// RLE -> BE - must produce valid BE DICOM.
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_rle_to_be() {
    let registry = CodecRegistry::default();

    // Build source as LE, pre-transcode to RLE
    let le_original = build_explicit_vr_le_mono(32, 32, 8);
    let rle_source = transcode_dicom(&le_original, ts_uid::RLE_LOSSLESS, &registry)
        .expect("RLE to BE: pre-transcode to RLE failed");

    // Decode RLE to get reference pixels
    let rle_codec = registry
        .get(ts_uid::RLE_LOSSLESS)
        .expect("RLE to BE: RLE codec missing");
    let ref_pixels = rle_codec
        .decode(&rle_source)
        .expect("RLE to BE: decode RLE failed");
    ref_pixels
        .validate()
        .expect("RLE to BE: ref_pixels invalid");

    // RLE -> BE
    let be_out = transcode_dicom(&rle_source, ts_uid::EXPLICIT_VR_BE, &registry)
        .expect("RLE to BE: RLE→BE failed");

    // Verify output is valid BE DICOM
    let be_codec = registry
        .get(ts_uid::EXPLICIT_VR_BE)
        .expect("RLE to BE: BE codec missing");
    let decoded = be_codec
        .decode(&be_out)
        .expect("RLE to BE: decode BE failed");
    decoded.validate().expect("RLE to BE: BE output invalid");

    assert_pixels_equal(&ref_pixels.data, &decoded.data, 0, "RLE to BE");

    eprintln!(
        "[OK] RLE → BE — {} px, {} bytes → {} bytes",
        ref_pixels.pixel_count(),
        rle_source.len(),
        be_out.len(),
    );
}

/// RLE -> JPEG Baseline - must produce valid JPEG DICOM.
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_rle_to_jpeg() {
    let registry = CodecRegistry::default();

    // Build source as LE, pre-transcode to RLE
    let le_original = build_explicit_vr_le_mono(64, 64, 8);
    let rle_source = transcode_dicom(&le_original, ts_uid::RLE_LOSSLESS, &registry)
        .expect("RLE to JPEG: pre-transcode to RLE failed");

    // Decode RLE to get reference pixels
    let rle_codec = registry
        .get(ts_uid::RLE_LOSSLESS)
        .expect("RLE to JPEG: RLE codec missing");
    let ref_pixels = rle_codec
        .decode(&rle_source)
        .expect("RLE to JPEG: decode RLE failed");
    ref_pixels
        .validate()
        .expect("RLE to JPEG: ref_pixels invalid");

    // RLE -> JPEG Baseline
    let jpeg_out = transcode_dicom(&rle_source, ts_uid::JPEG_BASELINE, &registry)
        .expect("RLE to JPEG: RLE→JPEG failed");

    // Verify output is valid JPEG DICOM
    let jpeg_codec = registry
        .get(ts_uid::JPEG_BASELINE)
        .expect("RLE to JPEG: JPEG codec missing");
    let decoded = jpeg_codec
        .decode(&jpeg_out)
        .expect("RLE to JPEG: decode JPEG failed");
    decoded
        .validate()
        .expect("RLE to JPEG: JPEG output invalid");

    // Lossy - allow small tolerance
    assert_pixels_equal(&ref_pixels.data, &decoded.data, 3, "RLE to JPEG");

    eprintln!(
        "[OK] RLE → JPEG — {} px, {} bytes → {} bytes",
        ref_pixels.pixel_count(),
        rle_source.len(),
        jpeg_out.len(),
    );
}

/// JPEG -> RLE - must produce valid RLE DICOM.
#[cfg(feature = "transcode")]
#[test]
fn roundtrip_jpeg_to_rle() {
    let registry = CodecRegistry::default();

    // Build source as LE, pre-transcode to JPEG Baseline
    let le_original = build_explicit_vr_le_mono(64, 64, 8);
    let jpeg_source = transcode_dicom(&le_original, ts_uid::JPEG_BASELINE, &registry)
        .expect("JPEG to RLE: pre-transcode to JPEG failed");

    // Decode JPEG to get reference pixels
    let jpeg_codec = registry
        .get(ts_uid::JPEG_BASELINE)
        .expect("JPEG to RLE: JPEG codec missing");
    let ref_pixels = jpeg_codec
        .decode(&jpeg_source)
        .expect("JPEG to RLE: decode JPEG failed");
    ref_pixels
        .validate()
        .expect("JPEG to RLE: ref_pixels invalid");

    // JPEG -> RLE
    let rle_out = transcode_dicom(&jpeg_source, ts_uid::RLE_LOSSLESS, &registry)
        .expect("JPEG to RLE: JPEG→RLE failed");

    // Verify output is valid RLE DICOM
    let rle_codec = registry
        .get(ts_uid::RLE_LOSSLESS)
        .expect("JPEG to RLE: RLE codec missing");
    let decoded = rle_codec
        .decode(&rle_out)
        .expect("JPEG to RLE: decode RLE failed");
    decoded.validate().expect("JPEG to RLE: RLE output invalid");

    assert_pixels_equal(&ref_pixels.data, &decoded.data, 0, "JPEG to RLE");

    // Extra: roundtrip JPEG->RLE->JPEG for resilience
    let back_to_jpeg = transcode_dicom(&rle_out, ts_uid::JPEG_BASELINE, &registry)
        .expect("JPEG to RLE: RLE→JPEG failed");
    let rt_pixels = jpeg_codec
        .decode(&back_to_jpeg)
        .expect("JPEG to RLE: roundtrip decode failed");
    assert_pixels_equal(
        &ref_pixels.data,
        &rt_pixels.data,
        3,
        "JPEG to RLE roundtrip",
    );

    eprintln!(
        "[OK] JPEG → RLE — {} px, {} bytes → {} bytes",
        ref_pixels.pixel_count(),
        jpeg_source.len(),
        rle_out.len(),
    );
}

// ===========================================================================
// E2E: JPEG Baseline -> Explicit VR LE - metadata verification (P37 audit)
// ===========================================================================

/// E2E transcode verification
/// 16-bit uncompressed -> JPEG Baseline (8-bit) -> Explicit VR LE.
///
/// Verifies the output DICOM has correct bit-depth metadata:
/// - BitsAllocated = 8, BitsStored = 8, HighBit = 7
/// - PhotometricInterpretation = MONOCHROME2
/// - No WindowCenter / WindowWidth (stripped by bit-depth reduction)
/// - Pixel data size = Rows x Columns x 1 byte
#[cfg(feature = "transcode")]
#[test]
fn e2e_16bit_to_jpeg_baseline_to_explicit_vr_le_metadata() {
    let registry = CodecRegistry::default();

    // 1. Build 16-bit uncompressed DICOM
    let src_16bit = build_explicit_vr_le_mono(32, 32, 16);

    // 2. Transcode to JPEG Baseline (lossy, 8-bit)
    let jpeg = transcode_dicom(&src_16bit, ts_uid::JPEG_BASELINE, &registry)
        .expect("E2E: transcode to JPEG Baseline failed");

    // Decode JPEG to verify it's valid
    let jpeg_codec = registry
        .get(ts_uid::JPEG_BASELINE)
        .expect("E2E: JPEG codec missing");
    let decoded_jpeg = jpeg_codec.decode(&jpeg).expect("E2E: decode JPEG failed");
    decoded_jpeg.validate().expect("E2E: JPEG pixels invalid");
    assert_eq!(decoded_jpeg.bits_allocated, 8, "E2E: JPEG should be 8-bit");

    // 3. Transcode JPEG -> Explicit VR LE
    let output = transcode_dicom(&jpeg, ts_uid::EXPLICIT_VR_LE, &registry)
        .expect("E2E: transcode to Explicit VR LE failed");

    // 4. Parse output with DICOM parser to verify it's valid
    let parser = clarus_adapters::dicom_rs::DicomRsParser;
    let meta = parser
        .extract_metadata(&output)
        .expect("E2E: parse output DICOM failed");

    // Verify key metadata fields from parser
    assert_eq!(
        meta.patient_name, "Test^Patient",
        "E2E: PatientName mismatch"
    );
    assert_eq!(meta.patient_id, "PAT-001", "E2E: PatientID mismatch");

    // 5. Verify bit-depth tags in raw output
    let (ba, bs, hb, pi) = extract_bit_depth_tags(&output);
    assert_eq!(ba, 8, "E2E: BitsAllocated should be 8");
    assert_eq!(bs, 8, "E2E: BitsStored should be 8");
    assert_eq!(hb, 7, "E2E: HighBit should be 7");
    assert_eq!(
        pi, "MONOCHROME2",
        "E2E: PhotometricInterpretation should be MONOCHROME2"
    );

    // 6. Verify no WindowCenter (0028,1050) / WindowWidth (0028,1051)
    assert!(
        !has_tag(&output, 0x0028, 0x1050),
        "E2E: WindowCenter should be absent"
    );
    assert!(
        !has_tag(&output, 0x0028, 0x1051),
        "E2E: WindowWidth should be absent"
    );

    // 7. Verify pixel data size = Rows x Columns x 1 byte (8-bit)
    let pixel_size = extract_pixel_data_len(&output);
    assert_eq!(
        pixel_size, 1024,
        "E2E: pixel data should be 32×32×1 = 1024 bytes"
    );

    eprintln!(
        "[OK] E2E: 16-bit → JPEG Baseline → Explicit VR LE — {} bytes → {} bytes → {} bytes, BA=8 BS=8 HB=7 PI=MONOCHROME2, px={}",
        src_16bit.len(),
        jpeg.len(),
        output.len(),
        pixel_size,
    );
}

/// Low-level DICOM tag reader: walk Explicit VR LE dataset and return
/// (BitsAllocated, BitsStored, HighBit, PhotometricInterpretation).
#[cfg(feature = "transcode")]
fn extract_bit_depth_tags(data: &[u8]) -> (u16, u16, u16, String) {
    let mut ba = 0u16;
    let mut bs = 0u16;
    let mut hb = 0u16;
    let mut pi = String::new();
    let mut pos = skip_to_dataset(data);

    while pos + 4 <= data.len() {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let elem = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        pos += 4;

        if pos + 2 > data.len() {
            break;
        }
        let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("");
        pos += 2;

        let len: usize = if matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        ) {
            pos += 2; // reserved
            let l = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4;
            l
        } else {
            let l = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            l
        };

        match (group, elem) {
            (0x0028, 0x0100) => ba = u16::from_le_bytes([data[pos], data[pos + 1]]),
            (0x0028, 0x0101) => bs = u16::from_le_bytes([data[pos], data[pos + 1]]),
            (0x0028, 0x0102) => hb = u16::from_le_bytes([data[pos], data[pos + 1]]),
            (0x0028, 0x0004) => pi = String::from_utf8_lossy(&data[pos..pos + len]).to_string(),
            _ => {}
        }

        pos += len;
        if !len.is_multiple_of(2) {
            pos += 1; // pad byte
        }
    }

    (ba, bs, hb, pi)
}

/// Find start of dataset (after DICM magic + meta-header group 0x0002).
#[cfg(feature = "transcode")]
fn skip_to_dataset(data: &[u8]) -> usize {
    if data.len() >= 132 && &data[128..132] == b"DICM" {
        let mut pos = 132;
        while pos + 4 <= data.len() {
            let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
            if group != 0x0002 {
                return pos;
            }
            // Skip meta-header element
            pos += 4; // tag
            if pos + 2 > data.len() {
                break;
            }
            let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("");
            pos += 2;
            let len: usize = if matches!(
                vr,
                "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            ) {
                pos += 2;
                let l = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize;
                pos += 4;
                l
            } else {
                let l = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2;
                l
            };
            pos += len;
            if !len.is_multiple_of(2) {
                pos += 1;
            }
        }
        return data.len().min(132);
    }
    0
}

/// Check if a specific DICOM tag exists in the dataset.
#[cfg(feature = "transcode")]
fn has_tag(data: &[u8], target_group: u16, target_elem: u16) -> bool {
    let mut pos = skip_to_dataset(data);
    while pos + 4 <= data.len() {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let elem = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        if group == target_group && elem == target_elem {
            return true;
        }
        pos += 4;
        if pos + 2 > data.len() {
            break;
        }
        let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("");
        pos += 2;
        let len: usize = if matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        ) {
            pos += 2;
            let l = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4;
            l
        } else {
            let l = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            l
        };
        pos += len;
        if !len.is_multiple_of(2) {
            pos += 1;
        }
    }
    false
}

/// Extract length of PixelData (7FE0,0010) element.
#[cfg(feature = "transcode")]
fn extract_pixel_data_len(data: &[u8]) -> usize {
    let mut pos = skip_to_dataset(data);
    while pos + 4 <= data.len() {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let elem = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        pos += 4;
        if pos + 2 > data.len() {
            break;
        }
        let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("");
        pos += 2;
        let len: usize = if matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        ) {
            pos += 2;
            let l = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4;
            l
        } else {
            let l = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            l
        };
        if group == 0x7FE0 && elem == 0x0010 {
            return len;
        }
        pos += len;
        if !len.is_multiple_of(2) {
            pos += 1;
        }
    }
    0
}

// ===========================================================================
// Direct transcode - structural validity (VR walk)
// ===========================================================================

/// Walk all Explicit VR elements in a DICOM file and verify every element
/// has a valid VR (2 uppercase ASCII letters). Returns (count, first_bad_pos).
fn validate_explicit_vr(data: &[u8]) -> (usize, Option<usize>) {
    let mut pos = if data.len() >= 132 && &data[128..132] == b"DICM" {
        132
    } else if data.len() >= 4 && &data[0..4] == b"DICM" {
        4
    } else {
        return (0, Some(0));
    };

    let mut count = 0usize;

    while pos + 6 <= data.len() {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let e = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);

        // Stop at PixelData
        if g == 0x7FE0 && e == 0x0010 {
            count += 1;
            break;
        }

        let vr_bytes = &data[pos + 4..pos + 6];
        let vr = std::str::from_utf8(vr_bytes).unwrap_or("");
        if vr.len() != 2 || !vr.chars().all(|c| c.is_ascii_uppercase()) {
            return (count, Some(pos));
        }

        let is_ext = matches!(
            vr,
            "OB" | "OD"
                | "OF"
                | "OL"
                | "OV"
                | "OW"
                | "SQ"
                | "SV"
                | "UC"
                | "UN"
                | "UR"
                | "UT"
                | "UV"
        );

        let val_len: usize = if is_ext {
            if pos + 12 > data.len() {
                break;
            }
            u32::from_le_bytes([data[pos + 8], data[pos + 9], data[pos + 10], data[pos + 11]])
                as usize
        } else {
            u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as usize
        };

        let elem_size = if is_ext {
            12 + val_len
        } else {
            8 + val_len + (val_len % 2)
        };
        pos += elem_size;
        count += 1;
    }

    (count, None)
}

/// Transcode to EVRLE and verify all elements have valid Explicit VR.
/// Catches UI-pad-byte bug (PS3.5 Section 6.2) where odd-length UI length excludes
/// NULL pad, causing subsequent element to be misparsed as implicit VR.
#[test]
fn transcode_direct_structural_validity() {
    let registry = CodecRegistry::default();

    // Build source with odd-length TS (19 bytes like real "1.2.840.10008.1.2.1")
    let source = build_explicit_vr_le_mono(64, 64, 16);
    let (src_count, src_bad) = validate_explicit_vr(&source);
    assert!(
        src_bad.is_none(),
        "Source DICOM has invalid VR at byte {}",
        src_bad.unwrap_or(0)
    );
    assert!(src_count > 5, "Source has only {src_count} elements");

    // Transcode 16-bit EVRLE -> 16-bit EVRLE (passthrough may trigger direct path)
    let output = transcode_dicom(&source, ts_uid::EXPLICIT_VR_LE, &registry)
        .expect("EVRLE structural validity: transcode to EVRLE failed");
    assert!(
        output.len() > 200,
        "Output too small: {} bytes",
        output.len()
    );

    let (out_count, out_bad) = validate_explicit_vr(&output);
    assert!(
        out_bad.is_none(),
        "Transcode output has invalid VR at byte {} ({} elements before failure)",
        out_bad.unwrap_or(0),
        out_count
    );
    assert!(
        out_count >= src_count,
        "Tag count mismatch: source={src_count}, output={out_count}"
    );

    eprintln!(
        "[OK] direct transcode structural validity — {} elements, {} bytes → {} bytes, all VRs valid",
        out_count,
        source.len(),
        output.len()
    );
}

/// Full direct path JPEG Baseline -> EVRLE with structural validation.
/// This exercises the complete transcode_direct code path including
/// meta-header TS replacement (odd-length UI) and PixelData element end
/// detection on encapsulated format.
#[cfg(feature = "transcode")]
#[test]
fn direct_jpeg_to_evrle_structural_validity() {
    let registry = CodecRegistry::default();

    // Build 8-bit source, pre-transcode to JPEG Baseline
    let le_source = build_explicit_vr_le_mono(32, 32, 8);
    let jpeg_source = transcode_dicom(&le_source, ts_uid::JPEG_BASELINE, &registry)
        .expect("JPEG to EVRLE inline: pre-transcode to JPEG failed");

    // Verify source is valid
    let (src_count, src_bad) = validate_explicit_vr(&jpeg_source);
    assert!(
        src_bad.is_none(),
        "JPEG source has invalid VR at byte {}",
        src_bad.unwrap_or(0)
    );

    // Transcode JPEG -> EVRLE (full direct path)
    let output = transcode_dicom(&jpeg_source, ts_uid::EXPLICIT_VR_LE, &registry)
        .expect("JPEG to EVRLE inline: transcode JPEG→EVRLE failed");
    assert!(output.len() > 200, "Output too small");

    let (out_count, out_bad) = validate_explicit_vr(&output);
    assert!(
        out_bad.is_none(),
        "JPEG to EVRLE inline: output has invalid VR at byte {} ({} elements ok)",
        out_bad.unwrap_or(0),
        out_count
    );

    // Output should have at least as many elements as source
    assert!(
        out_count >= src_count,
        "JPEG to EVRLE inline: tag loss — source={src_count}, output={out_count}"
    );

    // Also verify the output decodes correctly
    let evrle_codec = registry
        .get(ts_uid::EXPLICIT_VR_LE)
        .expect("JPEG to EVRLE inline: EVRLE codec missing");
    let decoded = evrle_codec
        .decode(&output)
        .expect("JPEG to EVRLE inline: decode output failed");
    decoded
        .validate()
        .expect("JPEG to EVRLE inline: output pixels invalid");

    eprintln!(
        "[OK] JPEG→EVRLE inline — {out_count} elements valid, {} px, {}B→{}B",
        decoded.pixel_count(),
        jpeg_source.len(),
        output.len()
    );
}
