// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! JPEG Baseline codec - decode/encode via `jpeg-decoder` crate.
//!
//! Transfer Syntax: 1.2.840.10008.1.2.4.50 (JPEG Baseline, lossy 8-bit).
//!
//! # DICOM-specific considerations
//!
//! DICOM JPEG Baseline uses YBR_FULL_422 photometric interpretation.
//! After decoding, pixels are in RGB. Color space conversion is handled
//! by the transcode pipeline in [`crate::transcode`].
//!
//! # Limitations
//! - Only 8-bit images are supported (JPEG Baseline is inherently 8-bit)
//! - JPEG encode uses quality=95 by default
//! - JPEG Lossless and JPEG 2000 are NOT covered here; they live in the
//!   shared transcode pipeline instead

#![forbid(unsafe_code)]

use super::{CodecError, DecodedPixels, DicomCodec, DicomMeta, ts_uid};
use super::{
    TAG_BITS_ALLOCATED, TAG_COLUMNS, TAG_PHOTOMETRIC_INTERPRETATION, TAG_PIXEL_DATA,
    TAG_PIXEL_REPRESENTATION, TAG_ROWS, TAG_SAMPLES_PER_PIXEL,
};

/// Codec for JPEG Baseline (1.2.840.10008.1.2.4.50).
pub struct JpegBaselineCodec;

impl DicomCodec for JpegBaselineCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::JPEG_BASELINE
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        let meta = parse_jpeg_meta(dicom_bytes)?;

        // Encapsulated PixelData: one JPEG codestream per frame. Reuse the
        // shared fragment extractor (BOT detection incl. the filled 1-entry
        // case, 01.09 field).
        let fragments = super::jpegls::extract_encapsulated_fragments(dicom_bytes)
            .map_err(|e| CodecError::DecodeFailed(format!("JPEG pixel data not found: {e}")))?;
        if fragments.is_empty() {
            return Err(CodecError::DecodeFailed("JPEG pixel data is empty".into()));
        }

        let frames = meta.frames.max(1) as usize;
        // A single frame may legitimately arrive split across fragments
        // (PS3.5 A.4 lets an encoder break a codestream anywhere). JPEG
        // Baseline always interleaves its components into ONE codestream per
        // frame, so there is no per-component split to preserve here.
        let fragments = super::coalesce_single_frame_fragments(fragments, frames, 1);
        if fragments.len() != frames {
            return Err(CodecError::DecodeFailed(format!(
                "JPEG Baseline: fragment count {} != NumberOfFrames {}",
                fragments.len(),
                frames
            )));
        }

        let pixel_count = meta.rows as usize * meta.columns as usize;
        let mut samples = 0usize;
        // Per-frame planar buffers, then assembled per-plane frame-major
        // (all frames of R, then all frames of G... - codec convention).
        let mut frame_planes: Vec<Vec<u8>> = Vec::with_capacity(frames);

        for frag in &fragments {
            let mut decoder = jpeg_decoder::Decoder::new(&frag[..]);
            let frame_pixels = decoder
                .decode()
                .map_err(|e| CodecError::DecodeFailed(format!("JPEG decode error: {e}")))?;
            let info = decoder
                .info()
                .ok_or_else(|| CodecError::DecodeFailed("JPEG decoder info unavailable".into()))?;

            if info.height != meta.rows || info.width != meta.columns {
                return Err(CodecError::DecodeFailed(format!(
                    "JPEG dimensions ({},{}) don't match DICOM metadata ({},{})",
                    info.width, info.height, meta.columns, meta.rows
                )));
            }
            let s = if info.pixel_format == jpeg_decoder::PixelFormat::L8 {
                1
            } else {
                3
            };
            if samples == 0 {
                samples = s;
            } else if s != samples {
                return Err(CodecError::DecodeFailed(format!(
                    "JPEG Baseline: frame sample count changed {} -> {}",
                    samples, s
                )));
            }
            // jpeg-decoder outputs interleaved RGB for color.
            let planar = if s == 3 {
                interleaved_to_planar(&frame_pixels, meta.rows as usize, meta.columns as usize)
            } else {
                frame_pixels
            };
            frame_planes.push(planar);
        }

        let mut data = Vec::with_capacity(pixel_count * samples * frames);
        for channel in 0..samples {
            for plane in frame_planes.iter().take(frames) {
                data.extend_from_slice(&plane[channel * pixel_count..(channel + 1) * pixel_count]);
            }
        }

        // E2 fix (31.08): jpeg-decoder converts YCbCr->RGB internally, so a
        // 3-component decode IS RGB in R,G,B order. Reporting the source tag
        // (YBR_FULL_422) on RGB bytes would produce a corrupt file downstream
        // (the transcode pipeline picks the target PI from THIS value).
        let photometric_interpretation = if samples == 3 {
            "RGB".to_string()
        } else {
            meta.photometric_interpretation
        };

        Ok(DecodedPixels {
            data,
            rows: meta.rows,
            columns: meta.columns,
            samples_per_pixel: samples as u8,
            bits_allocated: 8,
            photometric_interpretation,
        })
    }

    fn encode(&self, pixels: &DecodedPixels, meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        if meta.frames > 1 {
            return Err(CodecError::EncodeFailed(
                "JPEG Baseline multi-frame encode is not supported".into(),
            ));
        }
        if pixels.bits_allocated != 8 {
            return Err(CodecError::EncodeFailed(format!(
                "JPEG Baseline requires 8-bit pixels, got {}-bit",
                pixels.bits_allocated
            )));
        }

        let width = pixels.columns;
        let height = pixels.rows;

        // Build interleaved pixel buffer for JPEG encoder
        let interleaved = if pixels.samples_per_pixel == 3 {
            planar_to_interleaved(&pixels.data, pixels.rows as usize, pixels.columns as usize)
        } else {
            pixels.data.clone()
        };

        let mut output = Vec::new();

        let encoder = jpeg_encoder::Encoder::new(&mut output, 95);
        encoder
            .encode(
                &interleaved,
                width,
                height,
                if pixels.samples_per_pixel == 3 {
                    jpeg_encoder::ColorType::Rgb
                } else {
                    jpeg_encoder::ColorType::Luma
                },
            )
            .map_err(|e| CodecError::EncodeFailed(format!("JPEG encode error: {e}")))?;

        Ok(output)
    }

    fn encode_pixeldata(
        &self,
        pixels: &DecodedPixels,
        meta: &DicomMeta,
    ) -> Result<Vec<u8>, CodecError> {
        let compressed = self.encode(pixels, meta)?;
        Ok(crate::codec::build_encapsulated_pixeldata(
            &compressed,
            false,
        ))
    }
}

