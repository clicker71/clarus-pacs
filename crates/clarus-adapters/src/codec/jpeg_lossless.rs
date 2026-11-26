// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! JPEG Lossless DECODE (Process 14 / SOF3) via the `jpeg-decoder` crate.
//!
//! Transfer Syntaxes:
//! - 1.2.840.10008.1.2.4.57 (Process 14, Non-Hierarchical)
//! - 1.2.840.10008.1.2.4.70 (Process 14.1, Selection Value 1)
//!
//! Both codec types are DECODE-ONLY: `encode` returns `EncodeFailed` and
//! `decode_only` reports true. Lossless writes go through JPEG-LS
//! (1.2.840.10008.1.2.4.80/81).
//!
//! # References
//! - ISO/IEC 10918-1 Annex H - Lossless JPEG
//! - ITU-T T.81 Annex H

#![forbid(unsafe_code)]

use super::{CodecError, DecodedPixels, DicomCodec, DicomMeta, ts_uid};

// -- Codec structs -----------------------------------------------------

/// Codec for JPEG Lossless, Non-Hierarchical (Process 14).
/// Transfer Syntax: 1.2.840.10008.1.2.4.57
pub struct JpegLosslessCodec;

impl DicomCodec for JpegLosslessCodec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::JPEG_LOSSLESS
    }

    fn name(&self) -> &'static str {
        "JPEG Lossless (Process 14)"
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        jpeg_lossless_decode(dicom_bytes)
    }

    fn decode_only(&self) -> bool {
        true
    }

    fn encode(&self, _pixels: &DecodedPixels, _meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::EncodeFailed(
            "JPEG Lossless encode removed in v0.4.0 — use JPEG-LS".into(),
        ))
    }
}

/// Codec for JPEG Lossless, SV1 (Process 14.1).
/// Transfer Syntax: 1.2.840.10008.1.2.4.70
pub struct JpegLosslessSv1Codec;

impl DicomCodec for JpegLosslessSv1Codec {
    fn transfer_syntax_uid(&self) -> &'static str {
        ts_uid::JPEG_LOSSLESS_SV1
    }

    fn name(&self) -> &'static str {
        "JPEG Lossless SV1 (Process 14.1)"
    }

    fn decode(&self, dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
        jpeg_lossless_decode(dicom_bytes)
    }

    fn decode_only(&self) -> bool {
        true
    }

    fn encode(&self, _pixels: &DecodedPixels, _meta: &DicomMeta) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::EncodeFailed(
            "JPEG Lossless encode removed in v0.4.0 — use JPEG-LS".into(),
        ))
    }
}

// -- Decode (delegates to jpeg-decoder) --------------------------------

fn jpeg_lossless_decode(dicom_bytes: &[u8]) -> Result<DecodedPixels, CodecError> {
    // PS3.3 C.7.6.3.1.1: NumberOfFrames > 1 means one encapsulated value holds
    // several codestreams. `extract_jpeg_data` concatenates the item stream,
    // and `jpeg-decoder` stops at the first EOI marker - so the remaining
    // frames were dropped with no error at all. Refusing is the only honest
    // answer until this decoder walks frames.
    if number_of_frames(dicom_bytes)? > 1 {
        return Err(CodecError::DecodeFailed(
            "JPEG Lossless multi-frame decode is not supported".into(),
        ));
    }

    let jpeg_data = extract_jpeg_data(dicom_bytes)
        .ok_or_else(|| CodecError::DecodeFailed("JPEG pixel data not found".into()))?;

    // Read PhotometricInterpretation from DICOM - not all JPEG is MONOCHROME2.
    let pi = extract_pi(dicom_bytes).unwrap_or_else(|| "MONOCHROME2".to_string());

    let mut decoder = jpeg_decoder::Decoder::new(&jpeg_data[..]);
    let pixels = decoder
        .decode()
        .map_err(|e| CodecError::DecodeFailed(format!("JPEG decode: {e}")))?;

    let info = decoder
        .info()
        .ok_or_else(|| CodecError::DecodeFailed("JPEG info unavailable".into()))?;

    let rows = info.height;
    let columns = info.width;
    // PS3.3 C.7.6.3.1.1: the component count is a property of the COLOUR
    // format, not of the bit depth. Deriving it from the pixel format alone put
    // L16 in the same arm as L8, so the `bits == 16 && samples == 3` branch
    // below was unreachable: `bits` is 16 only when the format is L16, and L16
    // always yielded one sample. jpeg-decoder exposes no 16-bit colour format,
    // so a 16-bit colour codestream cannot be produced here at all - which is
    // what the code now says instead of carrying dead code that implies it can.
    let samples = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 | jpeg_decoder::PixelFormat::L16 => 1,
        _ => 3,
    };
    let bits = if info.pixel_format == jpeg_decoder::PixelFormat::L16 {
        16
    } else {
        8
    };
    debug_assert!(
        !(bits == 16 && samples == 3),
        "jpeg-decoder exposes no 16-bit colour format; this branch is unreachable"
    );

    let planar = if samples == 3 {
        let pc = rows as usize * columns as usize;
        // 8-bit only: see above. The former `if bits == 16` arm could not run.
        interleaved_to_planar(&pixels, pc)
    } else {
        pixels
    };

    Ok(DecodedPixels {
        data: planar,
        rows,
        columns,
        samples_per_pixel: samples,
        bits_allocated: bits,
        photometric_interpretation: pi,
    })
}

