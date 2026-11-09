// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Color space conversion utilities for DICOM transcoding.
//!
//! Handles conversions between common DICOM photometric interpretations:
//! - RGB <-> YBR_FULL
//! - MONOCHROME1 <-> MONOCHROME2
//!
//! # Supported conversions
//!
//! | From | To | Notes |
//! |---|---|---|
//! | RGB | YBR_FULL | For JPEG Baseline encoding |
//! | YBR_FULL | RGB | After JPEG Baseline decoding |
//! | MONOCHROME2 | MONOCHROME1 | Invert all pixels |
//! | MONOCHROME1 | MONOCHROME2 | Invert all pixels |
//!
//! # YBR_FULL <-> RGB
//!
//! DICOM PS3.3 Section C.7.6.3.1.2 defines YBR_FULL as:
//! - Y = +0.299*R + 0.587*G + 0.114*B
//! - CB = -0.168736*R - 0.331264*G + 0.5*B + 128
//! - CR = +0.5*R - 0.418688*G - 0.081312*B + 128

#![forbid(unsafe_code)]

use super::CodecError;

/// Convert a single pixel from YBR_FULL to RGB.
///
/// All inputs and outputs are in 0..255 range.
/// Uses 16-bit fixed-point integer arithmetic for exactness.
#[inline]
pub fn ybr_full_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = y as i32;
    let cb = cb as i32 - 128;
    let cr = cr as i32 - 128;

    // Coefficients x 2^16:
    // 1.402    ->  91881
    // 0.344136 ->  22554
    // 0.714136 ->  46801
    // 1.772    -> 116130
    let r = clamp_i32(y + ((91881 * cr) >> 16));
    let g = clamp_i32(y - ((22554 * cb) >> 16) - ((46801 * cr) >> 16));
    let b = clamp_i32(y + ((116130 * cb) >> 16));

    (r, g, b)
}

/// Convert a single pixel from RGB to YBR_FULL.
#[inline]
pub fn rgb_to_ybr_full(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let r = r as i32;
    let g = g as i32;
    let b = b as i32;

    // Coefficients x 2^16:
    //  0.299    ->  19595
    //  0.587    ->  38470
    //  0.114    ->   7471
    //  0.168736 ->  11059
    //  0.331264 ->  21710
    //  0.5      ->  32768
    //  0.418688 ->  27439
    //  0.081312 ->   5329
    let y = clamp_i32(((19595 * r) + (38470 * g) + (7471 * b)) >> 16);
    let cb = clamp_i32((-((11059 * r) >> 16) - ((21710 * g) >> 16) + ((32768 * b) >> 16)) + 128);
    let cr = clamp_i32((((32768 * r) >> 16) - ((27439 * g) >> 16) - ((5329 * b) >> 16)) + 128);

    (y, cb, cr)
}

/// Clamp i32 to u8 range.
#[inline]
fn clamp_i32(v: i32) -> u8 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

/// Apply color-space conversion to an entire pixel buffer.
///
/// The input and output use planar layout (RRR...GGG...BBB...).
/// Both input and output buffers must be the same size.
pub fn convert_color_space(
    pixels: &[u8],
    from: &str,
    to: &str,
    samples_per_pixel: u8,
    bits_allocated: u8,
) -> Result<Vec<u8>, CodecError> {
    if from == to {
        return Ok(pixels.to_vec());
    }

    let pixel_count = pixels.len() / samples_per_pixel as usize;

    match (from, to) {
        ("RGB", "YBR_FULL") => {
            let mut out = vec![0u8; pixels.len()];
            for i in 0..pixel_count {
                let r = pixels[i];
                let g = pixels[pixel_count + i];
                let b = pixels[2 * pixel_count + i];
                let (y, cb, cr) = rgb_to_ybr_full(r, g, b);
                out[i] = y;
                out[pixel_count + i] = cb;
                out[2 * pixel_count + i] = cr;
            }
            Ok(out)
        }
        ("YBR_FULL", "RGB") => {
            let mut out = vec![0u8; pixels.len()];
            for i in 0..pixel_count {
                let y = pixels[i];
                let cb = pixels[pixel_count + i];
                let cr = pixels[2 * pixel_count + i];
                let (r, g, b) = ybr_full_to_rgb(y, cb, cr);
                out[i] = r;
                out[pixel_count + i] = g;
                out[2 * pixel_count + i] = b;
            }
            Ok(out)
        }
        ("MONOCHROME1", "MONOCHROME2") | ("MONOCHROME2", "MONOCHROME1") => {
            let bytes_per_sample = (bits_allocated as usize) >> 3;
            if bytes_per_sample == 0 {
                return Err(CodecError::InvalidMetadata("bits_allocated < 8".into()));
            }
            let mut out = pixels.to_vec();
            if bytes_per_sample == 1 {
                for byte in out.iter_mut() {
                    *byte = 255u8.wrapping_sub(*byte);
                }
            } else {
                // 16-bit: invert each u16 little-endian word
                for chunk in out.as_chunks_mut::<2>().0 {
                    let val = u16::from_le_bytes([chunk[0], chunk[1]]);
                    let inv = 65535u16.wrapping_sub(val);
                    chunk[0] = inv as u8;
                    chunk[1] = (inv >> 8) as u8;
                }
            }
            Ok(out)
        }
        _ => Err(CodecError::InvalidMetadata(format!(
            "Unsupported color space conversion: {from} → {to}"
        ))),
    }
}