// ===========================================================================
// DICOM tag parsing helpers (self-contained)
// ===========================================================================

fn parse_jpeg_meta(data: &[u8]) -> Result<DicomMeta, CodecError> {
    let rows = find_tag_u16(data, TAG_ROWS)
        .ok_or_else(|| CodecError::InvalidMetadata("Missing Rows (0028,0010)".into()))?;
    let columns = find_tag_u16(data, TAG_COLUMNS)
        .ok_or_else(|| CodecError::InvalidMetadata("Missing Columns (0028,0011)".into()))?;
    let samples_per_pixel: u8 = find_tag_u16(data, TAG_SAMPLES_PER_PIXEL)
        .map(|v| v as u8)
        .unwrap_or(1);
    let bits_allocated: u8 = find_tag_u16(data, TAG_BITS_ALLOCATED).unwrap_or(8) as u8;
    let bits_stored: u8 = find_tag_u16(data, TAG_BITS_ALLOCATED)
        .map(|v| v as u8)
        .unwrap_or(bits_allocated);
    let photometric_interpretation = find_tag_str(data, TAG_PHOTOMETRIC_INTERPRETATION)
        .unwrap_or_else(|| "MONOCHROME2".to_string());
    // NumberOfFrames (0028,0008) is IS - a decimal string (01.09 field).
    let frames: u32 = find_tag_str(data, (0x0028, 0x0008))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1);

    Ok(DicomMeta {
        rows,
        columns,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        photometric_interpretation,
        planar_configuration: None,
        pixel_representation: find_tag_u16(data, TAG_PIXEL_REPRESENTATION).unwrap_or(0),
        frames,
    })
}

