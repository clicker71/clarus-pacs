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
        let planar = to_whole_image_planes(
            pixels,
            rows as usize,
            columns as usize,
            frames as usize,
            samples_per_pixel as usize,
            planar_configuration,
            bits_allocated as usize,
        )?;
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
        // Convert back from whole-image channel planes to the wire layout, then
        // byte-swap LE -> BE.
        //
        // The wire layout follows the PlanarConfiguration the metadata
        // declares, because PS3.3 C.7.6.3 makes that attribute describe the
        // bytes it accompanies. The decode side reads the same two cases in
        // reverse, so the pair is one contract: changing one without the other
        // makes a round trip through this codec swap frames between channels.
        let mut data = if pixels.samples_per_pixel == 3 {
            let frame_major = whole_image_planes_to_frame_major(
                &pixels.data,
                pixels.rows as usize,
                pixels.columns as usize,
                meta.frames.max(1) as usize,
                (pixels.bits_allocated as usize) >> 3,
            )?;
            match meta.planar_configuration {
                // Already the frame-major planes the wire wants.
                Some(1) => frame_major,
                // Interleaved wire: de-plane each frame into RGB triples.
                Some(0) | None => frame_major_planes_to_interleaved(
                    &frame_major,
                    pixels.rows as usize,
                    pixels.columns as usize,
                    meta.frames.max(1) as usize,
                    (pixels.bits_allocated as usize) >> 3,
                )?,
                Some(other) => {
                    return Err(CodecError::InvalidMetadata(format!(
                        "PlanarConfiguration must be 0 or 1, got {other}"
                    )));
                }
            }
        } else {
            pixels.data.clone()
        };

        if pixels.bits_allocated == 16 {
            swap_bytes_16(&mut data);
        }
        // For 8-bit: bytes remain as-is.

        Ok(data)
    }

    fn encode_pixeldata(
        &self,
        pixels: &DecodedPixels,
        meta: &DicomMeta,
    ) -> Result<Vec<u8>, CodecError> {
        // PS3.5 7.1.3: on Explicit VR Big Endian the TAG and the Value Length
        // are big endian like the value they describe. The inherited default
        // emits the little-endian element, so a 16-bit sample that `encode`
        // had just swapped to big endian sat behind a header saying the
        // opposite: a reader following the length took 0x0001 for 0x0100 and
        // lost every following element.
        let raw = self.encode(pixels, meta)?;
        let pad = usize::from(!raw.len().is_multiple_of(2));
        // PS3.5 6.2: an 8-bit sample is a byte stream, so its VR is OB, and OB
        // is byte-order free (PS3.5 7.3) - the big-endian header of this
        // transfer syntax applies to the tag and length, not to the value.
        // Writing OW for 8-bit data told a strict reader the value is a string
        // of 16-bit words and it read the samples in swapped pairs: a 2x1
        // image written `OB`-worth of bytes behind an `OW` header came back as
        // [2,1]. 16-bit samples keep OW, which is the word-oriented form.
        let vr: &[u8; 2] = if pixels.bits_allocated > 8 {
            b"OW"
        } else {
            b"OB"
        };
        let declared = raw
            .len()
            .checked_add(pad)
            .ok_or_else(|| CodecError::EncodeFailed("PixelData length overflows usize".into()))?;
        // The Value Length field is 32 bits, so anything at or above 4 GiB
        // cannot be described by this header. The cast used to wrap: a 4 GiB
        // value declared a length of 0 and the reader took the samples for the
        // next element's header.
        let declared = u32::try_from(declared).map_err(|_| {
            CodecError::EncodeFailed(format!(
                "PixelData of {} bytes does not fit the 32-bit Value Length field",
                raw.len() + pad
            ))
        })?;
        let mut out = Vec::with_capacity(12 + raw.len() + pad);
        out.extend_from_slice(&super::TAG_PIXEL_DATA.0.to_be_bytes());
        out.extend_from_slice(&super::TAG_PIXEL_DATA.1.to_be_bytes());
        out.extend_from_slice(vr);
        out.extend_from_slice(&[0x00, 0x00]); // reserved
        out.extend_from_slice(&declared.to_be_bytes());
        out.extend_from_slice(&raw);
        if pad == 1 {
            out.push(0x00);
        }
        Ok(out)
    }
}

// ===========================================================================
// Byte-swap helpers
// ===========================================================================

