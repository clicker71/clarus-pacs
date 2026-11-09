// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Explicit VR Little Endian codec - uncompressed passthrough.
//!
//! Handles both Explicit VR LE (1.2.840.10008.1.2.1) and
//! Implicit VR LE (1.2.840.10008.1.2) as source formats.
//! Uses `dicom_rs` parser for robust tag extraction.

#![forbid(unsafe_code)]

use super::{CodecError, DecodedPixels, DicomCodec, DicomMeta, ts_uid};
use super::{
    TAG_BITS_ALLOCATED, TAG_BITS_STORED, TAG_COLUMNS, TAG_NUMBER_OF_FRAMES,
    TAG_PHOTOMETRIC_INTERPRETATION, TAG_PIXEL_DATA, TAG_PLANAR_CONFIGURATION, TAG_ROWS,
    TAG_SAMPLES_PER_PIXEL,
};
use crate::dicom_rs::DicomRsParser;

/// Codec for Explicit VR Little Endian (1.2.840.10008.1.2.1).
///
/// Also supports Implicit VR LE (1.2.840.10008.1.2) as a source format -
/// the pixel data layout is identical (raw uncompressed bytes), only the
/// tag-navigation differs (no VR bytes, always 4-byte lengths).
///
/// Pixel data is already uncompressed - `decode()` reads it directly,
/// `encode()` writes it back as-is.
pub struct ExplicitVrLeCodec;