fn extract_jpeg_pixel_data(data: &[u8]) -> Option<Vec<u8>> {
    find_tag_bytes(data, TAG_PIXEL_DATA)
}

fn find_tag_bytes(data: &[u8], target: (u16, u16)) -> Option<Vec<u8>> {
    let mut pos = dicom_start(data)?;
    // Skip meta-header
    while pos + 4 <= data.len() {
        if u16_le(data, pos) != 0x0002 {
            break;
        }
        pos += 4;
        if !skip_explicit(data, &mut pos) {
            return None;
        }
    }
    // Walk dataset
    while pos + 4 <= data.len() {
        let g = u16_le(data, pos);
        let e = u16_le(data, pos + 2);
        pos += 4;
        // Handle group FFFE: Item (FFFE,E000), Item Delimitation (FFFE,E00D),
        // Sequence Delimiter (FFFE,E0DD). These have no VR, just 4-byte length.
        if g == 0xFFFE {
            if pos + 4 > data.len() {
                return None;
            }
            let len = u32_from_slice(&data[pos..pos + 4]) as usize;
            pos += 4;
            if len == 0xFFFF_FFFF {
                // Undefined length - skip by scanning for next FFFE delimiter
                // (encapsulated pixel data uses undefined-length Items)
                continue;
            }
            if pos + len > data.len() {
                return None;
            }
            pos += len;
            continue;
        }
        if g == 0x0000 || g == 0x0004 {
            if !skip_implicit(data, &mut pos) {
                return None;
            }
            continue;
        }
        if (g, e) == target {
            return read_explicit_val(data, &mut pos);
        }
        if !skip_explicit(data, &mut pos) {
            return None;
        }
    }
    None
}

fn find_tag_u16(data: &[u8], tag: (u16, u16)) -> Option<u16> {
    let bytes = find_tag_bytes(data, tag)?;
    if bytes.len() >= 2 {
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    } else if bytes.len() == 1 {
        Some(bytes[0] as u16)
    } else {
        None
    }
}

