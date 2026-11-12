// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Explicit VR Big Endian codec - uncompressed, byte-swapped passthrough.
//!
//! Handles Explicit VR BE (1.2.840.10008.1.2.2) as both source and target.
//! Pixel data in BE is OW-big-endian - each 16-bit sample is byte-swapped
//! relative to LE. The codec swaps every 2 bytes on decode/encode.
//!
//! DICOM Part 10 Section 7.3: Meta-header is ALWAYS Explicit VR Little Endian.
//! Only the dataset (after DICM and meta-header) is Big Endian.
//! The parser (`DicomRsParser`) handles this automatically - it detects
//! the TS from the meta-header and navigates the dataset correctly.
//!
//! However, numeric tag VALUES (Rows, Columns, BitsAllocated, etc.) are
//! stored as Big Endian bytes in the BE dataset. The codec must read them
//! with `from_be_bytes` (the parser returns raw bytes without interpreting
//! their endianness).

#![forbid(unsafe_code)]

use super::{CodecError, DecodedPixels, DicomCodec, DicomMeta, ts_uid};
use super::{
    TAG_BITS_ALLOCATED, TAG_BITS_STORED, TAG_COLUMNS, TAG_NUMBER_OF_FRAMES,
    TAG_PHOTOMETRIC_INTERPRETATION, TAG_PIXEL_DATA, TAG_PLANAR_CONFIGURATION, TAG_ROWS,
    TAG_SAMPLES_PER_PIXEL,
};
use crate::dicom_rs::DicomRsParser;

/// Codec for Explicit VR Big Endian (1.2.840.10008.1.2.2).
///
/// Also supports Explicit VR LE (1.2.840.10008.1.2.1) and
/// Implicit VR LE (1.2.840.10008.1.2) as source formats -
/// pixel data layout is the same after byte-order normalization.
pub struct BigEndianCodec;

