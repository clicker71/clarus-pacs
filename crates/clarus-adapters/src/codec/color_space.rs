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

/// The sample interpretation needed to convert a buffer losslessly.
#[derive(Debug, Clone, Copy)]
pub struct SampleShape {
    /// BitsAllocated (0028,0100): the container width of one sample.
    pub bits_allocated: u8,
    /// BitsStored (0028,0101): the meaningful width inside that container.
    pub bits_stored: u8,
    /// PixelRepresentation (0028,0103): 0 unsigned, 1 two's complement.
    pub pixel_representation: u8,
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
    shape: SampleShape,
) -> Result<Vec<u8>, CodecError> {
    if from == to {
        return Ok(pixels.to_vec());
    }

    // PS3.3 C.7.6.3.1.1: SamplesPerPixel is 1 or 3 and the planar layout is
    // one whole plane per sample. Dividing by zero panicked, and a buffer that
    // is not a whole number of equal planes indexed past its end - both on
    // input the caller supplies.
    if samples_per_pixel == 0 {
        return Err(CodecError::InvalidMetadata(
            "SamplesPerPixel must not be 0".into(),
        ));
    }
    let pixel_count = pixels.len() / samples_per_pixel as usize;
    if pixel_count * samples_per_pixel as usize != pixels.len() {
        return Err(CodecError::InvalidMetadata(format!(
            "Planar buffer of {} bytes is not a whole number of {} sample planes",
            pixels.len(),
            samples_per_pixel
        )));
    }

    match (from, to) {
        ("RGB", "YBR_FULL") | ("YBR_FULL", "RGB") => {
            // PS3.3 C.7.6.3.1.1: RGB and YBR_FULL are defined on three 8-bit
            // components. A 16-bit colour buffer read one byte at a time was
            // halved in resolution and wrong in value; a buffer without three
            // planes indexed past its end.
            if samples_per_pixel != 3 {
                return Err(CodecError::InvalidMetadata(format!(
                    "{from} -> {to} needs SamplesPerPixel 3, got {samples_per_pixel}"
                )));
            }
            if shape.bits_allocated != 8 {
                return Err(CodecError::InvalidMetadata(format!(
                    "{from} -> {to} is defined for 8-bit samples, got BitsAllocated {}",
                    shape.bits_allocated
                )));
            }
            let mut out = vec![0u8; pixels.len()];
            if from == "RGB" {
                for i in 0..pixel_count {
                    let r = pixels[i];
                    let g = pixels[pixel_count + i];
                    let b = pixels[2 * pixel_count + i];
                    let (y, cb, cr) = rgb_to_ybr_full(r, g, b);
                    out[i] = y;
                    out[pixel_count + i] = cb;
                    out[2 * pixel_count + i] = cr;
                }
            } else {
                for i in 0..pixel_count {
                    let y = pixels[i];
                    let cb = pixels[pixel_count + i];
                    let cr = pixels[2 * pixel_count + i];
                    let (r, g, b) = ybr_full_to_rgb(y, cb, cr);
                    out[i] = r;
                    out[pixel_count + i] = g;
                    out[2 * pixel_count + i] = b;
                }
            }
            Ok(out)
        }
        ("MONOCHROME1", "MONOCHROME2") | ("MONOCHROME2", "MONOCHROME1") => {
            let bytes_per_sample = (shape.bits_allocated as usize) >> 3;
            if bytes_per_sample != 1 && bytes_per_sample != 2 {
                return Err(CodecError::InvalidMetadata(format!(
                    "BitsAllocated {} is not 8 or 16",
                    shape.bits_allocated
                )));
            }
            // PS3.3 C.7.6.3.1.1 with PS3.5 8.1.1: the samples carry BitsStored
            // bits and the container may be wider, so the range to invert
            // WITHIN is [0, 2^BitsStored - 1] (unsigned) or the signed range of
            // the same width. Complementing the whole container turned a
            // 12-bit value stored in 16 bits into a 16-bit one, so the four
            // uninitialised high bits became signal.
            let stored = shape.bits_stored;
            if stored == 0 || stored > shape.bits_allocated || shape.bits_allocated > 16 {
                return Err(CodecError::InvalidMetadata(format!(
                    "BitsStored {stored} does not fit BitsAllocated {}",
                    shape.bits_allocated
                )));
            }
            if shape.pixel_representation > 1 {
                return Err(CodecError::InvalidMetadata(format!(
                    "PixelRepresentation must be 0 or 1, got {}",
                    shape.pixel_representation
                )));
            }
            let mut out = pixels.to_vec();
            if bytes_per_sample == 1 {
                // PS3.5 8.1.1: a one-byte container holds 8 bits, so BitsStored
                // 8 is the only width it can carry.
                if stored != 8 {
                    return Err(CodecError::InvalidMetadata(format!(
                        "8-bit container cannot hold BitsStored {stored}"
                    )));
                }
                for byte in out.iter_mut() {
                    *byte = 255u8.wrapping_sub(*byte);
                }
            } else {
                // PS3.5 8.1.1: a 16-bit container holds 2 bytes per sample, so
                // the buffer must be a whole number of samples. `as_chunks_mut`
                // silently ignores a trailing byte, so an odd-length buffer
                // (which the plane check admits when spp is 1) lost its last
                // sample with no error and the image came back one sample short.
                if !out.len().is_multiple_of(2) {
                    return Err(CodecError::InvalidMetadata(format!(
                        "16-bit container holds {} bytes, which is not a whole number of samples",
                        out.len()
                    )));
                }
                let mask: u16 = if stored == 16 {
                    u16::MAX
                } else {
                    (1u16 << stored) - 1
                };
                // PS3.3 C.7.6.3.1.1: the polarity swap mirrors the sample
                // about the middle of its range, i.e. `max + min - v`. For an
                // unsigned b-bit range that is `mask - v`; for a signed one it
                // is `-1 - v`, which the same `mask - v` produces because a
                // two's complement encoding wraps exactly that way.
                //
                // The wrap is deliberate: `mask - v` keeps the result inside
                // the stored width, which is the bit width the file declares.
                for chunk in out.as_chunks_mut::<2>().0 {
                    let val = u16::from_le_bytes([chunk[0], chunk[1]]);
                    let stored_val = val & mask;
                    let inv = mask.wrapping_sub(stored_val) & mask;
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
        let result = convert_color_space(&data, "RGB", "RGB", 3, shape8(3)).unwrap();
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

        let ybr = convert_color_space(&data, "RGB", "YBR_FULL", 3, shape8(3)).unwrap();
        assert_eq!(ybr.len(), data.len());

        let rgb_back = convert_color_space(&ybr, "YBR_FULL", "RGB", 3, shape8(3)).unwrap();

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
        let inverted =
            convert_color_space(&data, "MONOCHROME2", "MONOCHROME1", 1, shape8(1)).unwrap();
        assert_eq!(inverted, vec![255, 127, 0, 191]);
    }

    #[test]
    fn test_monochrome_double_invert() {
        let data = vec![10u8, 20, 30, 40];
        let inverted1 =
            convert_color_space(&data, "MONOCHROME2", "MONOCHROME1", 1, shape8(1)).unwrap();
        let inverted2 =
            convert_color_space(&inverted1, "MONOCHROME1", "MONOCHROME2", 1, shape8(1)).unwrap();
        assert_eq!(inverted2, data);
    }

    /// PS3.5 8.1.1: a 16-bit container holds two bytes per sample, so an
    /// odd-length buffer is not a whole number of samples.
    ///
    /// `as_chunks_mut::<2>` silently ignores a trailing byte, and the plane
    /// check admits an odd buffer whenever SamplesPerPixel is 1 - so the last
    /// sample was dropped with no error and the image came back one short.
    #[test]
    fn a_sixteen_bit_buffer_that_holds_half_a_sample_is_refused() {
        // 5 bytes at 16 bits per sample: two whole samples plus one stray byte.
        let data = vec![1u8, 2, 3, 4, 5];
        let shape = SampleShape {
            bits_allocated: 16,
            bits_stored: 16,
            pixel_representation: 0,
        };
        let err = convert_color_space(&data, "MONOCHROME2", "MONOCHROME1", 1, shape)
            .expect_err("an odd 16-bit buffer is not a whole number of samples");
        assert!(
            err.to_string().contains("not a whole number of samples"),
            "got: {err}"
        );

        // An even buffer of the same width still inverts.
        let ok = convert_color_space(&[1u8, 2, 3, 4], "MONOCHROME2", "MONOCHROME1", 1, shape)
            .expect("two whole samples convert");
        assert_eq!(ok.len(), 4);
    }

    #[test]
    fn test_unsupported_conversion() {
        // A buffer that IS a whole number of planes, so the plane check passes
        // and the unsupported pair is what refuses. Ten bytes with three
        // components is not a whole number of planes, so the earlier version
        // exercised the plane check and never reached the branch it names.
        let data = vec![0u8; 9]; // three pixels, three 8-bit components
        let result = convert_color_space(&data, "RGB", "YBR_ICT", 3, shape8(3));
        let err = result.expect_err("RGB -> YBR_ICT is not implemented");
        assert!(
            err.to_string()
                .contains("Unsupported color space conversion"),
            "must reach the unsupported-pair branch, got: {err}"
        );
    }

    /// An 8-bit, unsigned sample shape: the common case the colour tests use.
    fn shape8(_samples: u8) -> SampleShape {
        SampleShape {
            bits_allocated: 8,
            bits_stored: 8,
            pixel_representation: 0,
        }
    }

    /// PS3.3 C.7.6.3.1.1: SamplesPerPixel 0 is not a sample count, so it is
    /// refused instead of dividing by it.
    #[test]
    fn a_zero_sample_count_is_refused_not_divided_by() {
        let data = vec![0u8; 12];
        assert!(
            convert_color_space(&data, "RGB", "YBR_FULL", 0, shape8(0)).is_err(),
            "SamplesPerPixel 0 must be an error"
        );
    }

    /// Three planes are R, G and B - a buffer that is not a whole number of
    /// planes cannot be laid out that way.
    #[test]
    fn a_ragged_plane_count_is_refused_not_indexed() {
        let data = vec![0u8; 5];
        assert!(convert_color_space(&data, "RGB", "YBR_FULL", 3, shape8(3)).is_err());
    }

    /// PS3.3 C.7.6.3.1.1: a two-plane buffer with SamplesPerPixel 3 indexes
    /// past its end on the third plane.
    #[test]
    fn a_two_plane_buffer_with_three_samples_is_refused() {
        let data = vec![0u8; 7];
        let err = convert_color_space(&data, "RGB", "YBR_FULL", 3, shape8(3))
            .expect_err("7 bytes is not a whole number of planes");
        let CodecError::InvalidMetadata(msg) = err else {
            panic!("expected InvalidMetadata");
        };
        assert!(msg.contains("whole number"), "{msg}");
    }

    /// RGB <-> YBR_FULL is an 8-bit three-component transform. A 16-bit buffer
    /// read one byte per sample was halved in resolution.
    #[test]
    fn a_sixteen_bit_colour_buffer_is_refused() {
        let data = vec![0u8; 12];
        let shape = SampleShape {
            bits_allocated: 16,
            bits_stored: 12,
            pixel_representation: 0,
        };
        assert!(convert_color_space(&data, "RGB", "YBR_FULL", 3, shape).is_err());
    }

    /// PS3.3 C.7.6.3.1.1 with PS3.5 8.1.1: a 12-bit sample stored in 16 bits
    /// inverts within [0, 4095]; complementing the container gave 65535 - v.
    #[test]
    fn a_twelve_bit_sample_inverts_within_its_stored_range() {
        let mut data = Vec::new();
        for value in [0u16, 4095, 1000] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        let shape = SampleShape {
            bits_allocated: 16,
            bits_stored: 12,
            pixel_representation: 0,
        };
        let out = convert_color_space(&data, "MONOCHROME2", "MONOCHROME1", 1, shape)
            .expect("a 12-in-16 instance is legal");
        let values: Vec<u16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(values, vec![4095, 0, 3095], "inverted inside 12 bits");
    }

    /// A signed sample inverts about the midpoint of its signed range, so the
    /// most negative value becomes the most positive.
    #[test]
    fn a_signed_sample_inverts_about_its_midpoint() {
        // 12-bit signed: -2048 .. 2047, stored two's complement in 16 bits.
        let mut data = Vec::new();
        for value in [-2048i16, 0, -1, 2047] {
            data.extend_from_slice(&(value as u16).to_le_bytes());
        }
        let shape = SampleShape {
            bits_allocated: 16,
            bits_stored: 12,
            pixel_representation: 1,
        };
        let out = convert_color_space(&data, "MONOCHROME2", "MONOCHROME1", 1, shape)
            .expect("signed 12-in-16 is legal");
        let stored: Vec<u16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]) & 0x0FFF)
            .collect();
        let signed: Vec<i16> = stored
            .iter()
            .map(|v| {
                if *v >= 0x0800 {
                    (*v as i32 - 0x1000) as i16
                } else {
                    *v as i16
                }
            })
            .collect();
        assert_eq!(
            signed,
            vec![2047, -1, 0, -2048],
            "max + min - v, i.e. -1 - v for a signed sample"
        );
    }

    /// A 1-byte container cannot carry more than 8 stored bits.
    #[test]
    fn a_stored_width_wider_than_its_container_is_refused() {
        let data = vec![1u8; 4];
        let shape = SampleShape {
            bits_allocated: 8,
            bits_stored: 12,
            pixel_representation: 0,
        };
        assert!(convert_color_space(&data, "MONOCHROME2", "MONOCHROME1", 1, shape).is_err());
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