// -- Helpers -----------------------------------------------------------

/// Extract PhotometricInterpretation (0028,0004) from DICOM bytes.
fn extract_pi(dicom_bytes: &[u8]) -> Option<String> {
    let mut pos = if dicom_bytes.len() >= 132 && &dicom_bytes[128..132] == b"DICM" {
        132
    } else {
        0
    };
    // Skip meta header
    while pos + 4 <= dicom_bytes.len() {
        if u16_le(dicom_bytes, pos) != 0x0002 {
            break;
        }
        pos += 4;
        if pos + 6 > dicom_bytes.len() {
            return None;
        }
        let vr = dicom_bytes.get(pos..pos + 2)?;
        // PS3.5 6.2 Table 6.2-1 via `vr_widths`: the list written out here
        // omitted SV and UV, so those elements took the short branch and the
        // walk resumed inside their header.
        let ob_like = super::vr_widths::is_long_form_vr(vr);
        pos += 2;
        if ob_like {
            pos += 2;
            let len = u32_from_slice(dicom_bytes.get(pos..pos + 4)?) as usize;
            pos += 4 + len;
        } else {
            let len = u16_le(dicom_bytes, pos) as usize;
            pos += 2 + len;
        }
    }
    // Walk dataset looking for (0028,0004)
    while pos + 8 <= dicom_bytes.len() {
        let g = u16_le(dicom_bytes, pos);
        let e = u16_le(dicom_bytes, pos + 2);
        if (g, e) == (0x0028, 0x0004) {
            pos += 4;
            let vr = dicom_bytes.get(pos..pos + 2)?;
            let ob_like = super::vr_widths::is_long_form_vr(vr);
            pos += 2;
            let vlen = if ob_like {
                // PS3.5 6.2: the long form is VR(2) + reserved(2) +
                // ValueLength(4). A file truncated inside the header would
                // index past the buffer at the length read below.
                pos += 2;
                let l = u32_from_slice(dicom_bytes.get(pos..pos + 4)?) as usize;
                pos += 4;
                l
            } else {
                let l = u16_le(dicom_bytes, pos) as usize;
                pos += 2;
                l
            };
            if pos + vlen > dicom_bytes.len() {
                return None;
            }
            return Some(
                String::from_utf8_lossy(&dicom_bytes[pos..pos + vlen])
                    .trim_end_matches([' ', '\0'])
                    .to_string(),
            );
        }
        // Skip element
        pos += 4;
        if pos + 2 > dicom_bytes.len() {
            return None;
        }
        let vr = dicom_bytes.get(pos..pos + 2)?;
        let ob_like = super::vr_widths::is_long_form_vr(vr);
        pos += 2;
        if ob_like {
            if pos + 6 > dicom_bytes.len() {
                return None;
            }
            pos += 2;
            let len = u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize;
            pos += 4;
            if len == 0xFFFF_FFFF {
                // PS3.5 7.5.1: an undefined-length sequence ends at its
                // Sequence Delimitation Item, not at a declared length. Adding
                // 0xFFFFFFFF leaves the buffer, so a file whose Photometric
                // Interpretation follows such a sequence reported none at all.
                if !skip_undefined_sequence(dicom_bytes, &mut pos) {
                    return None;
                }
                continue;
            }
            pos += len;
        } else {
            let len = u16_le(dicom_bytes, pos) as usize;
            pos += 2 + len;
        }
    }
    None
}