fn find_tag_str(data: &[u8], tag: (u16, u16)) -> Option<String> {
    let bytes = find_tag_bytes(data, tag)?;
    let s = String::from_utf8_lossy(&bytes);
    let t = s.trim_end_matches([' ', '\0']);
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn dicom_start(data: &[u8]) -> Option<usize> {
    if data.len() >= 132 && &data[128..132] == b"DICM" {
        Some(132)
    } else if data.len() >= 4 && &data[0..4] == b"DICM" {
        Some(4)
    } else {
        None
    }
}

fn u16_le(data: &[u8], pos: usize) -> u16 {
    if pos + 2 > data.len() {
        0
    } else {
        u16::from_le_bytes([data[pos], data[pos + 1]])
    }
}

fn u32_from_slice(bytes: &[u8]) -> u32 {
    if bytes.len() < 4 {
        0
    } else {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}

fn read_explicit_val(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    if *pos + 4 > data.len() {
        return None;
    }
    let vr = std::str::from_utf8(&data[*pos..*pos + 2]).ok()?;
    *pos += 2;
    let extended = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
    );
    let len: usize = if extended {
        if *pos + 6 > data.len() {
            return None;
        }
        *pos += 2;
        u32_from_slice(&data[*pos..*pos + 4]) as usize
    } else {
        u16_le(data, *pos) as usize
    };
    *pos += if extended { 4 } else { 2 };

    if len == 0xFFFF_FFFF {
        // Encapsulated (undefined-length) PixelData.
        // Walk items: skip Basic Offset Table, extract first fragment.
        read_encapsulated_fragment(data, pos)
    } else {
        if *pos + len > data.len() {
            return None;
        }
        let v = data[*pos..*pos + len].to_vec();
        super::skip_value(pos, len);
        Some(v)
    }
}

/// Extract the first data fragment from an encapsulated PixelData sequence.
fn read_encapsulated_fragment(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    // Skip Basic Offset Table item: tag (FFFE,E000) + length (4 bytes)
    if *pos + 8 > data.len() {
        return None;
    }
    let tag = (u16_le(data, *pos), u16_le(data, *pos + 2));
    if tag != (0xFFFE, 0xE000) {
        return None;
    }
    let bot_len = u32_from_slice(&data[*pos + 4..*pos + 8]) as usize;
    *pos += 8;
    if bot_len > 0 {
        if *pos + bot_len > data.len() {
            return None;
        }
        *pos += bot_len;
    }
    // Read fragment item
    if *pos + 8 > data.len() {
        return None;
    }
    let frag_tag = (u16_le(data, *pos), u16_le(data, *pos + 2));
    if frag_tag != (0xFFFE, 0xE000) {
        return None;
    }
    let frag_len = u32_from_slice(&data[*pos + 4..*pos + 8]) as usize;
    *pos += 8;
    if frag_len == 0xFFFF_FFFF {
        return None;
    }
    if *pos + frag_len > data.len() {
        return None;
    }
    let fragment_data = data[*pos..*pos + frag_len].to_vec();
    *pos += frag_len;
    Some(fragment_data)
}

fn skip_explicit(data: &[u8], pos: &mut usize) -> bool {
    if *pos + 4 > data.len() {
        return false;
    }
    let vr = match std::str::from_utf8(&data[*pos..*pos + 2]) {
        Ok(v) => v,
        Err(_) => return false,
    };
    *pos += 2;
    let extended = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
    );
    let len: usize = if extended {
        if *pos + 6 > data.len() {
            return false;
        }
        *pos += 2;
        u32_from_slice(&data[*pos..*pos + 4]) as usize
    } else {
        u16_le(data, *pos) as usize
    };
    *pos += if extended { 4 } else { 2 };
    if len == 0xFFFF_FFFF {
        // Sequence (SQ/UN) with undefined length: skip items up to the
        // matching Sequence Delimiter. Field fact 01.09: ZONARE US has
        // SourceImageSequence (0008,2112) with undefined length before
        // PixelData - without this the walker bails with
        // "JPEG pixel data not found".
        return skip_undefined_sequence(data, pos);
    }
    if *pos + len > data.len() {
        return false;
    }
    super::skip_value(pos, len);
    true
}

