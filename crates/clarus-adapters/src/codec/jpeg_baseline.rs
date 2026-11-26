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
    TAG_BITS_ALLOCATED, TAG_BITS_STORED, TAG_COLUMNS, TAG_PHOTOMETRIC_INTERPRETATION,
    TAG_PIXEL_DATA, TAG_PIXEL_REPRESENTATION, TAG_ROWS, TAG_SAMPLES_PER_PIXEL,
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
        // (PS3.5 A.4 lets an encoder break a codestream anywhere), so the
        // fragment count alone cannot tell a split frame from a frame
        // boundary. The Basic Offset Table is what distinguishes them: a
        // non-empty table gives one offset per frame (PS3.5 A.4), and the
        // fragments between two offsets belong to one frame. Counting
        // fragments instead rejected a conformant two-frame file whose first
        // frame arrived in two fragments, because 3 fragments != 2 frames.
        let bot = super::jpegls::extract_basic_offset_table(dicom_bytes);
        let fragments = group_fragments_by_frame(fragments, bot, frames)?;
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
            // PS3.3 C.7.6.3.1.1: SamplesPerPixel describes the value that is
            // stored, so the decoded JPEG format and the attribute have to
            // agree. They were not compared, and a three-component codestream
            // declared as one sample decoded as colour behind metadata that
            // says monochrome - every downstream consumer then reads 3N bytes
            // as N samples.
            if s != meta.samples_per_pixel as usize {
                return Err(CodecError::DecodeFailed(format!(
                    "JPEG Baseline: decoded {s} component(s) but SamplesPerPixel (0028,0002) \
                     declares {}",
                    meta.samples_per_pixel
                )));
            }
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

        // PS3.3 C.7.6.3.1.1: SamplesPerPixel is 1 or 3. Anything else fell
        // through to the monochrome branch below, so a two-sample image was
        // encoded as a JPEG with the samples silently reinterpreted as Luma.
        if pixels.samples_per_pixel != 1 && pixels.samples_per_pixel != 3 {
            return Err(CodecError::EncodeFailed(format!(
                "JPEG Baseline: SamplesPerPixel must be 1 or 3, got {}",
                pixels.samples_per_pixel
            )));
        }

        // Build interleaved pixel buffer for JPEG encoder.
        // PS3.3 C.7.6.3.1.3: with SamplesPerPixel 3 the value holds three
        // whole-image planes - rows * columns bytes each. `planar_to_
        // interleaved` indexes all three, so a short buffer is a panic on
        // input rather than an error the caller can handle.
        let pixel_count = pixels.rows as usize * pixels.columns as usize;
        let interleaved = if pixels.samples_per_pixel == 3 {
            let needed = pixel_count
                .checked_mul(3)
                .ok_or_else(|| CodecError::EncodeFailed("pixel count overflow".into()))?;
            if pixels.data.len() < needed {
                return Err(CodecError::EncodeFailed(format!(
                    "JPEG Baseline: colour buffer holds {} bytes, {} rows x {} columns x 3 \
                     samples need {}",
                    pixels.data.len(),
                    pixels.rows,
                    pixels.columns,
                    needed
                )));
            }
            planar_to_interleaved(&pixels.data, pixels.rows as usize, pixels.columns as usize)
        } else {
            if pixels.data.len() < pixel_count {
                return Err(CodecError::EncodeFailed(format!(
                    "JPEG Baseline: monochrome buffer holds {} bytes, {} rows x {} columns \
                     need {}",
                    pixels.data.len(),
                    pixels.rows,
                    pixels.columns,
                    pixel_count
                )));
            }
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
        Ok(crate::codec::build_encapsulated_pixeldata(&compressed))
    }
}

