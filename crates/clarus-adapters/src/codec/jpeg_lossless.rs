// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! JPEG Lossless encoder (Process 14 / SOF3) - pure Rust, no C dependencies.
//!
//! Transfer Syntaxes:
//! - 1.2.840.10008.1.2.4.57 (Process 14, Non-Hierarchical)
//! - 1.2.840.10008.1.2.4.70 (Process 14.1, Selection Value 1)
//!
//! Decode delegates to `jpeg-decoder` (SOF3-aware).
//! Encode is hand-rolled: predictor 7 -> differential -> Huffman.
//!
//! # DICOM PS3.5 Section A.4
//! JPEG Lossless uses predictors 1-7. We use predictor 7
//! ((A+B)/2) which works well for medical images.
//!
//! # References
//! - ISO/IEC 10918-1 Section H - Lossless JPEG
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
    let samples = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 | jpeg_decoder::PixelFormat::L16 => 1,
        _ => 3,
    };
    let bits = if info.pixel_format == jpeg_decoder::PixelFormat::L16 {
        16
    } else {
        8
    };

    let planar = if samples == 3 {
        let pc = rows as usize * columns as usize;
        if bits == 16 {
            interleaved16_to_planar(&pixels, pc)
        } else {
            interleaved_to_planar(&pixels, pc)
        }
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
        let vr = std::str::from_utf8(&dicom_bytes[pos..pos + 2]).ok()?;
        pos += 2;
        let ob_like = matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        if ob_like {
            pos += 2;
            let len = u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize;
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
            let vr = std::str::from_utf8(&dicom_bytes[pos..pos + 2]).ok()?;
            pos += 2;
            let ob_like = matches!(
                vr,
                "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            );
            let vlen = if ob_like {
                pos += 2;
                let l = u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize;
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
        let vr = std::str::from_utf8(&dicom_bytes[pos..pos + 2]).ok()?;
        pos += 2;
        let ob_like = matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        if ob_like {
            if pos + 6 > dicom_bytes.len() {
                return None;
            }
            pos += 2;
            let len = u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize;
            pos += 4 + len;
        } else {
            let len = u16_le(dicom_bytes, pos) as usize;
            pos += 2 + len;
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
        let vr = std::str::from_utf8(&dicom_bytes[pos..pos + 2]).ok()?;
        pos += 2;
        let ob_like = matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        if ob_like {
            pos += 2;
            let len = u32_from_slice(&dicom_bytes[pos..pos + 4]) as usize;
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
            let vr = std::str::from_utf8(&dicom_bytes[pos..pos + 2]).ok()?;
            pos += 2;
            let ob_like = matches!(
                vr,
                "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            );
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
                // Encapsulated: extract first non-empty fragment.
                // BOT handling (01.09 field, ATLAS 4.70 slices): the
                // Basic Offset Table, when present, is the FIRST item - an
                // empty item (len 0) or N u32 offsets (len = 4*N, first
                // offset always 0). A 1-entry BOT is disambiguated by its
                // zero prefix: a JPEG stream never starts with 00 00 00 00.
                let mut is_first_item = true;
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
                        return Some(dicom_bytes[pos..pos + ilen].to_vec());
                    }
                    if (ig, ie) == (0xFFFE, 0xE0DD) {
                        break;
                    }
                    pos += ilen;
                }
                return None;
            } else {
                if pos + len > dicom_bytes.len() {
                    return None;
                }
                return Some(dicom_bytes[pos..pos + len].to_vec());
            }
        }

        // Skip this element
        let vr = std::str::from_utf8(&dicom_bytes[pos..pos + 2]).ok()?;
        pos += 2;
        let ob_like = matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
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

fn interleaved16_to_planar(data: &[u8], pc: usize) -> Vec<u8> {
    let mut planar = vec![0u8; pc * 6];
    let (r, rest) = planar.split_at_mut(pc * 2);
    let (g, b) = rest.split_at_mut(pc * 2);
    for i in 0..pc.min(data.len() / 6) {
        let s = i * 6;
        r[i * 2] = data[s];
        r[i * 2 + 1] = data[s + 1];
        g[i * 2] = data[s + 2];
        g[i * 2 + 1] = data[s + 3];
        b[i * 2] = data[s + 4];
        b[i * 2 + 1] = data[s + 5];
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
}