/// Skip an undefined-length sequence: nested Items may themselves be
/// undefined-length (closed by Item Delimitation E00D) and contain regular
/// elements (explicit VR, with an implicit-VR fallback); the sequence ends
/// at the matching Sequence Delimitation (E0DD).
fn skip_undefined_sequence(data: &[u8], pos: &mut usize) -> bool {
    let mut depth = 1usize;
    loop {
        if *pos + 8 > data.len() {
            return false;
        }
        let g = u16_le(data, *pos);
        let e = u16_le(data, *pos + 2);
        match (g, e) {
            (0xFFFE, 0xE000) => {
                let len = u32_from_slice(&data[*pos + 4..*pos + 8]) as usize;
                *pos += 8;
                if len == 0xFFFF_FFFF {
                    depth += 1;
                } else {
                    if *pos + len > data.len() {
                        return false;
                    }
                    *pos += len;
                }
            }
            (0xFFFE, 0xE00D) | (0xFFFE, 0xE0DD) => {
                *pos += 8;
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return true;
                }
            }
            _ => {
                // Regular element inside an Item. The tag took 4 bytes, so the
                // VR needs 2 more and the length field up to 4 after that; a
                // header truncated inside the item would index past the end.
                *pos += 4;
                if *pos + 2 > data.len() {
                    return false;
                }
                let vr_ok = std::str::from_utf8(&data[*pos..*pos + 2])
                    .map(|s| {
                        matches!(
                            s,
                            "AE" | "AS"
                                | "AT"
                                | "CS"
                                | "DA"
                                | "DS"
                                | "DT"
                                | "FD"
                                | "FL"
                                | "IS"
                                | "LO"
                                | "LT"
                                | "OB"
                                | "OD"
                                | "OF"
                                | "OL"
                                | "OW"
                                | "PN"
                                | "SH"
                                | "SL"
                                | "SQ"
                                | "SS"
                                | "ST"
                                | "TM"
                                | "UC"
                                | "UI"
                                | "UL"
                                | "UN"
                                | "UR"
                                | "US"
                                | "UT"
                                | "UV"
                        )
                    })
                    .unwrap_or(false);
                if vr_ok {
                    if *pos + 6 > data.len() {
                        return false;
                    }
                    let vr = std::str::from_utf8(&data[*pos..*pos + 2]).unwrap_or("");
                    *pos += 2;
                    let extended = matches!(
                        vr,
                        "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT" | "UV"
                    );
                    let len: usize = if extended {
                        *pos += 2;
                        u32_from_slice(&data[*pos..*pos + 4]) as usize
                    } else {
                        u16_le(data, *pos) as usize
                    };
                    *pos += if extended { 4 } else { 2 };
                    if len == 0xFFFF_FFFF {
                        depth += 1; // nested undefined SQ value
                        continue;
                    }
                    if *pos + len > data.len() {
                        return false;
                    }
                    *pos += len;
                    if !len.is_multiple_of(2) {
                        *pos += 1; // PS3.5 7.1 pad byte
                    }
                } else {
                    // Implicit VR fallback (conformant compressed-TS
                    // datasets encode item contents implicitly).
                    let len = u32_from_slice(&data[*pos..*pos + 4]) as usize;
                    *pos += 4;
                    if len == 0xFFFF_FFFF {
                        depth += 1;
                        continue;
                    }
                    if *pos + len > data.len() {
                        return false;
                    }
                    *pos += len;
                }
            }
        }
    }
}

fn skip_implicit(data: &[u8], pos: &mut usize) -> bool {
    if *pos + 4 > data.len() {
        return false;
    }
    let len = u32_from_slice(&data[*pos..*pos + 4]) as usize;
    *pos += 4;
    if *pos + len > data.len() {
        return false;
    }
    super::skip_value(pos, len);
    true
}

// ===========================================================================
// Pixel layout conversion
// ===========================================================================

fn interleaved_to_planar(data: &[u8], rows: usize, cols: usize) -> Vec<u8> {
    let pc = rows * cols;
    let mut planar = vec![0u8; pc * 3];
    let (r, rest) = planar.split_at_mut(pc);
    let (g, b) = rest.split_at_mut(pc);
    for i in 0..pc {
        r[i] = data[i * 3];
        g[i] = data[i * 3 + 1];
        b[i] = data[i * 3 + 2];
    }
    planar
}

fn planar_to_interleaved(data: &[u8], rows: usize, cols: usize) -> Vec<u8> {
    let pc = rows * cols;
    let mut interleaved = vec![0u8; pc * 3];
    for i in 0..pc {
        interleaved[i * 3] = data[i];
        interleaved[i * 3 + 1] = data[pc + i];
        interleaved[i * 3 + 2] = data[2 * pc + i];
    }
    interleaved
}

// ===========================================================================
// JPEG Lossless decode-only codecs (Process 14, 14.1)
// ===========================================================================
//
// jpeg-decoder supports SOF3 (JPEG Lossless, Process 14) in addition to
// SOF0 (Baseline). These codecs are decode-only - we can read legacy
// JPEG Lossless DICOM studies but don't encode to this format.
// JPEG-LS (T.87) is the modern lossless replacement.

/// Codec for JPEG Lossless, Non-Hierarchical (Process 14).
///
/// Transfer Syntax: 1.2.840.10008.1.2.4.57
/// Decode-only - no encode support.
pub struct JpegLosslessCodec;

impl DicomCodec for JpegLosslessCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::JPEG_LOSSLESS
    }

    fn name(&self) -> &'static str {
        "JPEG Lossless (Process 14)"
    }

    fn decode_only(&self) -> bool {
        true
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        jpeg_lossless_decode(dicom_bytes)
    }

    fn encode(&self, _pixels: &DecodedPixels, _meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::EncodeFailed(
            "JPEG Lossless (Process 14) encode is not implemented. Use JPEG-LS instead.".into(),
        ))
    }
}