/// Determine if a photometric interpretation is a monochrome format.
pub fn is_monochrome(pi: &str) -> bool {
    pi == "MONOCHROME1" || pi == "MONOCHROME2"
}

/// Determine if a photometric interpretation is a color format.
pub fn is_color(pi: &str) -> bool {
    pi == "RGB" || pi == "YBR_FULL" || pi == "YBR_FULL_422"
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ybr_rgb_roundtrip() {
        // Test with known values. YBR<->RGB is lossy -
        // expect results within +-2 due to integer truncation.
        let (r, g, b) = (100, 150, 200);
        let (y, cb, cr) = rgb_to_ybr_full(r, g, b);
        let (r2, g2, b2) = ybr_full_to_rgb(y, cb, cr);

        let tol = 2i32;
        assert!((r as i32 - r2 as i32).abs() <= tol, "R: {r} → {r2}");
        assert!((g as i32 - g2 as i32).abs() <= tol, "G: {g} → {g2}");
        assert!((b as i32 - b2 as i32).abs() <= tol, "B: {b} → {b2}");
    }

    #[test]
    fn test_ybr_rgb_black() {
        let (r, g, b) = ybr_full_to_rgb(0, 128, 128);
        assert!((r as i32) <= 1);
        assert!((g as i32) <= 1);
        assert!((b as i32) <= 1);
    }

    #[test]
    fn test_ybr_rgb_white() {
        let (r, g, b) = ybr_full_to_rgb(255, 128, 128);
        assert!(r >= 254);
        assert!(g >= 254);
        assert!(b >= 254);
    }

    #[test]
    fn test_rgb_to_ybr_black() {
        let (y, cb, cr) = rgb_to_ybr_full(0, 0, 0);
        assert!((y as i32) <= 1);
        assert_eq!(cb, 128);
        assert_eq!(cr, 128);
    }

    #[test]
    fn test_rgb_to_ybr_white() {
        let (y, cb, cr) = rgb_to_ybr_full(255, 255, 255);
        assert!(y >= 254);
        // Cb, Cr should be ~128 (+-1 acceptable due to integer truncation)
        assert!((cb as i32 - 128i32).abs() <= 1, "Cb: {cb}");
        assert!((cr as i32 - 128i32).abs() <= 1, "Cr: {cr}");
    }

    #[test]
    fn test_convert_color_space_identity() {
        let data = vec![10u8, 20, 30, 40, 50, 60];
        let result = convert_color_space(&data, "RGB", "RGB", 3, 8).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn test_convert_color_space_rgb_to_ybr_and_back() {
        // Create 4-pixel RGB test pattern (planar layout)
        let pixel_count = 4;
        let r = vec![100u8, 200, 50, 150];
        let g = vec![150u8, 100, 200, 50];
        let b = vec![200u8, 50, 150, 100];
        let mut data = Vec::new();
        data.extend_from_slice(&r);
        data.extend_from_slice(&g);
        data.extend_from_slice(&b);

        let ybr = convert_color_space(&data, "RGB", "YBR_FULL", 3, 8).unwrap();
        assert_eq!(ybr.len(), data.len());

        let rgb_back = convert_color_space(&ybr, "YBR_FULL", "RGB", 3, 8).unwrap();

        let tol = 3i32; // RGB<->YBR roundtrip is inherently lossy
        for i in 0..pixel_count {
            assert!(
                (data[i] as i32 - rgb_back[i] as i32).abs() <= tol,
                "R[{i}]: {} vs {}",
                data[i],
                rgb_back[i]
            );
            assert!(
                (data[pixel_count + i] as i32 - rgb_back[pixel_count + i] as i32).abs() <= tol,
                "G[{i}]: {} vs {}",
                data[pixel_count + i],
                rgb_back[pixel_count + i]
            );
            assert!(
                (data[2 * pixel_count + i] as i32 - rgb_back[2 * pixel_count + i] as i32).abs()
                    <= tol,
                "B[{i}]: {} vs {}",
                data[2 * pixel_count + i],
                rgb_back[2 * pixel_count + i]
            );
        }
    }

    #[test]
    fn test_monochrome_invert() {
        let data = vec![0u8, 128, 255, 64];
        let inverted = convert_color_space(&data, "MONOCHROME2", "MONOCHROME1", 1, 8).unwrap();
        assert_eq!(inverted, vec![255, 127, 0, 191]);
    }

    #[test]
    fn test_monochrome_double_invert() {
        let data = vec![10u8, 20, 30, 40];
        let inverted1 = convert_color_space(&data, "MONOCHROME2", "MONOCHROME1", 1, 8).unwrap();
        let inverted2 =
            convert_color_space(&inverted1, "MONOCHROME1", "MONOCHROME2", 1, 8).unwrap();
        assert_eq!(inverted2, data);
    }

    #[test]
    fn test_unsupported_conversion() {
        let data = vec![0u8; 10];
        let result = convert_color_space(&data, "RGB", "YBR_ICT", 3, 8);
        assert!(result.is_err());
    }

    #[test]
    fn test_is_monochrome() {
        assert!(is_monochrome("MONOCHROME1"));
        assert!(is_monochrome("MONOCHROME2"));
        assert!(!is_monochrome("RGB"));
    }

    #[test]
    fn test_is_color() {
        assert!(is_color("RGB"));
        assert!(is_color("YBR_FULL"));
        assert!(is_color("YBR_FULL_422"));
        assert!(!is_color("MONOCHROME2"));
    }
}