/// Swap every pair of bytes in-place (LE <-> BE for 16-bit data).
///
/// For BitsAllocated=8: data is byte-order independent (OB).
/// For BitsAllocated=16: each 16-bit sample needs byte-swap.
///
/// Public so the frames route can re-label a stored Big Endian instance's
/// pixel part as Explicit VR LE: the part carries the samples and no element
/// header, so the swap is the whole conversion and a second implementation of
/// it would be one more place to get the direction wrong.
pub fn swap_bytes_16(data: &mut [u8]) {
    for chunk in data.as_chunks_mut::<2>().0 {
        chunk.swap(0, 1);
    }
}

// ===========================================================================
// Pixel layout conversion helpers (same as explicit_vr_le.rs)
// ===========================================================================

/// Convert a wire-order BE pixel value into the `DecodedPixels` layout.
///
/// `DecodedPixels` is WHOLE-IMAGE channel planes (`R0 R1 G0 G1 B0 B1`), the
/// layout the Explicit VR LE codec produces and what every encoder in this
/// module reads back. Both wire cases have to land on it:
///
/// * PlanarConfiguration 0 puts interleaved samples in frame-major order
///   (PS3.5 8.1: the frames of a native multi-frame value are concatenated and
///   not padded), so the value needs de-interleaving ACROSS frames;
/// * PlanarConfiguration 1 already carries frame-major planes, so only the
///   frame/channel regroup is left.
///
/// Handing the interleaved case straight to a per-frame helper left the result
/// in frame-major planes, so frame 1's samples landed inside frame 0's planes.
fn to_whole_image_planes(
    pixels: Vec<u8>,
    rows: usize,
    cols: usize,
    frames: usize,
    samples_per_pixel: usize,
    planar_configuration: u16,
    bits_allocated: usize,
) -> Result<Vec<u8>, CodecError> {
    if samples_per_pixel != 3 {
        return Ok(pixels);
    }
    let bps = bits_allocated >> 3;
    let frame_major = if planar_configuration == 1 {
        pixels
    } else {
        interleaved_frames_to_frame_major_planes(&pixels, rows, cols, frames, bps)?
    };
    reframe_major_to_whole_image_planes(&frame_major, rows, cols, frames, bps)
}