impl DicomCodec for ExplicitVrLeCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::EXPLICIT_VR_LE
    }

    fn supported_source_ts(&self) -> &[&str] {
        &[ts_uid::IMPLICIT_VR_LE]
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        let parser = DicomRsParser;

        // -- Metadata ----------------------------------------------
        let get_u16 = |tag| -> Result<Option<u16>, CodecError> {
            parser
                .extract_tag_value(dicom_bytes, tag)
                .map_err(|e| CodecError::DecodeFailed(e.to_string()))
                .map(|opt| {
                    opt.map(|v| {
                        if v.len() >= 2 {
                            u16::from_le_bytes([v[0], v[1]])
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
        // (0028,0006): 0 = colour-by-pixel (interleaved), 1 = colour-by-plane.
        // Treating a planar source as interleaved shuffled R, G and B into
        // each other - every channel took its neighbour's samples.
        let planar_configuration: u16 = get_u16(TAG_PLANAR_CONFIGURATION)?.unwrap_or(0);

        // NumberOfFrames (0028,0008) IS, default 1. Multi-frame sources hold
        // all frames concatenated in PixelData (one frame_size block per frame).
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
            .unwrap_or(1);

        // -- PixelData ---------------------------------------------
        let pixel_data = parser
            .extract_tag_value(dicom_bytes, TAG_PIXEL_DATA)
            .map_err(|e| CodecError::DecodeFailed(e.to_string()))?
            .ok_or_else(|| CodecError::DecodeFailed("PixelData (7FE0,0010) not found".into()))?;

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

        // -- Planar conversion -------------------------------------
        let planar = if samples_per_pixel == 3 {
            if planar_configuration == 1 {
                // Already per-plane: the codec's convention is exactly this
                // layout, so nothing to reorder.
                pixel_data
            } else {
                interleaved_to_planar(
                    &pixel_data,
                    rows as usize,
                    columns as usize,
                    frames as usize,
                    (bits_allocated as usize) >> 3,
                )?
            }
        } else {
            pixel_data
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
        // Explicit VR LE = uncompressed.
        // Convert back from planar to interleaved if needed.
        if pixels.samples_per_pixel == 3 {
            Ok(planar_to_interleaved(
                &pixels.data,
                pixels.rows as usize,
                pixels.columns as usize,
                meta.frames as usize,
                (pixels.bits_allocated as usize) >> 3,
            )?)
        } else {
            Ok(pixels.data.clone())
        }
    }
}

// ===========================================================================
// Pixel layout conversion helpers
// ===========================================================================

/// Convert interleaved RGB pixel data to planar (RRR...GGG...BBB...).
///
/// Interleaved: RGB RGB RGB ... (per frame)
/// Planar:      RRR... GGG... BBB... (per plane, frame-major)
///
/// Multi-frame-aware: `frames` blocks are converted and concatenated.
/// `bps` = bytes per sample (1 or 2).
fn interleaved_to_planar(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Result<Vec<u8>, CodecError> {
    if bps == 0 || bps > 2 {
        return Err(CodecError::DecodeFailed(format!(
            "unsupported bytes-per-sample {bps}"
        )));
    }
    let frame_pixels = rows * cols;
    let frame_bytes = frame_pixels * 3 * bps;
    if data.len() != frame_bytes * frames {
        return Err(CodecError::DecodeFailed(format!(
            "interleaved size mismatch: got {}, expected {} ({} frames)",
            data.len(),
            frame_bytes * frames,
            frames
        )));
    }
    let mut planar = vec![0u8; data.len()];
    let plane_len = frame_pixels * bps * frames;
    let (r_plane, rest) = planar.split_at_mut(plane_len);
    let (g_plane, b_plane) = rest.split_at_mut(plane_len);

    for f in 0..frames {
        let src = &data[f * frame_bytes..(f + 1) * frame_bytes];
        for i in 0..frame_pixels {
            let dst = f * frame_pixels * bps + i * bps;
            r_plane[dst..dst + bps].copy_from_slice(&src[i * 3 * bps..i * 3 * bps + bps]);
            g_plane[dst..dst + bps].copy_from_slice(&src[i * 3 * bps + bps..i * 3 * bps + 2 * bps]);
            b_plane[dst..dst + bps]
                .copy_from_slice(&src[i * 3 * bps + 2 * bps..i * 3 * bps + 3 * bps]);
        }
    }

    Ok(planar)
}

/// Convert planar pixel data (RRR...GGG...BBB..., per plane frame-major)
/// to interleaved RGB (per frame).
fn planar_to_interleaved(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Result<Vec<u8>, CodecError> {
    if bps == 0 || bps > 2 {
        return Err(CodecError::EncodeFailed(format!(
            "unsupported bytes-per-sample {bps}"
        )));
    }
    let frame_pixels = rows * cols;
    let plane_len = frame_pixels * bps * frames;
    if data.len() != plane_len * 3 {
        return Err(CodecError::EncodeFailed(format!(
            "planar size mismatch: got {}, expected {} ({} frames)",
            data.len(),
            plane_len * 3,
            frames
        )));
    }
    let mut interleaved = vec![0u8; data.len()];
    let (r_plane, rest) = data.split_at(plane_len);
    let (g_plane, b_plane) = rest.split_at(plane_len);

    for f in 0..frames {
        let dst = &mut interleaved[f * frame_pixels * 3 * bps..(f + 1) * frame_pixels * 3 * bps];
        for i in 0..frame_pixels {
            let src = f * frame_pixels * bps + i * bps;
            dst[i * 3 * bps..i * 3 * bps + bps].copy_from_slice(&r_plane[src..src + bps]);
            dst[i * 3 * bps + bps..i * 3 * bps + 2 * bps].copy_from_slice(&g_plane[src..src + bps]);
            dst[i * 3 * bps + 2 * bps..i * 3 * bps + 3 * bps]
                .copy_from_slice(&b_plane[src..src + bps]);
        }
    }

    Ok(interleaved)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interleaved_to_planar_roundtrip() {
        let interleaved = vec![
            10, 20, 30, // Pixel 0: R=10, G=20, B=30
            40, 50, 60, // Pixel 1: R=40, G=50, B=60
            70, 80, 90, // Pixel 2: R=70, G=80, B=90
        ];
        let planar = interleaved_to_planar(&interleaved, 1, 3, 1, 1).unwrap();
        assert_eq!(planar[0], 10); // R plane
        assert_eq!(planar[1], 40);
        assert_eq!(planar[2], 70);
        assert_eq!(planar[3], 20); // G plane
        assert_eq!(planar[4], 50);
        assert_eq!(planar[5], 80);
        assert_eq!(planar[6], 30); // B plane
        assert_eq!(planar[7], 60);
        assert_eq!(planar[8], 90);

        let back = planar_to_interleaved(&planar, 1, 3, 1, 1).unwrap();
        assert_eq!(back, interleaved);
    }

    #[test]
    fn test_interleaved_to_planar_multiframe_16bit() {
        // 2 frames of 2x1 pixels, 16-bit interleaved RGB.
        // Frame 0: (1,2,3) (4,5,6); Frame 1: (7,8,9) (10,11,12).
        let mut interleaved = Vec::new();
        for v in [1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] {
            interleaved.extend_from_slice(&v.to_le_bytes());
        }
        let planar = interleaved_to_planar(&interleaved, 1, 2, 2, 2).unwrap();
        // R plane: frame0 [1,4], frame1 [7,10] as u16 LE
        let r: Vec<u16> = planar[0..8]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(r, vec![1, 4, 7, 10]);
        let g: Vec<u16> = planar[8..16]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(g, vec![2, 5, 8, 11]);
        let b: Vec<u16> = planar[16..24]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(b, vec![3, 6, 9, 12]);

        let back = planar_to_interleaved(&planar, 1, 2, 2, 2).unwrap();
        assert_eq!(back, interleaved);
    }

    #[test]
    fn test_identity_codec_transfer_syntax() {
        let codec = ExplicitVrLeCodec;
        assert_eq!(codec.transfer_syntax_uid(), ts_uid::EXPLICIT_VR_LE);
    }
}