/// Group the fragments of an encapsulated Pixel Data value into frames.
///
/// PS3.5 A.4: a non-empty Basic Offset Table holds one offset per frame, in
/// bytes from the first fragment's Item tag to the Item tag of the first
/// fragment of that frame. Two fragments between consecutive offsets are one
/// frame split in two, and must be joined before the JPEG decoder sees them.
/// An empty table says nothing about frame boundaries, so a single-frame
/// image is the only case where joining everything is correct.
fn group_fragments_by_frame(
    fragments: Vec<Vec<u8>>,
    bot: Vec<u32>,
    frames: usize,
) -> Result<Vec<Vec<u8>>, CodecError> {
    if bot.len() < 2 {
        // No usable table. The reading left is the one `dicom_rs::frame` states
        // for the same situation: with fewer entries than frames, the last
        // named frame runs to the END of the item stream, so trailing fragments
        // belong to it. That is also the only reading that decodes a frame split
        // across fragments when the table is empty - an encoder that emits an
        // empty table and splits frame 0 over two Items is legal, and refusing
        // it made a perfectly decodable instance unreadable.
        if frames <= 1 {
            return Ok(vec![fragments.into_iter().flatten().collect()]);
        }
        if fragments.len() < frames {
            return Err(CodecError::DecodeFailed(format!(
                "JPEG Baseline: {} fragments for {} frames with an empty Basic Offset Table",
                fragments.len(),
                frames
            )));
        }
        // One frame per fragment up to the declared count; the remainder (which
        // is the trailing fragments of the LAST frame) joins it.
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(frames);
        let mut it = fragments.into_iter();
        for _ in 0..frames - 1 {
            match it.next() {
                Some(frag) => out.push(frag),
                None => {
                    return Err(CodecError::DecodeFailed(
                        "JPEG Baseline: fewer fragments than frames".into(),
                    ));
                }
            }
        }
        let mut last = Vec::new();
        for frag in it {
            last.extend_from_slice(&frag);
        }
        out.push(last);
        return Ok(out);
    }
    if bot.len() != frames {
        return Err(CodecError::DecodeFailed(format!(
            "JPEG Baseline: Basic Offset Table holds {} entries for {} frames",
            bot.len(),
            frames
        )));
    }

    // PS3.5 A.4: an offset is the distance from the first byte of the first
    // fragment's Item tag to the first byte of the target frame's Item tag.
    // Fragment k therefore starts at the sum of the Item headers and values
    // before it - the value of fragment 0 being padded to even length.
    let mut lengths = Vec::with_capacity(fragments.len());
    let mut starts = Vec::with_capacity(fragments.len());
    let mut at = 0u32;
    for frag in &fragments {
        starts.push(at);
        let padding = if frag.len().is_multiple_of(2) { 0 } else { 1 };
        at = at
            .checked_add(8 + frag.len() as u32 + padding)
            .ok_or_else(|| CodecError::DecodeFailed("fragment offsets overflow".into()))?;
        lengths.push(frag);
    }

    let mut grouped = Vec::with_capacity(frames);
    let mut current: Vec<u8> = Vec::new();
    let mut frame = 0usize;
    for (frag, start) in lengths.into_iter().zip(starts) {
        while frame + 1 < bot.len() && start >= bot[frame + 1] {
            grouped.push(std::mem::take(&mut current));
            frame += 1;
        }
        current.extend_from_slice(frag);
    }
    grouped.push(current);
    Ok(grouped)
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
    // PS3.5 8.1.1: BitsStored (0028,0101) is the count of bits that actually
    // carry signal and shall never exceed BitsAllocated (0028,0100). Reading
    // BitsAllocated here declared a 12-bit-in-16 CT as 16-bit, so the
    // re-encoded output carried BitsStored=16 / HighBit=15 and a viewer showed
    // the four uninitialised low bits as signal.
    let bits_stored: u8 = find_tag_u16(data, TAG_BITS_STORED)
        .map(|v| v as u8)
        .filter(|v| *v > 0 && *v <= bits_allocated)
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
    // PS3.5 6.2 Table 6.2-1 via `vr_widths`: the list that used to be written
    // out here omitted SV and UV, so those elements took the short branch and
    // the walk resumed inside their header.
    let vr = data.get(*pos..*pos + 2)?;
    let extended = super::vr_widths::is_long_form_vr(vr);
    *pos += 2;
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
    // PS3.5 6.2 Table 6.2-1 via `vr_widths`; see `read_explicit_val`.
    let Some(vr_bytes) = data.get(*pos..*pos + 2) else {
        return false;
    };
    let extended = super::vr_widths::is_long_form_vr(vr_bytes);
    *pos += 2;
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
                // PS3.5 6.2 Table 6.2-1: the width class is a property of the
                // VR, and `vr_widths` is the one place this table is stated. The
                // hand-written `matches!` here listed thirteen VRs for the long
                // form and omitted OV and SV, so an Other-64-bit-Very-Long or
                // Signed-64-bit-Very-Long element inside the item was stepped
                // over with the SHORT form and every following element was read
                // from inside its header.
                let vr_pair = &data[*pos..*pos + 2];
                let extended = super::vr_widths::is_long_form_vr(vr_pair);
                // A pair that is not two VR letters is not an Explicit VR
                // element; fall through to the implicit branch below rather
                // than guessing.
                let is_vr = std::str::from_utf8(vr_pair)
                    .map(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_uppercase()))
                    .unwrap_or(false);
                if is_vr {
                    if *pos + 6 > data.len() {
                        return false;
                    }
                    *pos += 2;
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
                    // PS3.5 6.2: the declared length already contains the pad,
                    // so adding it again skipped a byte past every odd-length
                    // element and desynchronised the rest of the item.
                    *pos += len;
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

    /// R-01: BitsStored (0028,0101) is what the codec reports, not
    /// BitsAllocated (0028,0100). PS3.5 8.1.1 forbids BitsStored from
    /// exceeding BitsAllocated, so a larger value is clamped, not believed.
    #[test]
    fn the_codec_reports_bits_stored_not_bits_allocated() {
        let dicom = build_meta_only(16, 12);
        let meta = parse_jpeg_meta(&dicom).expect("metadata");
        assert_eq!(meta.bits_allocated, 16);
        assert_eq!(meta.bits_stored, 12, "BitsStored decides");

        let dicom = build_meta_only(8, 20);
        let meta = parse_jpeg_meta(&dicom).expect("metadata");
        assert_eq!(meta.bits_stored, 8, "clamped to BitsAllocated");
    }

    /// A Part 10 file with only the geometry the metadata reader needs.
    fn build_meta_only(bits_allocated: u16, bits_stored: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        push_us(&mut out, TAG_ROWS, 2);
        push_us(&mut out, TAG_COLUMNS, 2);
        push_us(&mut out, TAG_SAMPLES_PER_PIXEL, 1);
        push_us(&mut out, TAG_BITS_ALLOCATED, bits_allocated);
        push_us(&mut out, TAG_BITS_STORED, bits_stored);
        out
    }

    fn push_us(out: &mut Vec<u8>, tag: (u16, u16), value: u16) {
        out.extend_from_slice(&tag.0.to_le_bytes());
        out.extend_from_slice(&tag.1.to_le_bytes());
        out.extend_from_slice(b"US");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
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
        // PS3.5 6.2: the declared Value Length is the even, padded length, so a
        // 27-character UID declares 28 and the NUL is its last byte. Declaring
        // 27 and appending the pad outside wrote an element one byte longer
        // than it declared, which desynchronised the walk that follows it.
        let ui1_len = ui1.len() + usize::from(!ui1.len().is_multiple_of(2));
        out.extend_from_slice(&0x0008u16.to_le_bytes());
        out.extend_from_slice(&0x1150u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(ui1_len as u16).to_le_bytes());
        out.extend_from_slice(ui1);
        if ui1_len != ui1.len() {
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

    fn monochrome_meta(rows: u16, columns: u16, frames: u32) -> DicomMeta {
        DicomMeta {
            rows,
            columns,
            samples_per_pixel: 1,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames,
        }
    }

    fn encode_frame(v: u8, rows: u16, columns: u16) -> Vec<u8> {
        let pixels = DecodedPixels {
            data: vec![v; rows as usize * columns as usize],
            rows,
            columns,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        JpegBaselineCodec
            .encode(&pixels, &monochrome_meta(rows, columns, 1))
            .unwrap()
    }

    /// Build an explicit-VR instance whose encapsulated Pixel Data carries
    /// `frames` frames described by `bot`, with `fragments` in Item order.
    fn build_instance(
        rows: u16,
        columns: u16,
        frames: u32,
        bot: &[u32],
        fragments: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut enc = Vec::new();
        enc.extend_from_slice(&0x7FE0u16.to_le_bytes());
        enc.extend_from_slice(&0x0010u16.to_le_bytes());
        enc.extend_from_slice(b"OB");
        enc.extend_from_slice(&[0, 0]);
        enc.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        enc.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        enc.extend_from_slice(&((bot.len() * 4) as u32).to_le_bytes());
        for offset in bot {
            enc.extend_from_slice(&offset.to_le_bytes());
        }
        for frag in fragments {
            enc.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            // PS3.5 A.4 with 6.2: an Item's Value is padded to an even length
            // and the pad counts in the declared length. Declaring the raw
            // length and appending the pad outside it wrote an Item that
            // occupied one byte more than it declared.
            let frag_len = frag.len() + usize::from(!frag.len().is_multiple_of(2));
            enc.extend_from_slice(&(frag_len as u32).to_le_bytes());
            enc.extend_from_slice(frag);
            if frag_len != frag.len() {
                enc.push(0);
            }
        }
        enc.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        enc.extend_from_slice(&0u32.to_le_bytes());

        let mut d = Vec::new();
        d.extend_from_slice(&[0u8; 128]);
        d.extend_from_slice(b"DICM");
        let ts = b"1.2.840.10008.1.2.4.50";
        let ts_len = ts.len() + usize::from(!ts.len().is_multiple_of(2));
        d.extend_from_slice(&0x0002u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"UI");
        d.extend_from_slice(&(ts_len as u16).to_le_bytes());
        d.extend_from_slice(ts);
        if ts_len != ts.len() {
            d.push(0);
        }
        let us = |d: &mut Vec<u8>, tag: (u16, u16), v: u16| {
            d.extend_from_slice(&tag.0.to_le_bytes());
            d.extend_from_slice(&tag.1.to_le_bytes());
            d.extend_from_slice(b"US");
            d.extend_from_slice(&2u16.to_le_bytes());
            d.extend_from_slice(&v.to_le_bytes());
        };
        us(&mut d, (0x0028, 0x0010), rows);
        us(&mut d, (0x0028, 0x0011), columns);
        us(&mut d, (0x0028, 0x0002), 1);
        us(&mut d, (0x0028, 0x0100), 8);
        if frames > 1 {
            d.extend_from_slice(&0x0028u16.to_le_bytes());
            d.extend_from_slice(&0x0008u16.to_le_bytes());
            d.extend_from_slice(b"IS");
            let text = frames.to_string();
            let len = if text.len().is_multiple_of(2) {
                text.len()
            } else {
                text.len() + 1
            };
            d.extend_from_slice(&(len as u16).to_le_bytes());
            d.extend_from_slice(text.as_bytes());
            if !text.len().is_multiple_of(2) {
                d.push(b' ');
            }
        }
        let pi = b"MONOCHROME2 ";
        d.extend_from_slice(&0x0028u16.to_le_bytes());
        d.extend_from_slice(&0x0004u16.to_le_bytes());
        d.extend_from_slice(b"CS");
        d.extend_from_slice(&(pi.len() as u16).to_le_bytes());
        d.extend_from_slice(pi);
        d.extend_from_slice(&enc);
        d
    }

    /// PS3.5 A.4: two fragments between consecutive Basic Offset Table
    /// entries are ONE frame split in two, not two frames. Counting fragments
    /// instead of reading the table rejected a conformant 2-frame file whose
    /// first frame arrived in two fragments (3 fragments != 2 frames).
    #[test]
    fn a_frame_split_by_the_basic_offset_table_is_joined() {
        let (rows, columns) = (8u16, 8u16);
        let frame0 = encode_frame(20, rows, columns);
        let frame1 = encode_frame(220, rows, columns);
        // PS3.5 A.4: a fragment value is even-length, so cut on an even
        // boundary (frame0 is even, but do not rely on it).
        let cut = (frame0.len() / 2) & !1;
        assert!(cut > 0 && cut < frame0.len());

        // Offsets count bytes from the first fragment's Item tag: fragment 0
        // starts at 0, fragment 1 at 8 + cut, and frame 1 after fragment 1's
        // Item header plus its value.
        let second = (8 + cut) as u32;
        let rest = (frame0.len() - cut) as u32;
        let third = second + 8 + rest;
        let bot = vec![0u32, third];
        let fragments = vec![
            frame0[..cut].to_vec(),
            frame0[cut..].to_vec(),
            frame1.clone(),
        ];

        let dicom = build_instance(rows, columns, 2, &bot, &fragments);
        let pixels = JpegBaselineCodec
            .decode(&dicom)
            .expect("the offset table says the first two fragments are one frame");
        assert_eq!(pixels.data.len(), 128);
        let avg0 = pixels.data[..64].iter().map(|&b| b as u32).sum::<u32>() / 64;
        let avg1 = pixels.data[64..].iter().map(|&b| b as u32).sum::<u32>() / 64;
        assert!(avg0 < 60, "frame 0 not dark: {avg0}");
        assert!(avg1 > 170, "frame 1 not bright: {avg1}");
    }

    /// A colour buffer shorter than rows x columns x 3 is refused.
    ///
    /// `planar_to_interleaved` reads three whole planes, so the missing bytes
    /// were an index panic on input the caller can fix.
    #[test]
    fn a_short_colour_buffer_is_refused_not_indexed() {
        let pixels = DecodedPixels {
            data: vec![0u8; 100],
            rows: 10,
            columns: 10,
            samples_per_pixel: 3,
            bits_allocated: 8,
            photometric_interpretation: "RGB".into(),
        };
        let meta = DicomMeta {
            samples_per_pixel: 3,
            ..monochrome_meta(10, 10, 1)
        };
        let err = JpegBaselineCodec
            .encode(&pixels, &meta)
            .expect_err("the buffer is one plane short");
        let CodecError::EncodeFailed(msg) = err else {
            panic!("expected EncodeFailed");
        };
        assert!(msg.contains("300"), "message should name the need: {msg}");
    }

    /// PS3.5 6.2 Table 6.2-1: SV and UV are long-form, so an element carrying
    /// one sits in an 8-byte header. A walker that reads them as short-form
    /// resumes four bytes inside the header.
    #[test]
    fn the_walker_treats_sv_and_uv_as_long_form() {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        let ts = b"1.2.840.10008.1.2.4.50";
        out.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        out.extend_from_slice(ts);

        // (0018,1234) UV before the target: 2 values of 8 bytes.
        out.extend_from_slice(&0x0018u16.to_le_bytes());
        out.extend_from_slice(&0x1234u16.to_le_bytes());
        out.extend_from_slice(b"UV");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&[0x5A; 16]);

        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OB");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&[0xAA; 4]);

        assert_eq!(find_tag_bytes(&out, TAG_PIXEL_DATA), Some(vec![0xAA; 4]));
    }

    /// PS3.3 C.7.6.3.1.1: SamplesPerPixel describes the value that is stored,
    /// so it must agree with the component count the codestream actually has.
    ///
    /// They were never compared, and a three-component codestream declared as
    /// one sample decoded as colour behind metadata that says monochrome.
    #[test]
    fn a_component_count_that_contradicts_samples_per_pixel_is_refused() {
        let frame = {
            let pixels = DecodedPixels {
                data: vec![7u8; 3 * 16],
                rows: 1,
                columns: 16,
                samples_per_pixel: 3,
                bits_allocated: 8,
                photometric_interpretation: "RGB".into(),
            };
            let meta = DicomMeta {
                rows: 1,
                columns: 16,
                samples_per_pixel: 3,
                bits_allocated: 8,
                bits_stored: 8,
                photometric_interpretation: "RGB".into(),
                planar_configuration: Some(0),
                pixel_representation: 0,
                frames: 1,
            };
            JpegBaselineCodec
                .encode(&pixels, &meta)
                .expect("colour encodes")
        };

        // One fragment, declared as ONE sample although it is a colour stream.
        let mut dicom = Vec::new();
        dicom.extend_from_slice(&[0u8; 128]);
        dicom.extend_from_slice(b"DICM");
        let us_tag = |d: &mut Vec<u8>, tag: (u16, u16), v: u16| {
            d.extend_from_slice(&tag.0.to_le_bytes());
            d.extend_from_slice(&tag.1.to_le_bytes());
            d.extend_from_slice(b"US");
            d.extend_from_slice(&2u16.to_le_bytes());
            d.extend_from_slice(&v.to_le_bytes());
        };
        us_tag(&mut dicom, (0x0028, 0x0010), 1);
        us_tag(&mut dicom, (0x0028, 0x0011), 16);
        us_tag(&mut dicom, (0x0028, 0x0002), 1); // LIE: the stream has 3
        us_tag(&mut dicom, (0x0028, 0x0100), 8);
        dicom.extend_from_slice(&0x7FE0u16.to_le_bytes());
        dicom.extend_from_slice(&0x0010u16.to_le_bytes());
        dicom.extend_from_slice(b"OB");
        dicom.extend_from_slice(&[0, 0]);
        dicom.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        dicom.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        dicom.extend_from_slice(&0u32.to_le_bytes()); // empty BOT
        dicom.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        dicom.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        dicom.extend_from_slice(&frame);
        dicom.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        dicom.extend_from_slice(&0u32.to_le_bytes());

        let err = JpegBaselineCodec
            .decode(&dicom)
            .expect_err("a 3-component stream declared as 1 sample is a contradiction");
        assert!(
            err.to_string().contains("SamplesPerPixel"),
            "the reason names the attribute, got: {err}"
        );
    }

    /// PS3.3 C.7.6.3.1.1: SamplesPerPixel is 1 or 3. Anything else fell through
    /// to the monochrome branch and was encoded as Luma.
    #[test]
    fn an_unsupported_encoder_sample_count_is_refused() {
        let pixels = DecodedPixels {
            data: vec![0u8; 4],
            rows: 1,
            columns: 2,
            samples_per_pixel: 2,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };
        let meta = DicomMeta {
            rows: 1,
            columns: 2,
            samples_per_pixel: 2,
            bits_allocated: 8,
            bits_stored: 8,
            photometric_interpretation: "MONOCHROME2".into(),
            planar_configuration: None,
            pixel_representation: 0,
            frames: 1,
        };
        let err = JpegBaselineCodec
            .encode(&pixels, &meta)
            .expect_err("two samples per pixel is not encodable here");
        assert!(
            err.to_string().contains("SamplesPerPixel must be 1 or 3"),
            "got: {err}"
        );
    }

    /// PS3.5 A.4: a frame may span SEVERAL fragments even when the Basic Offset
    /// Table is empty. The last declared frame runs to the end of the item
    /// stream, so its trailing fragments belong to it.
    ///
    /// The old code refused any fragment count that was not exactly the frame
    /// count, so a split codestream with no table was called unreadable.
    #[test]
    fn an_empty_offset_table_lets_the_last_frame_absorb_trailing_fragments() {
        // Three fragments for two declared frames: fragment 2 belongs to frame 1.
        let out = group_fragments_by_frame(
            vec![vec![1u8, 2], vec![3u8, 4], vec![5u8, 6]],
            Vec::new(),
            2,
        )
        .expect("a legal split");
        assert_eq!(out.len(), 2, "two frames");
        assert_eq!(out[0], vec![1u8, 2], "frame 0 is its own fragment");
        assert_eq!(out[1], vec![3u8, 4, 5, 6], "frame 1 absorbs the remainder");

        // Fewer fragments than frames is still unreadable.
        assert!(group_fragments_by_frame(vec![vec![1u8, 2]], Vec::new(), 2).is_err());
    }

    /// PS3.5 6.2 Table 6.2-1: an SV element inside an item must not be read
    /// with the short header. The hand-written list here omitted SV and OV.
    #[test]
    fn an_sv_element_inside_a_sequence_is_stepped_correctly() {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        // (0008,1190) SQ undefined length, one undefined item, then an SV.
        out.extend_from_slice(&0x0008u16.to_le_bytes());
        out.extend_from_slice(&0x1190u16.to_le_bytes());
        out.extend_from_slice(b"SQ");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // An SV element: 8-byte header value.
        out.extend_from_slice(&0x0088u16.to_le_bytes());
        out.extend_from_slice(&0x0200u16.to_le_bytes());
        out.extend_from_slice(b"SV");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&[1u8; 8]);
        out.extend_from_slice(&[0xFE, 0xFF, 0x0D, 0xE0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        // The payload must still be reachable after the sequence.
        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OB");
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&[0xAA; 4]);

        assert_eq!(
            find_tag_bytes(&out, TAG_PIXEL_DATA),
            Some(vec![0xAA; 4]),
            "an SV inside the item must not desynchronise the walk"
        );
    }
}
