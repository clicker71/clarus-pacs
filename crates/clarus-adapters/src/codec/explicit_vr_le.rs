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

        // PS3.5 8.1.1: Bits Allocated is 1 or a multiple of 8, and a 1-bit
        // sample is PACKED - a frame may start mid-byte
        // (`standard/ps3.5/bit-packed-pixel-data-8.1.json`). This codec has no
        // packed form, and `bits_allocated >> 3` turned 1 into a stride of 0,
        // so every 1-bit image declared an expected size of zero and passed the
        // length check with no bytes at all. Refuse it, matching the encoder
        // contract in `DecodedPixels::validate_frames`, instead of reading a
        // zero-byte image as if it were complete.
        let bytes_per_sample = (bits_allocated as usize) >> 3;
        if bytes_per_sample == 0 {
            return Err(CodecError::InvalidMetadata(format!(
                "Explicit VR LE: BitsAllocated {bits_allocated} has no unpacked byte form; \
                 bit-packed (BitsAllocated=1) pixel data is not supported"
            )));
        }

        let expected_size = rows as usize
            * columns as usize
            * samples_per_pixel as usize
            * bytes_per_sample
            * frames as usize;

        if pixel_data.len() != expected_size {
            // PS3.5 8.1: the Value Field is padded to an even length and the
            // pad "shall not be considered significant" - it is not part of the
            // image. A three-sample 1x1 image is 3 bytes, so a conformant file
            // carries 4 and the last is padding. Accepting only the exact
            // expected size refused that file.
            let is_even_pad = pixel_data.len() == expected_size + 1
                && !expected_size.is_multiple_of(2)
                && pixel_data.last() == Some(&0);
            if !is_even_pad {
                return Err(CodecError::DecodeFailed(format!(
                    "PixelData size mismatch: got {} bytes, expected {} ({}x{}x{}x{}x{} frames)",
                    pixel_data.len(),
                    expected_size,
                    rows,
                    columns,
                    samples_per_pixel,
                    bytes_per_sample,
                    frames,
                )));
            }
        }
        // Drop the insignificant pad so the layout helpers see samples only.
        let pixel_data: &[u8] = &pixel_data[..expected_size];

        // -- Planar conversion -------------------------------------
        let planar = if samples_per_pixel == 3 {
            if planar_configuration == 1 {
                // PS3.5 8.1: a multi-frame planar value on the wire is
                // FRAME-major (`R0 G0 B0 | R1 G1 B1`), while `DecodedPixels` is
                // whole-image plane-major (`R0 R1 G0 G1 B0 B1`). Returning the
                // wire bytes unchanged made `planar_to_interleaved` read all
                // frames of R before G, so every frame after the first had its
                // channels assigned to the wrong plane. The single-frame case is
                // identical in both layouts and is returned as-is.
                reframe_major_to_whole_image_planes(
                    pixel_data,
                    rows as usize,
                    columns as usize,
                    frames as usize,
                    (bits_allocated as usize) >> 3,
                )?
            } else {
                interleaved_to_planar(
                    pixel_data,
                    rows as usize,
                    columns as usize,
                    frames as usize,
                    (bits_allocated as usize) >> 3,
                )?
            }
        } else {
            pixel_data.to_vec()
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
        if pixels.samples_per_pixel != 3 {
            return Ok(pixels.data.clone());
        }
        // PS3.3 C.7.6.3 (0028,0006): the VALUE of PlanarConfiguration decides
        // how the bytes are laid out, and the metadata written alongside them
        // states it. Emitting interleaved samples under a declared 1 was a file
        // whose own attributes contradicted its pixels, so a conformant reader
        // read R, G and B from each other's planes.
        match meta.planar_configuration {
            Some(1) => Ok(whole_image_planes_to_frame_major(
                &pixels.data,
                pixels.rows as usize,
                pixels.columns as usize,
                meta.frames.max(1) as usize,
                (pixels.bits_allocated as usize) >> 3,
            )?),
            Some(0) | None => Ok(planar_to_interleaved(
                &pixels.data,
                pixels.rows as usize,
                pixels.columns as usize,
                meta.frames as usize,
                (pixels.bits_allocated as usize) >> 3,
            )?),
            Some(other) => Err(CodecError::InvalidMetadata(format!(
                "PlanarConfiguration must be 0 or 1, got {other}"
            ))),
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
    // Checked: `rows*cols*3*bps` wraps on a release build for a large image,
    // and a wrapped length that happens to equal the buffer size lets an
    // undersized buffer through to the slicing below.
    let frame_bytes = frame_pixels
        .checked_mul(3)
        .and_then(|v| v.checked_mul(bps))
        .ok_or_else(|| CodecError::DecodeFailed("frame size overflows usize".into()))?;
    let expected = frame_bytes
        .checked_mul(frames)
        .ok_or_else(|| CodecError::DecodeFailed("image size overflows usize".into()))?;
    if data.len() != expected {
        return Err(CodecError::DecodeFailed(format!(
            "interleaved size mismatch: got {}, expected {} ({} frames)",
            data.len(),
            expected,
            frames
        )));
    }
    let mut planar = vec![0u8; data.len()];
    let plane_len = frame_pixels
        .checked_mul(bps)
        .and_then(|v| v.checked_mul(frames))
        .ok_or_else(|| CodecError::DecodeFailed("plane size overflows usize".into()))?;
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
    // Checked for the same reason as the decode direction: the three factors
    // below are multiplied before any length is compared.
    let plane_len = frame_pixels
        .checked_mul(bps)
        .and_then(|v| v.checked_mul(frames))
        .ok_or_else(|| CodecError::EncodeFailed("plane size overflows usize".into()))?;
    let expected = plane_len
        .checked_mul(3)
        .ok_or_else(|| CodecError::EncodeFailed("image size overflows usize".into()))?;
    if data.len() != expected {
        return Err(CodecError::EncodeFailed(format!(
            "planar size mismatch: got {}, expected {} ({} frames)",
            data.len(),
            expected,
            frames
        )));
    }
    let mut interleaved = vec![0u8; data.len()];
    let (r_plane, rest) = data.split_at(plane_len);
    let (g_plane, b_plane) = rest.split_at(plane_len);

    let frame_rgb_bytes = frame_pixels
        .checked_mul(3)
        .and_then(|v| v.checked_mul(bps))
        .ok_or_else(|| CodecError::EncodeFailed("frame size overflows usize".into()))?;
    for f in 0..frames {
        let dst = &mut interleaved[f * frame_rgb_bytes..(f + 1) * frame_rgb_bytes];
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

/// Convert whole-image channel planes into the wire order of a planar value.
///
/// The inverse of [`reframe_major_to_whole_image_planes`]: `R0 R1 G0 G1 B0 B1`
/// becomes `R0 G0 B0 | R1 G1 B1`, which is what PS3.5 8.1 lays out for a
/// multi-frame PlanarConfiguration = 1 value.
fn whole_image_planes_to_frame_major(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Result<Vec<u8>, CodecError> {
    let frame_pixels = rows * cols;
    let plane_bytes = frame_pixels
        .checked_mul(bps)
        .ok_or_else(|| CodecError::EncodeFailed("frame size overflows usize".into()))?;
    let whole_plane = plane_bytes
        .checked_mul(frames)
        .ok_or_else(|| CodecError::EncodeFailed("image size overflows usize".into()))?;
    let expected = whole_plane
        .checked_mul(3)
        .ok_or_else(|| CodecError::EncodeFailed("image size overflows usize".into()))?;
    if data.len() != expected {
        return Err(CodecError::EncodeFailed(format!(
            "planar input: {} bytes for {} frame(s) of {}x{}x3x{}",
            data.len(),
            frames,
            rows,
            cols,
            bps
        )));
    }
    if frames == 1 {
        return Ok(data.to_vec());
    }
    let mut out = vec![0u8; data.len()];
    let frame_bytes = plane_bytes * 3;
    for f in 0..frames {
        for channel in 0..3 {
            let src = channel * whole_plane + f * plane_bytes;
            let dst = f * frame_bytes + channel * plane_bytes;
            out[dst..dst + plane_bytes].copy_from_slice(&data[src..src + plane_bytes]);
        }
    }
    Ok(out)
}

/// Convert a frame-major planar value into whole-image channel planes.
///
/// The read-side twin of the function above; see its documentation for why the
/// two layouts differ and why one frame needs neither.
fn reframe_major_to_whole_image_planes(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Result<Vec<u8>, CodecError> {
    let frame_pixels = rows * cols;
    let plane_bytes = frame_pixels
        .checked_mul(bps)
        .ok_or_else(|| CodecError::DecodeFailed("frame size overflows usize".into()))?;
    let frame_bytes = plane_bytes
        .checked_mul(3)
        .ok_or_else(|| CodecError::DecodeFailed("frame size overflows usize".into()))?;
    let expected = frame_bytes
        .checked_mul(frames)
        .ok_or_else(|| CodecError::DecodeFailed("image size overflows usize".into()))?;
    if data.len() != expected {
        return Err(CodecError::DecodeFailed(format!(
            "planar input: {} bytes for {} frame(s) of {}x{}x3x{}",
            data.len(),
            frames,
            rows,
            cols,
            bps
        )));
    }
    if frames == 1 {
        return Ok(data.to_vec());
    }
    let whole_plane = plane_bytes * frames;
    let mut out = vec![0u8; data.len()];
    for f in 0..frames {
        let src_frame = f * frame_bytes;
        let dst_plane = f * plane_bytes;
        for channel in 0..3 {
            let src = src_frame + channel * plane_bytes;
            let dst = channel * whole_plane + dst_plane;
            out[dst..dst + plane_bytes].copy_from_slice(&data[src..src + plane_bytes]);
        }
    }
    Ok(out)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// PS3.5 8.1.1 allows BitsAllocated 1, where samples are PACKED and a frame
    /// may start mid-byte. This codec has no packed form, and `>> 3` gave a
    /// 1-bit image a stride of 0 - so it declared an expected size of zero and
    /// an empty PixelData passed the length check as complete.
    #[test]
    fn a_one_bit_image_is_refused_rather_than_sized_as_zero() {
        let codec = ExplicitVrLeCodec;
        // A minimal Explicit VR LE dataset: Rows, Columns, BitsAllocated,
        // SamplesPerPixel, then an EMPTY PixelData. The empty value is the
        // point: it was accepted when the expected size came out at zero.
        fn us(tag: (u16, u16), v: u16) -> Vec<u8> {
            let mut d = Vec::new();
            d.extend_from_slice(&tag.0.to_le_bytes());
            d.extend_from_slice(&tag.1.to_le_bytes());
            d.extend_from_slice(b"US");
            d.extend_from_slice(&2u16.to_le_bytes());
            d.extend_from_slice(&v.to_le_bytes());
            d
        }
        let mut d = Vec::new();
        d.extend_from_slice(&[0u8; 128]);
        d.extend_from_slice(b"DICM");
        d.extend_from_slice(&us(TAG_ROWS, 1));
        d.extend_from_slice(&us(TAG_COLUMNS, 1));
        d.extend_from_slice(&us(TAG_SAMPLES_PER_PIXEL, 1));
        d.extend_from_slice(&us(TAG_BITS_ALLOCATED, 1));
        // PixelData with a zero-length value.
        d.extend_from_slice(&TAG_PIXEL_DATA.0.to_le_bytes());
        d.extend_from_slice(&TAG_PIXEL_DATA.1.to_le_bytes());
        d.extend_from_slice(b"OW");
        d.extend_from_slice(&[0, 0]);
        d.extend_from_slice(&0u32.to_le_bytes());

        let err = codec.decode(&d).expect_err("a 1-bit image must be refused");
        assert!(
            err.to_string().contains("bit-packed"),
            "the reason must name the packed form, got: {err}"
        );
    }

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

    // ---- A2b: requested planar layout ----------------------------------

    fn rgb_pixels(data: Vec<u8>) -> DecodedPixels {
        DecodedPixels {
            data,
            rows: 1,
            columns: 1,
            samples_per_pixel: 3,
            bits_allocated: 8,
            photometric_interpretation: "RGB".to_string(),
        }
    }

    fn rgb_meta(planar: Option<u16>, frames: u32) -> DicomMeta {
        DicomMeta {
            rows: 1,
            columns: 1,
            samples_per_pixel: 3,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "RGB".to_string(),
            planar_configuration: planar,
            pixel_representation: 0,
            frames,
        }
    }

    #[test]
    fn a_requested_planar_layout_is_actually_written() {
        // PS3.3 C.7.6.3 (0028,0006): the VALUE decides the layout, and the
        // metadata written with the pixels states it. Emitting interleaved
        // samples under a declared 1 made the file contradict itself.
        //
        // Two frames of two pixels, so the two layouts differ: DecodedPixels
        // is whole-image plane-major, the wire is frame-major when planar.
        let codec = ExplicitVrLeCodec;
        let mut pixels = rgb_pixels(vec![
            10, 11, 12, 13, // R plane, both frames
            20, 21, 22, 23, // G plane
            30, 31, 32, 33, // B plane
        ]);
        pixels.rows = 1;
        pixels.columns = 2;
        let mut meta = rgb_meta(Some(0), 2);
        meta.rows = 1;
        meta.columns = 2;

        // Some(0): each pixel's three samples together, frame by frame.
        let interleaved = codec.encode(&pixels, &meta).expect("encoded");
        assert_eq!(
            interleaved,
            vec![10, 20, 30, 11, 21, 31, 12, 22, 32, 13, 23, 33],
            "Some(0) interleaves"
        );

        // Some(1): each frame's three planes together (PS3.5 8.1).
        meta.planar_configuration = Some(1);
        let planar = codec.encode(&pixels, &meta).expect("encoded");
        assert_eq!(
            planar,
            vec![10, 11, 20, 21, 30, 31, 12, 13, 22, 23, 32, 33],
            "Some(1) writes frame-major planes"
        );
    }

    #[test]
    fn a_planar_configuration_other_than_zero_or_one_is_refused() {
        // PS3.3 C.7.6.3 enumerates 0 and 1 only.
        let codec = ExplicitVrLeCodec;
        let err = codec
            .encode(&rgb_pixels(vec![1, 2, 3]), &rgb_meta(Some(2), 1))
            .expect_err("2 is not a PlanarConfiguration");
        assert!(err.to_string().contains("must be 0 or 1"), "got: {err}");
    }

    #[test]
    fn a_multiframe_planar_source_becomes_whole_image_planes_on_decode() {
        // The read side of the same rule: a wire planar value is frame-major,
        // DecodedPixels is whole-image plane-major.
        let wire = vec![1u8, 2, 3, 4, 5, 6]; // F0: R1 G2 B3, F1: R4 G5 B6
        let out = reframe_major_to_whole_image_planes(&wire, 1, 1, 2, 1).expect("two whole frames");
        assert_eq!(out, vec![1, 4, 2, 5, 3, 6]);
        // And the two conversions are inverses.
        let back = whole_image_planes_to_frame_major(&out, 1, 1, 2, 1).expect("two whole frames");
        assert_eq!(back, wire);
    }
}