/// Convert frame-major INTERLEAVED samples into frame-major PLANES.
///
/// The wire form of a PlanarConfiguration 0 value with N frames is
/// `R0 G0 B0 | R1 G1 B1 | ...` and PS3.5 8.1 says the frames are concatenated
/// without padding, so the samples of one frame stay together. This turns each
/// frame into that frame's three planes, leaving the frames in order - which is
/// the shape [`reframe_major_to_whole_image_planes`] then regroups.
///
/// `bps` is rejected when zero BEFORE any arithmetic: with a zero stride every
/// slice below is empty, so the loop would copy nothing and return a
/// full-length buffer of zeros that looks like a successful decode.
fn interleaved_frames_to_frame_major_planes(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Result<Vec<u8>, CodecError> {
    if bps == 0 {
        return Err(CodecError::InvalidMetadata(
            "Big Endian colour conversion: zero bytes per sample".into(),
        ));
    }
    let frame_pixels = rows * cols;
    let plane_bytes = frame_pixels
        .checked_mul(bps)
        .ok_or_else(|| CodecError::InvalidMetadata("frame size overflows usize".into()))?;
    let frame_bytes = plane_bytes
        .checked_mul(3)
        .ok_or_else(|| CodecError::InvalidMetadata("frame size overflows usize".into()))?;
    let expected = frame_bytes
        .checked_mul(frames)
        .ok_or_else(|| CodecError::InvalidMetadata("image size overflows usize".into()))?;
    if data.len() != expected {
        return Err(CodecError::DecodeFailed(format!(
            "Big Endian interleaved input: {} bytes for {} frame(s) of {}x{}x3x{}",
            data.len(),
            frames,
            rows,
            cols,
            bps
        )));
    }
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
    Ok(planar)
}

/// The inverse of [`interleaved_frames_to_frame_major_planes`]: frame-major
/// planes back to frame-major interleaved samples.
fn frame_major_planes_to_interleaved(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Result<Vec<u8>, CodecError> {
    if bps == 0 {
        return Err(CodecError::InvalidMetadata(
            "Big Endian colour conversion: zero bytes per sample".into(),
        ));
    }
    let frame_pixels = rows * cols;
    let plane_bytes = frame_pixels
        .checked_mul(bps)
        .ok_or_else(|| CodecError::InvalidMetadata("frame size overflows usize".into()))?;
    let frame_bytes = plane_bytes
        .checked_mul(3)
        .ok_or_else(|| CodecError::InvalidMetadata("frame size overflows usize".into()))?;
    let expected = frame_bytes
        .checked_mul(frames)
        .ok_or_else(|| CodecError::InvalidMetadata("image size overflows usize".into()))?;
    if data.len() != expected {
        return Err(CodecError::EncodeFailed(format!(
            "Big Endian planar input: {} bytes for {} frame(s) of {}x{}x3x{}",
            data.len(),
            frames,
            rows,
            cols,
            bps
        )));
    }
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
    Ok(interleaved)
}

/// Convert whole-image channel planes into frame-major planes.
///
/// `DecodedPixels` is `R0 R1 G0 G1 B0 B1` (each CHANNEL's frames together)
/// while the wire wants `R0 G0 B0 | R1 G1 B1` (each FRAME's planes together,
/// PS3.5 8.1). A single frame reads the same in both, which is why the identity
/// shortcut is correct and why the mismatch only showed on multi-frame colour.
fn whole_image_planes_to_frame_major(
    data: &[u8],
    rows: usize,
    cols: usize,
    frames: usize,
    bps: usize,
) -> Result<Vec<u8>, CodecError> {
    if bps == 0 {
        return Err(CodecError::InvalidMetadata(
            "Big Endian colour conversion: zero bytes per sample".into(),
        ));
    }
    let frame_pixels = rows * cols;
    let plane_bytes = frame_pixels
        .checked_mul(bps)
        .ok_or_else(|| CodecError::InvalidMetadata("frame size overflows usize".into()))?;
    let whole_plane = plane_bytes
        .checked_mul(frames)
        .ok_or_else(|| CodecError::InvalidMetadata("image size overflows usize".into()))?;
    let expected = whole_plane
        .checked_mul(3)
        .ok_or_else(|| CodecError::InvalidMetadata("image size overflows usize".into()))?;
    if data.len() != expected {
        return Err(CodecError::EncodeFailed(format!(
            "Big Endian planar input: {} bytes for {} frame(s) of {}x{}x3x{}",
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
    let frame_bytes = plane_bytes * 3;
    let mut out = vec![0u8; data.len()];
    for f in 0..frames {
        for channel in 0..3 {
            let src = channel * whole_plane + f * plane_bytes;
            let dst = f * frame_bytes + channel * plane_bytes;
            out[dst..dst + plane_bytes].copy_from_slice(&data[src..src + plane_bytes]);
        }
    }
    Ok(out)
}

/// Convert a wire-order planar colour value into whole-image channel planes.
///
/// The two layouts differ in what is contiguous:
///
/// * on the wire (PS3.5 8.1, frame-major): `R0 G0 B0 | R1 G1 B1` - each
///   frame's three planes are together;
/// * in [`DecodedPixels`]: `R0 R1 G0 G1 B0 B1` - each CHANNEL's frames are
///   together.
///
/// A single frame is already correct in both readings, which is why the
/// mismatch only showed on multi-frame colour and why the identity shortcut
/// here is safe.
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
        .ok_or_else(|| CodecError::InvalidMetadata("frame size overflows usize".into()))?;
    let frame_bytes = plane_bytes
        .checked_mul(3)
        .ok_or_else(|| CodecError::InvalidMetadata("frame size overflows usize".into()))?;
    let expected = frame_bytes
        .checked_mul(frames)
        .ok_or_else(|| CodecError::InvalidMetadata("image size overflows usize".into()))?;
    if data.len() != expected {
        return Err(CodecError::DecodeFailed(format!(
            "Big Endian planar input: {} bytes for {} frame(s) of {}x{}x3x{}",
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
    let whole_plane = plane_bytes * frames;
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

        let planar = interleaved_frames_to_frame_major_planes(&interleaved, 1, 2, 2, 1)
            .expect("two whole frames");
        assert_eq!(
            planar,
            vec![1u8, 4, 2, 5, 3, 6, 7, 10, 8, 11, 9, 12],
            "each frame keeps its own three planes"
        );
        assert_eq!(
            frame_major_planes_to_interleaved(&planar, 1, 2, 2, 1).expect("valid"),
            interleaved,
            "the conversion is reversible"
        );
    }

    /// The decode path must hand back WHOLE-IMAGE channel planes, which is the
    /// `DecodedPixels` contract and what the Explicit VR LE codec produces.
    ///
    /// Frame-major planes are an intermediate, not the answer: for two
    /// one-pixel frames the wire RGB is `R0 G0 B0 | R1 G1 B1` = 1,2,3,4,5,6 and
    /// the contract wants `R0 R1 G0 G1 B0 B1` = 1,4,2,5,3,6. Returning the
    /// frame-major regroup put frame 1's samples inside frame 0's planes.
    #[test]
    fn a_multi_frame_colour_decode_ends_as_whole_image_planes() {
        let wire: Vec<u8> = (1..=6).collect();
        let frame_major =
            interleaved_frames_to_frame_major_planes(&wire, 1, 1, 2, 1).expect("two frames");
        assert_eq!(frame_major, vec![1u8, 2, 3, 4, 5, 6], "frame-major planes");
        let whole =
            reframe_major_to_whole_image_planes(&frame_major, 1, 1, 2, 1).expect("regrouped");
        assert_eq!(
            whole,
            vec![1u8, 4, 2, 5, 3, 6],
            "whole-image channel planes"
        );
        // And the encode path walks it back to the wire bytes.
        let back = whole_image_planes_to_frame_major(&whole, 1, 1, 2, 1).expect("regrouped");
        let wire_again = frame_major_planes_to_interleaved(&back, 1, 1, 2, 1).expect("de-planed");
        assert_eq!(wire_again, wire, "the pair round trips");
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
        let planar = interleaved_frames_to_frame_major_planes(&interleaved, 1, 2, 1, 2)
            .expect("one whole frame");
        assert_eq!(
            planar,
            vec![1u8, 2, 7, 8, 3, 4, 9, 10, 5, 6, 11, 12],
            "each plane holds whole 2-byte samples"
        );
        assert_eq!(
            frame_major_planes_to_interleaved(&planar, 1, 2, 1, 2).expect("valid"),
            interleaved,
            "the conversion is reversible"
        );
    }

    // ---- A2b: Big Endian header and planar layout ----------------------

    fn be_meta() -> DicomMeta {
        DicomMeta {
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            bits_stored: 16,
            photometric_interpretation: "MONOCHROME2".to_string(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        }
    }

    #[test]
    fn the_pixeldata_header_is_big_endian_for_a_big_endian_target() {
        // PS3.5 7.1.3: on Explicit VR Big Endian the tag and the Value Length
        // follow the byte order. The inherited default wrote them little
        // endian, so a reader following the length read 0x0100 for 0x0001.
        let codec = BigEndianCodec;
        let pixels = DecodedPixels {
            data: vec![0x01, 0x02, 0x03, 0x04],
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME2".to_string(),
        };
        let element = codec
            .encode_pixeldata(&pixels, &be_meta())
            .expect("encoded");
        assert_eq!(&element[0..2], &[0x7F, 0xE0], "big endian group");
        assert_eq!(&element[2..4], &[0x00, 0x10], "big endian element");
        assert_eq!(&element[4..6], b"OW", "VR letters are order-free");
        assert_eq!(
            &element[8..12],
            &4u32.to_be_bytes(),
            "the length is big endian"
        );
    }

    /// PS3.5 6.2 leaves the choice between OB and OW to the transfer syntax and
    /// the sample width. An 8-bit sample is a byte stream, so its VR is OB; only
    /// a 16-bit sample is word-oriented and takes OW.
    ///
    /// Writing OW for 8-bit data told a strict reader the value is a string of
    /// 16-bit words, so it read the samples in swapped pairs: a 2x1 image came
    /// back as [2,1] instead of [1,2].
    #[test]
    fn eight_bit_pixel_data_is_written_as_ob_and_sixteen_bit_as_ow() {
        let codec = BigEndianCodec;
        let mut meta = be_meta();
        meta.rows = 1;
        meta.columns = 2;
        meta.bits_allocated = 8;
        meta.bits_stored = 8;
        let pixels = DecodedPixels {
            data: vec![1u8, 2],
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".to_string(),
        };
        let element = codec
            .encode_pixeldata(&pixels, &meta)
            .expect("8-bit encoded");
        assert_eq!(
            &element[4..6],
            b"OB",
            "an 8-bit sample is an octet stream, so the VR is OB"
        );

        // The 16-bit case keeps OW.
        let wide = DecodedPixels {
            data: vec![0x01, 0x02, 0x03, 0x04],
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME2".to_string(),
        };
        let element = codec
            .encode_pixeldata(&wide, &be_meta())
            .expect("16-bit encoded");
        assert_eq!(&element[4..6], b"OW", "a 16-bit sample keeps OW");
    }

    #[test]
    fn a_multiframe_planar_source_becomes_whole_image_planes() {
        // PS3.5 8.1: the wire order of a planar value is frame-major, while
        // DecodedPixels is whole-image plane-major. Two frames of one pixel,
        // distinct per frame so a wrong reading is visible.
        // Frame 0: R=1 G=2 B=3. Frame 1: R=4 G=5 B=6.
        let wire = vec![1u8, 2, 3, 4, 5, 6];
        let out = reframe_major_to_whole_image_planes(&wire, 1, 1, 2, 1).expect("two whole frames");
        assert_eq!(out, vec![1, 4, 2, 5, 3, 6], "channel planes across frames");
    }

    /// The decode path must end on whole-image channel planes.
    ///
    /// Driving `reframe_major_to_whole_image_planes` directly does not prove
    /// which branch the codec takes: the interleaved case was routed to a
    /// per-frame helper that returns frame-major planes, so two one-pixel
    /// frames decoded to `[1,2,3,4,5,6]` where the contract is
    /// `[1,4,2,5,3,6]`. This calls the same `to_whole_image_planes` the codec
    /// calls, so the wiring is what is asserted.
    ///
    /// The dataset cannot be driven through `decode` here: the shared
    /// `DicomRsParser` walks elements Little Endian, so it cannot locate tags
    /// in a Big Endian instance at all. That is a decode-path gap of its own
    /// and not something this batch changes.
    #[test]
    fn interleaved_multiframe_colour_decodes_to_whole_image_planes() {
        // Wire: frame-major interleaved, one pixel per frame.
        // Frame 0 = R1 G2 B3, frame 1 = R4 G5 B6.
        let out = to_whole_image_planes(vec![1, 2, 3, 4, 5, 6], 1, 1, 2, 3, 0, 8).expect("decoded");
        assert_eq!(
            out,
            vec![1, 4, 2, 5, 3, 6],
            "whole-image channel planes: R0 R1 G0 G1 B0 B1"
        );
    }

    /// And PlanarConfiguration 1 reaches the same contract from frame-major
    /// planes, which are also the wire order for that value.
    #[test]
    fn planar_multiframe_colour_decodes_to_whole_image_planes() {
        let out = to_whole_image_planes(vec![1, 2, 3, 4, 5, 6], 1, 1, 2, 3, 1, 8).expect("decoded");
        assert_eq!(
            out,
            vec![1, 4, 2, 5, 3, 6],
            "the planar branch lands on the same layout"
        );
    }

    #[test]
    fn a_single_frame_planar_value_is_left_alone() {
        // One frame reads the same in both layouts, so the conversion is a
        // copy rather than a reordering.
        let wire = vec![1u8, 2, 3];
        let out = reframe_major_to_whole_image_planes(&wire, 1, 1, 1, 1).expect("one frame");
        assert_eq!(out, wire);
    }

    #[test]
    fn a_planar_buffer_that_does_not_hold_whole_frames_is_refused() {
        // The slicing indexes `f * frame_bytes .. (f + 1) * frame_bytes`, so a
        // short buffer used to panic the worker on a caller-supplied value.
        let six = vec![1u8, 2, 3, 4, 5, 6]; // one RGB frame
        let err = reframe_major_to_whole_image_planes(&six, 1, 2, 2, 1)
            .expect_err("two frames need twelve bytes");
        assert!(err.to_string().contains("for 2 frame(s)"), "got: {err}");

        let err = interleaved_frames_to_frame_major_planes(&six, 1, 2, 2, 1)
            .expect_err("and the interleaved direction refuses it too");
        assert!(err.to_string().contains("for 2 frame(s)"), "got: {err}");
    }

    /// A zero sample stride used to pass every guard: the `checked_mul` set
    /// only catches overflow, and `0 * anything` does not overflow. Every slice
    /// in the layout loops was then empty, so the helper returned a
    /// full-length buffer of zeros that read as a successful decode.
    #[test]
    fn a_zero_sample_stride_is_refused_before_the_layout_loops() {
        let data = vec![0u8; 6];
        for err in [
            interleaved_frames_to_frame_major_planes(&data, 1, 2, 1, 0)
                .expect_err("decode direction"),
            whole_image_planes_to_frame_major(&data, 1, 2, 1, 0).expect_err("encode direction"),
            frame_major_planes_to_interleaved(&data, 1, 2, 1, 0).expect_err("de-plane direction"),
        ] {
            assert!(
                err.to_string().contains("zero bytes per sample"),
                "got: {err}"
            );
        }
    }
}