/// Codec for JPEG Lossless, SV1 (Process 14.1).
///
/// Transfer Syntax: 1.2.840.10008.1.2.4.70
/// Decode-only - identical to .57 from decoder perspective.
pub struct JpegLosslessSv1Codec;

impl DicomCodec for JpegLosslessSv1Codec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::JPEG_LOSSLESS_SV1
    }

    fn name(&self) -> &'static str {
        "JPEG Lossless SV1 (Process 14.1)"
    }

    fn decode_only(&self) -> bool {
        true
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        jpeg_lossless_decode(dicom_bytes)
    }

    fn encode(&self, _pixels: &DecodedPixels, _meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::EncodeFailed(
            "JPEG Lossless (Process 14.1) encode is not implemented. Use JPEG-LS instead.".into(),
        ))
    }
}

/// Shared decode logic for JPEG Lossless (Process 14/14.1).
///
/// `jpeg-decoder` automatically detects SOF3 (lossless) vs SOF0 (baseline)
/// and applies the correct Huffman predictor. The API is identical to
/// the baseline path - we just use it with a different TS UID.
fn jpeg_lossless_decode(dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
    // Extract JPEG pixel data from DICOM
    let jpeg_data = extract_jpeg_pixel_data(dicom_bytes)
        .ok_or_else(|| CodecError::DecodeFailed("JPEG pixel data not found".into()))?;

    if jpeg_data.is_empty() {
        return Err(CodecError::DecodeFailed("JPEG pixel data is empty".into()));
    }

    // Parse metadata for image dimensions
    let meta = parse_jpeg_meta(dicom_bytes)?;

    // Decode JPEG (jpeg-decoder handles both SOF0 Baseline and SOF3 Lossless)
    let mut decoder = jpeg_decoder::Decoder::new(&jpeg_data[..]);
    let pixels = decoder
        .decode()
        .map_err(|e| CodecError::DecodeFailed(format!("JPEG Lossless decode error: {e}")))?;

    let info = decoder
        .info()
        .ok_or_else(|| CodecError::DecodeFailed("JPEG decoder info unavailable".into()))?;

    let decoded_rows = info.height;
    let decoded_cols = info.width;
    let decoded_samples = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 | jpeg_decoder::PixelFormat::L16 => 1,
        _ => 3, // RGB24, CMYK, etc.
    };

    // JPEG Lossless supports 8-16 bits. For 16-bit, jpeg-decoder returns
    // pixels as L16 (little-endian u16 pairs packed in u8 slice).
    let bits_allocated = if info.pixel_format == jpeg_decoder::PixelFormat::L16 {
        16u8
    } else {
        8u8
    };

    if decoded_rows != meta.rows || decoded_cols != meta.columns {
        return Err(CodecError::DecodeFailed(format!(
            "JPEG Lossless dimensions ({decoded_cols},{decoded_rows}) don't match DICOM metadata ({},{})",
            meta.columns, meta.rows
        )));
    }

    // Convert to planar layout if color
    let planar = if decoded_samples == 3 {
        if bits_allocated == 16 {
            // 16-bit interleaved (6 bytes/pixel) -> planar
            interleaved16_to_planar(&pixels, meta.rows as usize, meta.columns as usize)
        } else {
            interleaved_to_planar(&pixels, meta.rows as usize, meta.columns as usize)
        }
    } else {
        pixels
    };

    Ok(DecodedPixels {
        data: planar,
        rows: meta.rows,
        columns: meta.columns,
        samples_per_pixel: decoded_samples,
        bits_allocated,
        photometric_interpretation: meta.photometric_interpretation,
    })
}