impl DicomCodec for BigEndianCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::EXPLICIT_VR_BE
    }

    fn supported_source_ts(&self) -> &[&str] {
        &[] // Only handles BE -- LE/Implicit LE -> ExplicitVrLeCodec
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        let parser = DicomRsParser;

        // -- Metadata ----------------------------------------------
        // BE numeric values use Big Endian byte order.
        // Strings (PN, UI, CS, etc.) are byte-order independent.
        let get_u16 = |tag| -> Result<Option<u16>, CodecError> {
            parser
                .extract_tag_value(dicom_bytes, tag)
                .map_err(|e| CodecError::DecodeFailed(e.to_string()))
                .map(|opt| {
                    opt.map(|v| {
                        if v.len() >= 2 {
                            u16::from_be_bytes([v[0], v[1]])
                        } else if v.len() == 1 {
                            v[0] as u16
                        } else {
                            0
                        }
                    })
                })
        };

        let get_str = |tag| -> Result<Option<String>, CodecError> {
            parser
                .extract_tag_value(dicom_bytes, tag)
                .map_err(|e| CodecError::DecodeFailed(e.to_string()))
                .map(|opt| {
                    opt.map(|v| {
                        String::from_utf8_lossy(&v)
                            .trim_end_matches([' ', '\0'])
                            .to_string()
                    })
                })
        };

        let rows = get_u16(TAG_ROWS)?
            .ok_or_else(|| CodecError::InvalidMetadata("Missing Rows (0028,0010)".into()))?;
        let columns = get_u16(TAG_COLUMNS)?
            .ok_or_else(|| CodecError::InvalidMetadata("Missing Columns (0028,0011)".into()))?;
        let samples_per_pixel: u8 = get_u16(TAG_SAMPLES_PER_PIXEL)?
            .map(|v| v as u8)
            .unwrap_or(1);
        let bits_allocated: u8 = get_u16(TAG_BITS_ALLOCATED)?.ok_or_else(|| {
            CodecError::InvalidMetadata("Missing BitsAllocated (0028,0100)".into())
        })? as u8;
        let _bits_stored: u8 = get_u16(TAG_BITS_STORED)?
            .map(|v| v as u8)
            .unwrap_or(bits_allocated);
        let photometric_interpretation =
            get_str(TAG_PHOTOMETRIC_INTERPRETATION)?.unwrap_or_else(|| "MONOCHROME2".to_string());
        // (0028,0006): 0 = colour-by-pixel, 1 = colour-by-plane. The swap above
        // is a pure byte reversal, so it is correct for either layout; the
        // planar conversion below is not, and applying it to an already-planar
        // source interleaved R, G and B into each other.
        let planar_configuration: u16 = get_u16(TAG_PLANAR_CONFIGURATION)?.unwrap_or(0);

        // -- PixelData ---------------------------------------------
        let pixel_data = parser
            .extract_tag_value(dicom_bytes, TAG_PIXEL_DATA)
            .map_err(|e| CodecError::DecodeFailed(e.to_string()))?
            .ok_or_else(|| CodecError::DecodeFailed("PixelData (7FE0,0010) not found".into()))?;

        // NumberOfFrames (0028,0008) IS, default 1. Multi-frame sources hold
        // all frames concatenated in PixelData, and the per-frame planes of a
        // colour-by-plane source are contiguous within each frame - so the
        // planar conversion needs the frame count, exactly as the Explicit VR
        // LE codec does.
        let frames: u32 = parser
            .extract_tag_value(dicom_bytes, TAG_NUMBER_OF_FRAMES)
            .map_err(|e| CodecError::DecodeFailed(e.to_string()))?
            .map(|v| {
                String::from_utf8_lossy(&v)
                    .trim()
                    .trim_end_matches('\0')
                    .parse::<u32>()
                    .unwrap_or(1)
            })
            .unwrap_or(1)
            .max(1);

        let expected_size = rows as usize
            * columns as usize
            * samples_per_pixel as usize
            * (bits_allocated as usize >> 3)
            * frames as usize;
        if pixel_data.len() != expected_size {
            return Err(CodecError::DecodeFailed(format!(
                "PixelData size mismatch: got {} bytes, expected {} ({}x{}x{}x{}x{} frames)",
                pixel_data.len(),
                expected_size,
                rows,
                columns,
                samples_per_pixel,
                bits_allocated >> 3,
                frames,
            )));
        }

        // -- Byte-swap BE -> LE -------------------------------------
        let mut pixels = pixel_data;
        if bits_allocated == 16 {
            swap_bytes_16(&mut pixels);
        }
        // For 8-bit: bytes are identical (OB is byte-order independent).

        // -- Planar conversion -------------------------------------
        // The codec's convention is planar (RRR...GGG...BBB... per frame), so
        // an interleaved source is reordered and a colour-by-plane source is
        // already in that shape. Comparing against `1` matters: `!= 1` is the
        // interleaved case, and treating a planar source as interleaved
        // shuffles the three channels into each other.
        let planar = if samples_per_pixel == 3 && planar_configuration != 1 {
            interleaved_to_planar_frames(
                &pixels,
                rows as usize,
                columns as usize,
                frames as usize,
                (bits_allocated as usize) >> 3,
            )
        } else {
            pixels
        };

        Ok(DecodedPixels {
            data: planar,
            rows,
            columns,
            samples_per_pixel,
            bits_allocated,
            photometric_interpretation,
        })
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        // Explicit VR BE = uncompressed, but pixel data stored Big Endian.
        // Convert back from planar to interleaved if needed, then byte-swap LE -> BE.
        let mut data = if pixels.samples_per_pixel == 3 {
            planar_to_interleaved_frames(
                &pixels.data,
                pixels.rows as usize,
                pixels.columns as usize,
                meta.frames.max(1) as usize,
                (pixels.bits_allocated as usize) >> 3,
            )
        } else {
            pixels.data.clone()
        };

        if pixels.bits_allocated == 16 {
            swap_bytes_16(&mut data);
        }
        // For 8-bit: bytes remain as-is.

        Ok(data)
    }
}

// ===========================================================================
// Byte-swap helpers
// ===========================================================================

/// Swap every pair of bytes in-place (LE <-> BE for 16-bit data).
///
/// For BitsAllocated=8: data is byte-order independent (OB).
/// For BitsAllocated=16: each 16-bit sample needs byte-swap.
fn swap_bytes_16(data: &mut [u8]) {
    for chunk in data.as_chunks_mut::<2>().0 {
        chunk.swap(0, 1);
    }
}

// ===========================================================================
// Pixel layout conversion helpers (same as explicit_vr_le.rs)
// ===========================================================================