/// Read NumberOfFrames (0028,0008).
///
/// PS3.3 C.7.6.3.1.1 makes the attribute optional with a default of 1, so an
/// absent value is one frame. A PRESENT but unparsable value is not evidence
/// of one frame either; it is reported as 0 so the caller refuses the input
/// rather than reading a multi-frame value as a single image.
fn number_of_frames(dicom_bytes: &[u8]) -> Result<u32, CodecError> {
    let Some(raw) = extract_is_string(dicom_bytes, (0x0028, 0x0008)) else {
        // PS3.3 C.7.6.3.1.4: an ABSENT NumberOfFrames means one frame.
        return Ok(1);
    };
    // A PRESENT value that will not parse, or that names zero frames, is a
    // malformed file. `unwrap_or(0)` returned 0 for both, and the `> 1`
    // multi-frame guard happily admitted it - so a file declaring "xx" was
    // decoded as a single frame with no complaint at all.
    let frames: u32 = raw.trim().parse().map_err(|_| {
        CodecError::InvalidMetadata(format!(
            "JPEG Lossless: NumberOfFrames (0028,0008) is not a number: {raw:?}"
        ))
    })?;
    if frames == 0 {
        return Err(CodecError::InvalidMetadata(
            "JPEG Lossless: NumberOfFrames (0028,0008) is present but 0".into(),
        ));
    }
    Ok(frames)
}

/// Read the text of a SHORT-form text element (IS/DS/CS/LO/SH) by tag.
fn extract_is_string(dicom_bytes: &[u8], (target_g, target_e): (u16, u16)) -> Option<String> {
    let mut pos = if dicom_bytes.len() >= 132 && &dicom_bytes[128..132] == b"DICM" {
        132
    } else {
        0
    };
    while pos + 4 <= dicom_bytes.len() && u16_le(dicom_bytes, pos) == 0x0002 {
        pos += 4;
        let vr = dicom_bytes.get(pos..pos + 2)?;
        let long = super::vr_widths::is_long_form_vr(vr);
        pos += 2;
        let len = if long {
            pos += 2;
            let l = u32_from_slice(dicom_bytes.get(pos..pos + 4)?) as usize;
            pos += 4;
            l
        } else {
            let l = u16_le(dicom_bytes, pos) as usize;
            pos += 2;
            l
        };
        pos = pos.checked_add(len)?;
    }
    while pos + 8 <= dicom_bytes.len() {
        let g = u16_le(dicom_bytes, pos);
        let e = u16_le(dicom_bytes, pos + 2);
        pos += 4;
        let vr = dicom_bytes.get(pos..pos + 2)?;
        let long = super::vr_widths::is_long_form_vr(vr);
        pos += 2;
        let len = if long {
            pos += 2;
            let l = u32_from_slice(dicom_bytes.get(pos..pos + 4)?) as usize;
            pos += 4;
            l
        } else {
            let l = u16_le(dicom_bytes, pos) as usize;
            pos += 2;
            l
        };
        if len == 0xFFFF_FFFF {
            if !skip_undefined_sequence(dicom_bytes, &mut pos) {
                return None;
            }
            continue;
        }
        let value = dicom_bytes.get(pos..pos + len)?;
        pos += len;
        if (g, e) == (target_g, target_e) {
            let text = String::from_utf8_lossy(value);
            return Some(text.trim_end_matches([' ', '\0']).to_string());
        }
    }
    None
}