/// Convert 16-bit interleaved RGB (6 bytes/pixel) to planar layout.
fn interleaved16_to_planar(data: &[u8], rows: usize, cols: usize) -> Vec<u8> {
    let pc = rows * cols;
    let expected = pc * 6;
    // SAFEGUARD: jpeg-decoder Process 14 may return slightly different byte count
    let actual = data.len().min(expected);
    let mut planar = vec![0u8; expected];
    let (r, rest) = planar.split_at_mut(pc * 2);
    let (g, b) = rest.split_at_mut(pc * 2);
    for i in 0..pc {
        let src = i * 6;
        if src + 6 > actual {
            break;
        }
        let dst = i * 2;
        r[dst..dst + 2].copy_from_slice(&data[src..src + 2]);
        g[dst..dst + 2].copy_from_slice(&data[src + 2..src + 4]);
        b[dst..dst + 2].copy_from_slice(&data[src + 4..src + 6]);
    }
    planar
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jpeg_codec_transfer_syntax() {
        assert_eq!(
            JpegBaselineCodec.transfer_syntax_uid(),
            ts_uid::JPEG_BASELINE
        );
    }

    #[test]
    fn test_jpeg_encode_grayscale() {
        let w = 64usize;
        let h = 64usize;
        let mut data = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                data[y * w + x] = ((x + y) % 256) as u8;
            }
        }

        let pixels = DecodedPixels {
            data: data.clone(),
            rows: h as u16,
            columns: w as u16,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let meta = DicomMeta {
            rows: h as u16,
            columns: w as u16,
            samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };

        let encoded = JpegBaselineCodec.encode(&pixels, &meta).unwrap();
        assert!(!encoded.is_empty());
        assert!(encoded.len() < data.len()); // JPEG should compress
    }

    #[test]
    fn test_jpeg_encode_rejects_16bit() {
        let pixels = DecodedPixels {
            data: vec![0u8; 100],
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
            bits_stored: 12,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };
        assert!(JpegBaselineCodec.encode(&pixels, &meta).is_err());
    }

    #[test]
    fn test_interleaved_planar_roundtrip() {
        let data = vec![10, 20, 30, 40, 50, 60, 70, 80, 90];
        let planar = interleaved_to_planar(&data, 1, 3);
        let back = planar_to_interleaved(&planar, 1, 3);
        assert_eq!(back, data);
    }

    /// Field shape 01.09 (ZONARE US): SourceImageSequence (0008,2112) with
    /// undefined length and an undefined-length Item containing elements sits
    /// BEFORE PixelData. The walker must skip it, not bail.
    #[test]
    fn test_find_tag_bytes_skips_undefined_sequence() {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        let ts = b"1.2.840.10008.1.2.4.50";
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        out.extend_from_slice(ts);
        if !ts.len().is_multiple_of(2) {
            out.push(0x00);
        }

        // SQ (0008,2112), undefined length, one undefined-length item
        out.extend_from_slice(&0x0008u16.to_le_bytes());
        out.extend_from_slice(&0x2112u16.to_le_bytes());
        out.extend_from_slice(b"SQ");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let ui1 = b"1.2.840.10008.5.1.4.1.1.3.1";
        out.extend_from_slice(&0x0008u16.to_le_bytes());
        out.extend_from_slice(&0x1150u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(ui1.len() as u16).to_le_bytes());
        out.extend_from_slice(ui1);
        if !ui1.len().is_multiple_of(2) {
            out.push(0x00);
        }
        out.extend_from_slice(&[0xFE, 0xFF, 0x0D, 0xE0]); // Item Delimitation
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]); // Sequence Delimitation
        out.extend_from_slice(&0u32.to_le_bytes());

        // PixelData (7FE0,0010) OB, defined length 4
        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OB");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&[0xAA; 4]);

        assert_eq!(find_tag_bytes(&out, TAG_PIXEL_DATA), Some(vec![0xAA; 4]));
    }

    /// Multi-frame JPEG Baseline: one codestream per fragment. Frame order
    /// must survive decode (field 01.09: ZONARE 64-frame US).
    #[test]
    fn test_jpeg_decode_multiframe_baseline() {
        let make_frame = |v: u8| {
            let pixels = DecodedPixels {
                data: vec![v; 64],
                rows: 8,
                columns: 8,
                samples_per_pixel: 1,
                bits_allocated: 8,
                photometric_interpretation: "MONOCHROME2".into(),
            };
            let meta = DicomMeta {
                rows: 8,
                columns: 8,
                samples_per_pixel: 1,
                bits_allocated: 8,
                bits_stored: 8,
                photometric_interpretation: "MONOCHROME2".into(),
                planar_configuration: None,
                pixel_representation: 0,
                frames: 1,
            };
            JpegBaselineCodec.encode(&pixels, &meta).unwrap()
        };
        let f0 = make_frame(10);
        let f1 = make_frame(200);

        let mut enc = Vec::new();
        enc.extend_from_slice(&0x7FE0u16.to_le_bytes());
        enc.extend_from_slice(&0x0010u16.to_le_bytes());
        enc.extend_from_slice(b"OB");
        enc.extend_from_slice(&[0, 0]);
        enc.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        enc.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        enc.extend_from_slice(&8u32.to_le_bytes());
        enc.extend_from_slice(&0u32.to_le_bytes());
        enc.extend_from_slice(&((8 + f0.len()) as u32).to_le_bytes());
        for f in [&f0, &f1] {
            enc.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            enc.extend_from_slice(&(f.len() as u32).to_le_bytes());
            enc.extend_from_slice(f);
        }
        enc.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        enc.extend_from_slice(&0u32.to_le_bytes());

        let mut dicom = Vec::new();
        dicom.extend_from_slice(&[0u8; 128]);
        dicom.extend_from_slice(b"DICM");
        let ts = b"1.2.840.10008.1.2.4.50";
        dicom.extend_from_slice(&0x0002u16.to_le_bytes());
        dicom.extend_from_slice(&0x0010u16.to_le_bytes());
        dicom.extend_from_slice(b"UI");
        dicom.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        dicom.extend_from_slice(ts);
        if !ts.len().is_multiple_of(2) {
            dicom.push(0x00);
        }
        let us_tag = |d: &mut Vec<u8>, tag: (u16, u16), v: u16| {
            d.extend_from_slice(&tag.0.to_le_bytes());
            d.extend_from_slice(&tag.1.to_le_bytes());
            d.extend_from_slice(b"US");
            d.extend_from_slice(&2u16.to_le_bytes());
            d.extend_from_slice(&v.to_le_bytes());
        };
        us_tag(&mut dicom, (0x0028, 0x0010), 8);
        us_tag(&mut dicom, (0x0028, 0x0011), 8);
        us_tag(&mut dicom, (0x0028, 0x0002), 1);
        us_tag(&mut dicom, (0x0028, 0x0100), 8);
        dicom.extend_from_slice(&0x0028u16.to_le_bytes());
        dicom.extend_from_slice(&0x0008u16.to_le_bytes());
        dicom.extend_from_slice(b"IS");
        dicom.extend_from_slice(&1u16.to_le_bytes());
        dicom.extend_from_slice(b"2");
        dicom.push(b' ');
        let pi = b"MONOCHROME2";
        dicom.extend_from_slice(&0x0028u16.to_le_bytes());
        dicom.extend_from_slice(&0x0004u16.to_le_bytes());
        dicom.extend_from_slice(b"CS");
        dicom.extend_from_slice(&(pi.len() as u16).to_le_bytes());
        dicom.extend_from_slice(pi);
        dicom.push(b' ');
        dicom.extend_from_slice(&enc);

        let pixels = JpegBaselineCodec.decode(&dicom).unwrap();
        assert_eq!(pixels.data.len(), 128);
        assert_eq!(pixels.rows, 8);
        assert_eq!(pixels.columns, 8);
        assert_eq!(pixels.samples_per_pixel, 1);
        let avg0 = pixels.data[..64].iter().map(|&b| b as u32).sum::<u32>() / 64;
        let avg1 = pixels.data[64..].iter().map(|&b| b as u32).sum::<u32>() / 64;
        assert!(avg0 < 30, "frame 0 not dark: {}", avg0);
        assert!(avg1 > 170, "frame 1 not bright: {}", avg1);
    }
}