/// Convert interleaved RGB pixel data to planar (RRR...GGG...BBB...), one
/// frame at a time.
///
/// Multi-frame PixelData concatenates whole frames, so the planes of each
/// frame are contiguous per frame: converting the buffer as one frame would
/// have moved the second frame's samples into the first frame's planes.
fn interleaved_to_planar_frames(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Vec<u8> {
    let frame_pixels = rows * cols;
    let plane_bytes = frame_pixels * bps;
    let frame_bytes = plane_bytes * 3;
    let mut planar = vec![0u8; data.len()];
    for f in 0..frames {
        let src = &data[f * frame_bytes..(f + 1) * frame_bytes];
        let dst = &mut planar[f * frame_bytes..(f + 1) * frame_bytes];
        let (r_plane, rest) = dst.split_at_mut(plane_bytes);
        let (g_plane, b_plane) = rest.split_at_mut(plane_bytes);
        for px in 0..frame_pixels {
            let s = px * 3 * bps;
            r_plane[px * bps..(px + 1) * bps].copy_from_slice(&src[s..s + bps]);
            g_plane[px * bps..(px + 1) * bps].copy_from_slice(&src[s + bps..s + 2 * bps]);
            b_plane[px * bps..(px + 1) * bps].copy_from_slice(&src[s + 2 * bps..s + 3 * bps]);
        }
    }
    planar
}

/// Convert planar pixel data (RRR...GGG...BBB...) back to interleaved RGB,
/// one frame at a time.
fn planar_to_interleaved_frames(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Vec<u8> {
    let frame_pixels = rows * cols;
    let plane_bytes = frame_pixels * bps;
    let frame_bytes = plane_bytes * 3;
    let mut interleaved = vec![0u8; data.len()];
    for f in 0..frames {
        let src = &data[f * frame_bytes..(f + 1) * frame_bytes];
        let dst = &mut interleaved[f * frame_bytes..(f + 1) * frame_bytes];
        for px in 0..frame_pixels {
            let d = px * 3 * bps;
            let r = px * bps;
            dst[d..d + bps].copy_from_slice(&src[r..r + bps]);
            dst[d + bps..d + 2 * bps].copy_from_slice(&src[plane_bytes + r..plane_bytes + r + bps]);
            dst[d + 2 * bps..d + 3 * bps]
                .copy_from_slice(&src[2 * plane_bytes + r..2 * plane_bytes + r + bps]);
        }
    }
    interleaved
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- swap_bytes_16 ------------------------------------------------

    #[test]
    fn test_swap_bytes_16_empty() {
        let mut data: Vec<u8> = vec![];
        swap_bytes_16(&mut data);
        assert!(data.is_empty());
    }

    #[test]
    fn test_swap_bytes_16_single_pair() {
        let mut data = vec![0x00, 0x01];
        swap_bytes_16(&mut data);
        assert_eq!(data, vec![0x01, 0x00]);
    }

    #[test]
    fn test_swap_bytes_16_multiple_pairs() {
        let mut data = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        swap_bytes_16(&mut data);
        assert_eq!(data, vec![0x01, 0x00, 0x03, 0x02, 0x05, 0x04]);
    }

    #[test]
    fn test_swap_bytes_16_double_swap_identity() {
        let original = vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC];
        let mut data = original.clone();
        swap_bytes_16(&mut data);
        swap_bytes_16(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn test_swap_bytes_16_odd_length() {
        // chunks_exact_mut ignores trailing odd byte
        let mut data = vec![0x00, 0x01, 0x02];
        swap_bytes_16(&mut data);
        assert_eq!(data, vec![0x01, 0x00, 0x02]); // last byte untouched
    }

    // -- Codec identity -----------------------------------------------

    #[test]
    fn test_be_codec_transfer_syntax() {
        let codec = BigEndianCodec;
        assert_eq!(codec.transfer_syntax_uid(), ts_uid::EXPLICIT_VR_BE);
    }

    #[test]
    fn test_be_codec_supported_source_ts() {
        let codec = BigEndianCodec;
        let sources = codec.supported_source_ts();
        assert!(sources.is_empty()); // BE codec only handles BE, not LE/Implicit LE
    }

    // -- Roundtrip: LE -> BE -> LE pixel identity -----------------------

    #[test]
    fn test_be_roundtrip_8bit_monochrome() {
        // Create synthetic 8-bit monochrome pixels
        let pixels = DecodedPixels {
            data: (0..100u8).collect::<Vec<_>>(), // 10x10 = 100 bytes
            rows: 10,
            columns: 10,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let meta = DicomMeta {
            rows: 10,
            columns: 10,
            samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };

        let codec = BigEndianCodec;
        let encoded = codec.encode(&pixels, &meta).expect("encode should succeed");
        // For 8-bit, BE encoded bytes should be identical (no byte-swap needed)
        assert_eq!(encoded, pixels.data);

        // decode would need a full DICOM file, so not tested here.
        // Roundtrip via encode->swap->compare is tested below.
    }

    #[test]
    fn test_be_roundtrip_16bit_monochrome() {
        // Create synthetic 16-bit monochrome pixels (LE)
        // 2x2 image = 4 pixels x 2 bytes = 8 bytes
        let le_pixels: Vec<u8> = vec![
            0x34, 0x12, // pixel 0: 0x1234 in LE
            0x78, 0x56, // pixel 1: 0x5678
            0xBC, 0x9A, // pixel 2: 0x9ABC
            0xF0, 0xDE, // pixel 3: 0xDEF0
        ];
        let pixels = DecodedPixels {
            data: le_pixels.clone(),
            rows: 2,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let meta = DicomMeta {
            rows: 2,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            bits_stored: 16,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };

        let codec = BigEndianCodec;

        // Encode: LE -> BE
        let be_encoded = codec.encode(&pixels, &meta).expect("encode should succeed");
        // Verify BE byte order
        assert_eq!(
            be_encoded,
            vec![
                0x12, 0x34, // pixel 0: 0x1234 in BE
                0x56, 0x78, // pixel 1: 0x5678
                0x9A, 0xBC, // pixel 2: 0x9ABC
                0xDE, 0xF0, // pixel 3: 0xDEF0
            ]
        );

        // Decode back: swap BE -> LE
        let mut decoded = be_encoded.clone();
        swap_bytes_16(&mut decoded);
        assert_eq!(decoded, le_pixels);
    }

    #[test]
    fn test_be_encode_then_swap_gives_le() {
        // Verify that encode(BE) followed by swap_bytes_16 gives original LE data
        let le_data: Vec<u8> = (0..200u8).collect(); // 10x10x2 = 200 bytes
        let pixels = DecodedPixels {
            data: le_data.clone(),
            rows: 10,
            columns: 10,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let meta = DicomMeta {
            rows: 10,
            columns: 10,
            samples_per_pixel: 1,
            bits_allocated: 16,
            bits_stored: 16,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };

        let codec = BigEndianCodec;
        let be_encoded = codec.encode(&pixels, &meta).expect("encode should succeed");

        // Swap BE -> LE
        let mut back_to_le = be_encoded;
        swap_bytes_16(&mut back_to_le);
        assert_eq!(back_to_le, le_data);

        // Double swap -> identity
        swap_bytes_16(&mut back_to_le);
        let mut swapped_twice = back_to_le;
        swap_bytes_16(&mut swapped_twice);
        assert_eq!(swapped_twice, le_data);
    }

    /// The planar conversion is per FRAME, checked where it can actually be
    /// observed: a 2-pixel-wide frame makes a single-frame conversion produce
    /// a different order than the per-frame one.
    ///
    /// The planar conversion is per FRAME, in the frame-major layout the
    /// Explicit VR LE codec already produces: all three planes of frame 0,
    /// then all three of frame 1.
    ///
    /// Frame 0 = (1,2,3) (4,5,6); frame 1 = (7,8,9) (10,11,12).
    /// Expected: 1,4 | 2,5 | 3,6 | 7,10 | 8,11 | 9,12.
    /// A buffer-wide conversion would emit 1,4,7,10 | 2,5,8,11 | 3,6,9,12 -
    /// the second frame's red plane lands in the first frame's green plane.
    #[test]
    fn a_multi_frame_conversion_keeps_frames_apart() {
        let interleaved: Vec<u8> = (1..=12).collect();

        let planar = interleaved_to_planar_frames(&interleaved, 1, 2, 2, 1);
        assert_eq!(
            planar,
            vec![1u8, 4, 2, 5, 3, 6, 7, 10, 8, 11, 9, 12],
            "each frame keeps its own three planes"
        );
        assert_eq!(
            planar_to_interleaved_frames(&planar, 1, 2, 2, 1),
            interleaved,
            "the conversion is reversible"
        );
    }

    /// A 16-bit colour instance interleaves 16-bit samples, so the plane
    /// stride is TWO bytes per pixel - not one.
    ///
    /// Handled sample-wise, the first byte of a sample stays with its own
    /// channel; handled byte-wise, the high and low halves of neighbouring
    /// samples swap channels, which reads as a colour fringe on every edge.
    #[test]
    fn a_sixteen_bit_colour_instance_keeps_samples_whole() {
        // Two pixels: R=(1,2) G=(3,4) B=(5,6), then R=(7,8) G=(9,10) B=(11,12).
        let interleaved: Vec<u8> = (1..=12).collect();
        let planar = interleaved_to_planar_frames(&interleaved, 1, 2, 1, 2);
        assert_eq!(
            planar,
            vec![1u8, 2, 7, 8, 3, 4, 9, 10, 5, 6, 11, 12],
            "each plane holds whole 2-byte samples"
        );
        assert_eq!(
            planar_to_interleaved_frames(&planar, 1, 2, 1, 2),
            interleaved,
            "the conversion is reversible"
        );
    }
}