fn extract_jpeg_data(dicom_bytes: &[u8]) -> Option<Vec<u8>> {
    // Walk DICOM dataset looking for (7FE0,0010) PixelData.
    // Encapsulated (undefined-length) - extract first fragment.
    let mut pos = if dicom_bytes.len() >= 132 && &dicom_bytes[128..132] == b"DICM" {
        132
    } else {
        0
    };

    // Skip meta header if present
    while pos + 4 <= dicom_bytes.len() {
        if u16_le(dicom_bytes, pos) != 0x0002 {
            break;
        }
        pos += 4;
        if pos + 6 > dicom_bytes.len() {
            return None;
        }
        let vr = dicom_bytes.get(pos..pos + 2)?;
        let ob_like = super::vr_widths::is_long_form_vr(vr);
        pos += 2;
        if ob_like {
            pos += 2;
            let len = u32_from_slice(dicom_bytes.get(pos..pos + 4)?) as usize;
            pos += 4 + len;
        } else {
            let len = u16_le(dicom_bytes, pos) as usize;
            pos += 2 + len;
        }
    }

    // Walk dataset looking for PixelData
    while pos + 4 <= dicom_bytes.len() {
        let g = u16_le(dicom_bytes, pos);
        let e = u16_le(dicom_bytes, pos + 2);
        pos += 4;

        if g == 0xFFFE {
            if pos + 4 > dicom_bytes.len() {
                return None;
            }
            let len = u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize;
            pos += 4;
            if len == 0xFFFF_FFFF {
                continue;
            }
            pos += len;
            continue;
        }

        if pos + 4 > dicom_bytes.len() {
            return None;
        }

        if (g, e) == (0x7FE0, 0x0010) {
            // Found PixelData - extract value
            let vr = dicom_bytes.get(pos..pos + 2)?;
            let ob_like = super::vr_widths::is_long_form_vr(vr);
            pos += 2;
            let len = if ob_like {
                if pos + 6 > dicom_bytes.len() {
                    return None;
                }
                pos += 2;
                u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize
            } else {
                u16_le(dicom_bytes, pos) as usize
            };
            pos += if ob_like { 4 } else { 2 };

            if len == 0xFFFF_FFFF {
                // Encapsulated: coalesce ALL non-empty fragments. A JPEG
                // codestream may be split across multiple Items (PS3.5 A.4),
                // and a decoder fed only the FIRST fragment sees a truncated
                // stream. The fragments are concatenated in item order.
                //
                // That is only correct for ONE frame. With several frames the
                // concatenation chain-runs the codestreams, the decoder stops
                // at the first EOI and the remaining frames are dropped
                // silently - so a multi-frame value is refused rather than
                // decoded as its first frame alone.
                // BOT handling (01.09 field, ATLAS 4.70 slices): the
                // Basic Offset Table, when present, is the FIRST item - an
                // empty item (len 0) or N u32 offsets (len = 4*N, first
                // offset always 0). A 1-entry BOT is disambiguated by its
                // zero prefix: a JPEG stream never starts with 00 00 00 00.
                let mut is_first_item = true;
                let mut jpeg = Vec::new();
                let mut terminated = false;
                while pos + 8 <= dicom_bytes.len() {
                    let ig = u16_le(dicom_bytes, pos);
                    let ie = u16_le(dicom_bytes, pos + 2);
                    pos += 4;
                    let ilen = u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize;
                    pos += 4;
                    if (ig, ie) == (0xFFFE, 0xE000) {
                        if is_first_item {
                            is_first_item = false;
                            let is_bot = ilen == 0
                                || (ilen.is_multiple_of(4)
                                    && pos + 4 <= dicom_bytes.len()
                                    && dicom_bytes[pos..pos + 4] == [0, 0, 0, 0]);
                            if is_bot {
                                pos = pos.saturating_add(ilen); // skip the offsets
                                continue;
                            }
                        }
                        if ilen == 0 {
                            continue;
                        }
                        if ilen == 0xFFFF_FFFF || pos + ilen > dicom_bytes.len() {
                            return None;
                        }
                        jpeg.extend_from_slice(&dicom_bytes[pos..pos + ilen]);
                        pos += ilen;
                        continue;
                    }
                    if (ig, ie) == (0xFFFE, 0xE0DD) {
                        terminated = true;
                        break;
                    }
                    pos += ilen;
                }
                // PS3.5 Table A.4-1: an encapsulated value is closed by a
                // Sequence Delimitation Item. Reaching the end of the buffer
                // without one means the value is truncated, and returning the
                // fragments in hand reported a short codestream as a whole
                // frame.
                if !terminated {
                    return None;
                }
                if jpeg.is_empty() {
                    return None;
                }
                return Some(jpeg);
            } else {
                if pos + len > dicom_bytes.len() {
                    return None;
                }
                return Some(dicom_bytes[pos..pos + len].to_vec());
            }
        }

        // Skip this element
        let vr = dicom_bytes.get(pos..pos + 2)?;
        let ob_like = super::vr_widths::is_long_form_vr(vr);
        pos += 2;
        let len = if ob_like {
            if pos + 6 > dicom_bytes.len() {
                return None;
            }
            pos += 2;
            u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize
        } else {
            u16_le(dicom_bytes, pos) as usize
        };
        pos += if ob_like { 4 } else { 2 };
        if len == 0xFFFF_FFFF {
            // Undefined-length sequence (SQ/UN): skip items up to the
            // matching Sequence Delimiter. Field fact 01.09: ZONARE US has
            // SourceImageSequence (0008,2112) before PixelData.
            if !skip_undefined_sequence(dicom_bytes, &mut pos) {
                return None;
            }
            continue;
        }
        pos += len;
    }
    None
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
                // PS3.5 6.2 Table 6.2-1: the width class follows from the VR,
                // and `vr_widths` states that table once. The hand-written
                // `matches!` here listed thirteen long-form VRs and omitted OV
                // and SV, so a 64-bit element inside the item was stepped over
                // with the short header and the rest of the item was read from
                // inside its value.
                let vr_pair = &data[*pos..*pos + 2];
                let extended = super::vr_widths::is_long_form_vr(vr_pair);
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
                    // PS3.5 6.2: the declared length already includes the pad.
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

fn interleaved_to_planar(data: &[u8], pc: usize) -> Vec<u8> {
    let mut planar = vec![0u8; pc * 3];
    let (r, rest) = planar.split_at_mut(pc);
    let (g, b) = rest.split_at_mut(pc);
    for i in 0..pc.min(data.len() / 3) {
        r[i] = data[i * 3];
        g[i] = data[i * 3 + 1];
        b[i] = data[i * 3 + 2];
    }
    planar
}

// -- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pixeldata(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OB");
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        for item in items {
            out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
            out.extend_from_slice(&(item.len() as u32).to_le_bytes());
            out.extend_from_slice(item);
        }
        out.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    #[test]
    fn extract_skips_filled_one_entry_bot() {
        // 01.09 field (ATLAS 4.70 slice): the Basic Offset Table is a
        // non-empty FIRST item (len=4, one u32 offset = 0). It must be
        // skipped - a JPEG stream never starts with 00 00 00 00.
        let frag = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let data = build_pixeldata(&[vec![0, 0, 0, 0], frag.clone()]);
        assert_eq!(extract_jpeg_data(&data).unwrap(), frag);
    }

    #[test]
    fn extract_skips_empty_bot() {
        let frag = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let data = build_pixeldata(&[vec![], frag.clone()]);
        assert_eq!(extract_jpeg_data(&data).unwrap(), frag);
    }

    #[test]
    fn extract_fragment_first_without_bot() {
        let frag = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let data = build_pixeldata(std::slice::from_ref(&frag));
        assert_eq!(extract_jpeg_data(&data).unwrap(), frag);
    }

    /// A codestream split across multiple fragments (PS3.5 A.4) must be
    /// coalesced in item order - returning only the first fragment fed the
    /// decoder a truncated stream.
    #[test]
    fn extract_coalesces_multiple_fragments() {
        let frag1 = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let frag2 = vec![0xFF, 0xDA, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02];
        let data = build_pixeldata(&[frag1.clone(), frag2.clone()]);
        let mut expected = frag1.clone();
        expected.extend_from_slice(&frag2);
        assert_eq!(extract_jpeg_data(&data).unwrap(), expected);
    }

    fn push_is(out: &mut Vec<u8>, tag: (u16, u16), text: &str) {
        out.extend_from_slice(&tag.0.to_le_bytes());
        out.extend_from_slice(&tag.1.to_le_bytes());
        out.extend_from_slice(b"IS");
        let len = if text.len().is_multiple_of(2) {
            text.len()
        } else {
            text.len() + 1
        };
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        if !text.len().is_multiple_of(2) {
            out.push(b' ');
        }
    }

    /// PS3.3 C.7.6.3.1.1 lets NumberOfFrames default to 1 when absent, but a
    /// value of 3 is a statement about the file. Concatenating the item
    /// stream hands the decoder three codestreams; it stops at the first EOI
    /// and the other two are lost with no error.
    #[test]
    fn a_multiframe_value_is_refused_not_decoded_as_one_frame() {
        let frag = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 128]);
        data.extend_from_slice(b"DICM");
        push_is(&mut data, (0x0028, 0x0002), "1 ");
        push_is(&mut data, (0x0028, 0x0008), "3");
        data.extend_from_slice(&build_pixeldata(&[frag])[132..]);

        assert_eq!(number_of_frames(&data).expect("three parses"), 3);
        let err = JpegLosslessCodec
            .decode(&data)
            .expect_err("three frames are not one image");
        let CodecError::DecodeFailed(msg) = err else {
            panic!("expected DecodeFailed");
        };
        assert!(msg.contains("multi-frame"), "{msg}");
    }

    /// An absent NumberOfFrames means one frame; a present but unparsable one
    /// does not - it is refused rather than guessed as a single image.
    ///
    /// `unwrap_or(0)` used to answer 0 for both "xx" and "0", and the `> 1`
    /// multi-frame guard admitted 0 - so a file whose own attribute is garbage
    /// decoded as a single frame with no complaint at all.
    #[test]
    fn an_unparsable_frame_count_is_not_read_as_one() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 128]);
        data.extend_from_slice(b"DICM");
        push_is(&mut data, (0x0028, 0x0008), "xx");
        let err = number_of_frames(&data).expect_err("not a number is refused");
        assert!(err.to_string().contains("not a number"), "got: {err}");

        // A present "0" names no frames and is refused for its own reason.
        let mut zero = Vec::new();
        zero.extend_from_slice(&[0u8; 128]);
        zero.extend_from_slice(b"DICM");
        push_is(&mut zero, (0x0028, 0x0008), "0 ");
        let err = number_of_frames(&zero).expect_err("zero frames is malformed");
        assert!(err.to_string().contains("present but 0"), "got: {err}");

        let mut absent = Vec::new();
        absent.extend_from_slice(&[0u8; 128]);
        absent.extend_from_slice(b"DICM");
        assert_eq!(
            number_of_frames(&absent).expect("absent defaults"),
            1,
            "the PS3.3 default"
        );
    }

    /// PS3.5 7.5.1: an undefined-length sequence ends at its Sequence
    /// Delimitation Item. Adding the 0xFFFFFFFF sentinel as a length leaves
    /// the buffer, so a Photometric Interpretation placed AFTER such a
    /// sequence was reported as absent.
    #[test]
    fn a_photometric_interpretation_after_an_undefined_sequence_is_found() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 128]);
        data.extend_from_slice(b"DICM");
        // (0008,2112) SQ, undefined length, one defined-length item.
        data.extend_from_slice(&0x0008u16.to_le_bytes());
        data.extend_from_slice(&0x2112u16.to_le_bytes());
        data.extend_from_slice(b"SQ");
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        data.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        data.extend_from_slice(&0u32.to_le_bytes());
        // (0028,0004) CS
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0004u16.to_le_bytes());
        data.extend_from_slice(b"CS");
        data.extend_from_slice(&8u16.to_le_bytes());
        data.extend_from_slice(b"RGB     ");

        assert_eq!(extract_pi(&data).as_deref(), Some("RGB"));
    }

    /// PS3.5 6.2 Table 6.2-1: UV is long-form. A walker that reads it as
    /// short-form resumes four bytes inside the header.
    #[test]
    fn the_pi_walker_treats_uv_as_long_form() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 128]);
        data.extend_from_slice(b"DICM");
        data.extend_from_slice(&0x0018u16.to_le_bytes());
        data.extend_from_slice(&0x1234u16.to_le_bytes());
        data.extend_from_slice(b"UV");
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&[0x5A; 16]);
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0004u16.to_le_bytes());
        data.extend_from_slice(b"CS");
        data.extend_from_slice(&8u16.to_le_bytes());
        data.extend_from_slice(b"YBR_FUL ");

        assert_eq!(extract_pi(&data).as_deref(), Some("YBR_FUL"));
    }

    /// A header truncated inside the extended length field is a missing
    /// value, not an out-of-bounds index.
    #[test]
    fn a_truncated_extended_header_is_not_indexed() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 128]);
        data.extend_from_slice(b"DICM");
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0004u16.to_le_bytes());
        data.extend_from_slice(b"UT");
        data.extend_from_slice(&[0, 0]); // reserved, then nothing
        assert_eq!(extract_pi(&data), None);
    }
}
